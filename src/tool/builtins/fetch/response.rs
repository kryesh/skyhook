//! Bounded response streaming, atomic downloads, and response decoding.
use std::{collections::BTreeMap, path::PathBuf};

use crate::blocking;
use crate::fs::{CommitMode, PermissionPolicy};
use crate::tool::diagnostic::{Operation, Subject};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use reqwest::{Method, header::HeaderMap};
use tokio::io::AsyncWriteExt;

use super::super::workspace::atomic_write_context;
use super::progress::{FetchProgress, transport_error};
use super::validation::{HttpRequestUrl, InlineMode, OutputPlan};
use super::{LocalContext, LocalError, ResponseBody, text};

enum BodySink {
    Memory { bytes: Vec<u8>, mode: InlineMode },
    Download(PendingDownload),
}

/// Owns the entire uncommitted download. Dropping it removes the staging file.
/// Declare the writer before the staging file so ordinary drop closes it first.
struct PendingDownload {
    writer: tokio::fs::File,
    staged: crate::fs::StagedFile,
    destination: PathBuf,
    commit: CommitMode,
}

impl PendingDownload {
    async fn new(
        destination: PathBuf,
        overwrite: bool,
        progress: &mut FetchProgress,
    ) -> Result<Self, LocalError> {
        progress.download_io(Operation::CreateCapture, &destination);
        let staging = destination.clone();
        let staged = blocking(move || {
            crate::fs::StagedFile::create(&staging, PermissionPolicy::Inherit)
                .map_err(|error| LocalError::io(error.into()))
        })
        .await?;
        let writer = tokio::fs::File::from_std(staged.as_file().try_clone()?);
        Ok(Self {
            writer,
            staged,
            destination,
            commit: if overwrite {
                CommitMode::Replace
            } else {
                CommitMode::NoClobber
            },
        })
    }

    /// Consuming commit: flush and sync before closing the writer and publishing.
    /// Cancellation before publication preserves the destination. Once the commit
    /// starts it runs to the end, so cancellation cannot undo a rename.
    async fn finish(
        mut self,
        context: &LocalContext,
        progress: &mut FetchProgress,
        bytes: u64,
    ) -> Result<ResponseBody, LocalError> {
        let path = self.destination.to_string_lossy().into_owned();
        // Keep the owner intact across every await: cancellation must drop its
        // writer field before its staging-file field, not reverse-order locals.
        progress.download_io(Operation::FinishCapture, self.staged.path());
        self.writer.flush().await?;
        progress.download_io(Operation::SyncFile, self.staged.path());
        self.writer.sync_all().await?;
        if context.is_cancelled() {
            return Err(LocalError::cancelled());
        }
        // Close the writer before publication.
        let Self {
            writer,
            staged,
            destination,
            commit,
        } = self;
        drop(writer);
        // A cancellation or the timeout from here on may leave the rename to finish, so
        // what it did to the destination is unknown.
        let renaming = crate::fs::AtomicWriteStage::Commit;
        progress.operation(atomic_write_context(&destination, renaming));
        let committed = blocking(move || Ok::<_, LocalError>(staged.commit(commit))).await?;
        committed.map_err(|error| {
            progress.operation(atomic_write_context(&destination, error.stage));
            LocalError::io(error.source)
        })?;
        Ok(ResponseBody::File { path, bytes })
    }
}

impl BodySink {
    async fn new(output: OutputPlan, progress: &mut FetchProgress) -> Result<Self, LocalError> {
        match output {
            OutputPlan::Inline(mode) => Ok(Self::Memory {
                bytes: Vec::new(),
                mode,
            }),
            OutputPlan::Download {
                destination,
                overwrite,
            } => (PendingDownload::new(destination.into(), overwrite, progress).await)
                .map(Self::Download),
        }
    }

    async fn write(
        &mut self,
        chunk: &[u8],
        progress: &mut FetchProgress,
    ) -> Result<(), LocalError> {
        match self {
            Self::Memory { bytes, .. } => {
                bytes
                    .try_reserve(chunk.len())
                    .map_err(|_| LocalError::failed("the response does not fit in memory"))?;
                bytes.extend_from_slice(chunk);
            }
            Self::Download(download) => {
                progress.download_io(Operation::WriteCapture, download.staged.path());
                download.writer.write_all(chunk).await?;
            }
        }
        Ok(())
    }

    async fn finish(
        self,
        context: &LocalContext,
        progress: &mut FetchProgress,
        content_type: Option<&str>,
        url: &HttpRequestUrl,
        received: u64,
    ) -> Result<ResponseBody, LocalError> {
        match self {
            Self::Memory { bytes, mode } => {
                match mode {
                    InlineMode::ExtractText => progress.step(
                        Operation::Capture,
                        Subject::Label("HTML text extraction".into()),
                    ),
                    _ => progress.step(
                        Operation::Deserialize,
                        Subject::Label("HTTP response body".into()),
                    ),
                }
                response_body(bytes, content_type, url, mode).await
            }
            Self::Download(download) => download.finish(context, progress, received).await,
        }
    }
}

fn exceeds_max_bytes() -> LocalError {
    LocalError::failed("response exceeds max_bytes")
}

/// The body and its decoded entity byte count.
pub(super) async fn read_body(
    context: &LocalContext,
    progress: &mut FetchProgress,
    response: reqwest::Response,
    output: OutputPlan,
    max_bytes: u64,
    url: &HttpRequestUrl,
    method: &Method,
) -> Result<(ResponseBody, u64), LocalError> {
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    progress.step(
        Operation::Receive,
        Subject::Label("HTTP response body".into()),
    );
    if *method != Method::HEAD
        && response
            .content_length()
            .is_some_and(|length| length > max_bytes)
    {
        return Err(exceeds_max_bytes());
    }
    let mut sink = BodySink::new(output, progress).await?;
    let mut stream = response.bytes_stream();
    let mut received = 0;
    loop {
        if let BodySink::Download(download) = &sink {
            progress.download_io(Operation::Receive, &download.destination);
        }
        let Some(chunk) = stream.next().await else {
            break;
        };
        let chunk = chunk.map_err(transport_error)?;
        received += chunk.len() as u64;
        if received > max_bytes {
            return Err(exceeds_max_bytes());
        }
        sink.write(&chunk, progress).await?;
    }
    let body = sink
        .finish(context, progress, content_type.as_deref(), url, received)
        .await?;
    Ok((body, received))
}

pub(super) fn collect_headers(headers: &HeaderMap) -> BTreeMap<String, Vec<String>> {
    let mut result: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in headers {
        result
            .entry(name.to_string())
            .or_default()
            .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
    }
    result
}
async fn response_body(
    bytes: Vec<u8>,
    content_type: Option<&str>,
    url: &HttpRequestUrl,
    mode: InlineMode,
) -> Result<ResponseBody, LocalError> {
    use text::{ContentClass, ExtractableHtml, ResponseEntity};

    if bytes.is_empty() {
        return Ok(ResponseBody::Empty);
    }
    if matches!(mode, InlineMode::Base64) {
        return Ok(ResponseBody::Base64 {
            data: STANDARD.encode(bytes),
        });
    }
    let entity = ResponseEntity::new(bytes, content_type);
    if matches!(mode, InlineMode::ExtractText) {
        match entity.class() {
            ContentClass::Html => {
                let input = ExtractableHtml::admit(entity, url).map_err(LocalError::failed)?;
                let extracted = text::extract(input).await.map_err(LocalError::failed)?;
                return Ok(ResponseBody::Text {
                    text: extracted.text,
                    metadata: Some(extracted.metadata),
                });
            }
            ContentClass::Binary => {
                return Err(LocalError::failed(
                    "text extraction is unsupported for this binary content type",
                ));
            }
            ContentClass::Text => {}
        }
    }
    if matches!(mode, InlineMode::Auto) && entity.class() == ContentClass::Binary {
        return Ok(ResponseBody::Base64 {
            data: STANDARD.encode(entity.bytes()),
        });
    }
    let text = entity.decode();
    // Only automatic handling reads JSON; `text` keeps the decoded text as sent.
    if !matches!(mode, InlineMode::Auto) {
        return Ok(ResponseBody::Text {
            text,
            metadata: None,
        });
    }
    // A body can be large, so it is parsed off the async workers.
    let parsed = tokio::task::spawn_blocking(move || {
        let json = crate::job::output::parse_json_text(&text);
        (text, json)
    })
    .await;
    let (text, json) = parsed.unwrap_or_else(|error| std::panic::resume_unwind(error.into_panic()));
    Ok(match json {
        Some(value) => ResponseBody::Json { value },
        None => ResponseBody::Text {
            text,
            metadata: None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{executor, fetch, server};
    use super::*;
    use crate::provider::http::transport::tests::{Plan, Server, read_request, reply};
    use crate::tool::diagnostic::{Cause, Effects, IoKind, RequestEffects};
    use serde_json::json;

    #[tokio::test]
    async fn extraction_failure_names_its_reason() {
        let runtime = crate::tests::TestRuntime::new().await;
        let executor = executor(&runtime);
        let html = "<html><body></body></html>";
        let (url, task) = server(vec![reply(
            "201 Created",
            "Content-Type: text/html\r\n",
            html,
        )])
        .await;
        let error = fetch(&runtime, &executor, json!({"url":url, "text":true}))
            .await
            .unwrap_err();
        task.finish().await;
        let (diagnostic, output) = error.into_parts();
        assert!(output.is_none());
        assert_eq!(diagnostic.context.operation, Operation::Capture);
        assert_eq!(
            diagnostic.cause,
            Cause::Message(text::ExtractionFailure::Parser.to_string())
        );
    }

    #[tokio::test]
    async fn download_staging_failure_identifies_destination_and_unchanged_effects() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("missing").join("download");
        let mut progress = FetchProgress::new();
        let error = PendingDownload::new(destination.clone(), false, &mut progress)
            .await
            .err()
            .unwrap();
        let diagnostic = progress.failure(error).diagnostic();
        assert_eq!(diagnostic.context.operation, Operation::CreateCapture);
        assert_eq!(diagnostic.context.subject, Subject::path(&destination));
        assert_eq!(diagnostic.context.effects, Effects::Unchanged);
        assert!(matches!(
            diagnostic.cause,
            Cause::Io {
                kind: IoKind::NotFound,
                ..
            }
        ));
        assert!(!destination.exists());
    }

    #[tokio::test]
    async fn download_publication_failure_keeps_response_and_existing_destination() {
        let runtime = crate::tests::TestRuntime::new().await;
        let destination = runtime.root.path().join("raced-download");
        let raced = destination.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            // The observed request proves destination preflight has finished;
            // publication must still reject a newly created destination.
            tokio::fs::write(raced, b"keep original").await.unwrap();
            socket
                .write_all(reply("200 OK", "", "replacement").as_bytes())
                .await
                .unwrap();
        });
        let executor = executor(&runtime);
        let error = crate::tests::bounded(fetch(
            &runtime,
            &executor,
            json!({"url":url,"save_to":"raced-download"}),
        ))
        .await
        .unwrap_err();
        crate::tests::bounded(task).await.unwrap();
        let context = error.diagnostic().context;
        assert_eq!(context.operation, Operation::Rename);
        assert_eq!(context.subject, Subject::path(&destination));
        assert_eq!(context.request, RequestEffects::MayHaveExecuted);
        assert_eq!(
            tokio::fs::read(destination).await.unwrap(),
            b"keep original"
        );
    }

    #[tokio::test]
    async fn download_atomicity_limits_truncation_and_head() {
        let runtime = crate::tests::TestRuntime::new().await;
        let destination = runtime.root.path().join("out");
        tokio::fs::write(&destination, b"original").await.unwrap();
        let executor = executor(&runtime);
        let oversized = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\n1234\r\n4\r\n5678\r\n0\r\n\r\n";
        // A truncated response fails without retries.
        let truncated = "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 100\r\n\r\nshort";
        for (raw, arguments) in [
            (
                oversized,
                json!({"save_to":"out","overwrite":true,"max_bytes":5}),
            ),
            (truncated, json!({})),
        ] {
            let (url, task) = server(vec![raw.to_owned()]).await;
            let mut arguments = arguments;
            arguments["url"] = json!(url);
            assert!(fetch(&runtime, &executor, arguments).await.is_err());
            assert_eq!(task.finish().await.len(), 1);
            assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"original");
        }
        let (url, task) = server(vec![reply("200 OK", "", "replacement")]).await;
        let arguments = json!({"url":url,"save_to":"out","overwrite":true});
        let result = fetch(&runtime, &executor, arguments).await.unwrap();
        task.finish().await;
        assert_eq!(
            (&result["body"]["kind"], &result["body"]["bytes"]),
            (&json!("file"), &json!(11))
        );
        assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"replacement");
        let unreachable = json!({"url":"http://127.0.0.1:1","save_to":"out"});
        assert!(fetch(&runtime, &executor, unreachable).await.is_err());
        let head = "HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\nConnection: close\r\n\r\n";
        let (url, task) = server(vec![head.to_owned()]).await;
        let arguments = json!({"url":url,"method":"HEAD","max_bytes":1});
        assert_eq!(
            fetch(&runtime, &executor, arguments).await.unwrap()["size"],
            0
        );
        task.finish().await;
    }

    #[tokio::test]
    async fn response_formats_decode_extract_or_reject_by_mode() {
        let url = HttpRequestUrl::parse("https://example.org/article").unwrap();
        let html = format!(
            "<!doctype html><html><title>Sniffed article</title><article><p>{}</p></article></html>",
            "Meaningful article details, independent of media type spelling. ".repeat(30)
        );
        let body =
            |bytes: Vec<u8>, content_type, mode| response_body(bytes, content_type, &url, mode);
        assert!(matches!(
            body(vec![0, 255, 3], Some("application/octet-stream"), InlineMode::Auto).await,
            Ok(ResponseBody::Base64 { data }) if data == "AP8D"
        ));
        assert!(matches!(
            body(vec![0xe9], Some("text/plain; charset=windows-1252"), InlineMode::Auto).await,
            Ok(ResponseBody::Text { text, metadata: None }) if text == "\u{e9}"
        ));
        assert!(matches!(
            body(Vec::new(), Some("text/html"), InlineMode::Text).await,
            Ok(ResponseBody::Empty)
        ));
        // Sniffed HTML extracts even without a declared media type.
        let Ok(ResponseBody::Text {
            text,
            metadata: Some(metadata),
        }) = body(html.into_bytes(), None, InlineMode::ExtractText).await
        else {
            panic!("expected extracted text")
        };
        assert!(text.contains("Meaningful article details"));
        assert_eq!(metadata.title, "Sniffed article");
        let error = body(
            vec![0, 255],
            Some("application/pdf"),
            InlineMode::ExtractText,
        )
        .await;
        assert!(error.is_err());
    }

    #[tokio::test]
    async fn decompressed_bytes_are_counted_and_limited() {
        // Gzip of 4096 ASCII A bytes, with deterministic zero mtime.
        let compressed = STANDARD
            .decode("H4sIAAAAAAAC/+3BAQ0AAADCoGzvX8oeDigAAADg3QBANKb+ABAAAA==")
            .unwrap();
        let mut response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", compressed.len()).into_bytes();
        response.extend_from_slice(&compressed);
        let runtime = crate::tests::TestRuntime::new().await;
        let executor = executor(&runtime);
        for max_bytes in [4096, 100] {
            let (url, task) = server(vec![response.clone()]).await;
            let result = fetch(
                &runtime,
                &executor,
                json!({"url":url,"max_bytes":max_bytes}),
            )
            .await;
            if max_bytes == 4096 {
                let output = result.unwrap();
                assert_eq!(output["size"], 4096);
                assert_eq!(output["body"]["text"], "A".repeat(4096));
            } else {
                assert!(result.is_err());
            }
            task.finish().await;
        }
    }

    #[tokio::test]
    async fn cancellation_preserves_destination_and_cleans_temporary_download() {
        let runtime = crate::tests::TestRuntime::new().await;
        let destination = runtime.root.path().join("saved.txt");
        tokio::fs::write(&destination, b"original").await.unwrap();
        let head = b"HTTP/1.1 200 OK\r\nContent-Length: 10000\r\n\r\npartial";
        let mut server = Server::start(vec![Plan::stalled(head)]).await;
        let url = server.root();
        let executor = executor(&runtime);
        let agent = runtime.agent.clone();
        let arguments = json!({"url":url,"save_to":"saved.txt","overwrite":true});
        let pending =
            tokio::spawn(async move { executor.run_host(&agent, "fetch", arguments).await });
        server.request().await;
        assert_eq!(
            runtime
                .jobs
                .cancel_all(&runtime.agent, crate::job::CancelScope::Outcome)
                .await,
            1
        );
        let result = crate::tests::bounded(pending).await;
        assert!(result.unwrap().is_err());
        drop(server);
        assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"original");
        let entries: Vec<_> = std::fs::read_dir(runtime.root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            entries.len(),
            2,
            "only sessions and original destination should remain: {entries:?}"
        );
    }
}
