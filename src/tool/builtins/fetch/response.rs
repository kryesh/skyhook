//! Bounded response streaming, atomic downloads, and response decoding.
use std::{collections::BTreeMap, path::PathBuf};

use crate::tool::diagnostic::{Effects, Operation, PartialContext, Subject};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use reqwest::{Method, header::HeaderMap};
use tokio::io::AsyncWriteExt;

use super::diagnostics::{
    DiagnosticMessage, FetchDiagnostic, FetchError, FetchErrorKind, FetchPhase,
};
use super::progress::FetchProgress;
use super::validation::{HttpRequestUrl, InlineMode, OutputPlan};
use super::{LocalContext, LocalError, ResponseBody, fetch_text, invalid};

enum BodySink {
    Memory {
        bytes: Vec<u8>,
        mode: InlineMode,
        limit: u64,
    },
    Download(PendingDownload),
}

/// Owns the entire uncommitted download. Dropping it removes the temporary path.
/// Declare the writer before the temporary file so ordinary drop closes it first.
struct PendingDownload {
    writer: tokio::fs::File,
    temp: tempfile::NamedTempFile,
    destination: PathBuf,
    overwrite: bool,
}

impl PendingDownload {
    fn new(
        destination: PathBuf,
        overwrite: bool,
        progress: &mut FetchProgress,
    ) -> Result<Self, FetchError> {
        progress.download_io(Operation::CreateCapture, &destination);
        let temp = tempfile::NamedTempFile::new_in(destination.parent().ok_or_else(|| {
            FetchError::from_tool_error(invalid("save_to has no parent"), FetchPhase::LocalIo)
        })?)
        .map_err(local_io)?;
        let writer = tokio::fs::File::from_std(temp.as_file().try_clone().map_err(local_io)?);
        Ok(Self {
            writer,
            temp,
            destination,
            overwrite,
        })
    }

    /// Consuming commit: flush and sync before closing the writer and publishing.
    /// Cancellation before publication preserves the destination. Persist is
    /// synchronous: cancellation cannot undo a rename that has already completed.
    async fn finish(
        mut self,
        context: &LocalContext,
        progress: &mut FetchProgress,
    ) -> Result<ResponseBody, FetchError> {
        let path = self.destination.to_string_lossy().into_owned();
        // Keep the owner intact across every await: cancellation must drop its
        // writer field before its temporary-file field, not reverse-order locals.
        progress.download_io(Operation::FinishCapture, self.temp.path());
        self.writer.flush().await.map_err(local_io)?;
        progress.download_io(Operation::SyncFile, self.temp.path());
        self.writer.sync_all().await.map_err(local_io)?;
        if context.is_cancelled() {
            return Err(FetchError::from_tool_error(
                LocalError::cancelled(),
                FetchPhase::LocalIo,
            ));
        }
        // No await follows this split. Close the writer before publication.
        let Self {
            writer,
            temp,
            destination,
            overwrite,
        } = self;
        drop(writer);
        progress.local_io(Operation::Rename, Subject::path(&destination));
        if overwrite {
            temp.persist(&destination).map_err(|e| local_io(e.error))?;
        } else {
            temp.persist_noclobber(&destination)
                .map_err(|e| local_io(e.error))?;
        }
        Ok(ResponseBody::File {
            path,
            bytes: progress.received_bytes,
        })
    }
}

impl BodySink {
    fn new(
        output: OutputPlan,
        limit: u64,
        progress: &mut FetchProgress,
    ) -> Result<Self, FetchError> {
        match output {
            OutputPlan::Inline(mode) => Ok(Self::Memory {
                bytes: Vec::new(),
                mode,
                limit,
            }),
            OutputPlan::Download {
                destination,
                overwrite,
            } => PendingDownload::new(destination, overwrite, progress).map(Self::Download),
        }
    }

    async fn write(
        &mut self,
        chunk: &[u8],
        progress: &mut FetchProgress,
    ) -> Result<(), FetchError> {
        match self {
            Self::Memory { bytes, limit, .. } => {
                reserve_bounded(bytes, chunk.len(), *limit)?;
                bytes.extend_from_slice(chunk);
            }
            Self::Download(download) => {
                progress.download_io(Operation::WriteCapture, download.temp.path());
                download.writer.write_all(chunk).await.map_err(local_io)?;
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
    ) -> Result<ResponseBody, FetchError> {
        match self {
            Self::Memory { bytes, mode, .. } => {
                progress.phase(match mode {
                    InlineMode::ExtractText => FetchPhase::Extraction,
                    _ => FetchPhase::Decode,
                });
                response_body(bytes, content_type, url, mode).await
            }
            Self::Download(download) => download.finish(context, progress).await,
        }
    }
}

/// The diagnostic counter is the sole accounting owner. Publish the rejecting
/// chunk's decoded size before rejecting it; callers must not write on failure.
fn observe_chunk(
    progress: &mut FetchProgress,
    chunk_len: usize,
    limit: u64,
) -> Result<(), FetchError> {
    progress.received_bytes += chunk_len as u64;
    if progress.received_bytes > limit {
        return Err(response_exceeds_max_bytes());
    }
    Ok(())
}

/// Keep geometric growth but cap requested vector capacity at the admitted byte
/// limit. This bounds this buffer's request, not allocator rounding or total heap
/// (decoded strings, base64, decompression and extraction have separate costs).
fn reserve_bounded(bytes: &mut Vec<u8>, additional: usize, limit: u64) -> Result<(), FetchError> {
    let required = bytes
        .len()
        .checked_add(additional)
        .ok_or_else(response_too_large)?;
    let limit = usize::try_from(limit).map_err(|_| response_too_large())?;
    if required > limit {
        return Err(response_exceeds_max_bytes());
    }
    if required > bytes.capacity() {
        let geometric = bytes.capacity().checked_mul(2).unwrap_or(limit);
        let capacity = required.max(geometric).max(8).min(limit);
        bytes
            .try_reserve_exact(capacity - bytes.len())
            .map_err(|_| response_too_large())?;
    }
    Ok(())
}

fn response_too_large() -> FetchError {
    FetchDiagnostic::new(
        FetchPhase::ResponseBody,
        FetchErrorKind::ResponseBodyFailure,
    )
    .into()
}

fn response_exceeds_max_bytes() -> FetchError {
    FetchDiagnostic::classified(
        FetchPhase::ResponseBody,
        FetchErrorKind::SizeLimit,
        DiagnosticMessage::ResponseExceedsMaxBytes,
    )
    .into()
}

fn extraction_failure(message: DiagnosticMessage) -> FetchError {
    FetchDiagnostic::classified(
        FetchPhase::Extraction,
        FetchErrorKind::ExtractionFailure,
        message,
    )
    .into()
}

fn local_io(error: std::io::Error) -> FetchError {
    FetchDiagnostic::from_io(&error, FetchPhase::LocalIo).into()
}

pub(super) async fn read_body(
    context: &LocalContext,
    progress: &mut FetchProgress,
    response: reqwest::Response,
    output: OutputPlan,
    max_bytes: u64,
    url: &HttpRequestUrl,
    method: &Method,
) -> Result<ResponseBody, FetchError> {
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    progress.phase(FetchPhase::ResponseBody);
    // Content-Length is decoded length when known; the streamed count remains authoritative.
    if *method != Method::HEAD
        && response
            .content_length()
            .is_some_and(|length| length > max_bytes)
    {
        return Err(response_exceeds_max_bytes());
    }
    let mut sink = BodySink::new(output, max_bytes, progress)?;
    let mut stream = response.bytes_stream();
    loop {
        match &sink {
            BodySink::Memory { .. } => progress.phase(FetchPhase::ResponseBody),
            BodySink::Download(download) => progress.operation(
                FetchPhase::ResponseBody,
                PartialContext::new(Operation::Receive, Subject::path(&download.destination))
                    .effects(Effects::Unchanged),
            ),
        }
        let Some(chunk) = stream.next().await else {
            break;
        };
        let chunk = chunk
            .map_err(|error| FetchDiagnostic::from_reqwest(&error, FetchPhase::ResponseBody))?;
        observe_chunk(progress, chunk.len(), max_bytes)?;
        sink.write(&chunk, progress).await?;
    }
    sink.finish(context, progress, content_type.as_deref(), url)
        .await
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
) -> Result<ResponseBody, FetchError> {
    use fetch_text::{ContentClass, ExtractableHtml, ResponseEntity};

    if bytes.is_empty() {
        return Ok(ResponseBody::Empty);
    }
    // Explicit Base64 requires no media parsing, sniffing or decoding.
    if matches!(mode, InlineMode::Base64) {
        return Ok(ResponseBody::Base64 {
            data: STANDARD.encode(bytes),
        });
    }
    let entity = ResponseEntity::new(bytes, content_type);
    if matches!(mode, InlineMode::ExtractText) {
        match entity.class() {
            ContentClass::Html => {
                let extraction_error =
                    |reason| extraction_failure(DiagnosticMessage::Extraction(reason));
                let input = ExtractableHtml::admit(entity, url).map_err(extraction_error)?;
                let extracted = fetch_text::extract(input).await.map_err(extraction_error)?;
                return Ok(ResponseBody::Text {
                    text: extracted.text,
                    metadata: Some(extracted.metadata),
                });
            }
            ContentClass::Binary => {
                return Err(extraction_failure(
                    DiagnosticMessage::TextExtractionUnsupported,
                ));
            }
            ContentClass::Text => {}
        }
    }
    if matches!(mode, InlineMode::Auto) && entity.class() == ContentClass::Binary {
        Ok(ResponseBody::Base64 {
            data: STANDARD.encode(entity.bytes()),
        })
    } else {
        Ok(ResponseBody::Text {
            text: entity.decode(),
            metadata: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        executor, fetch, progress_for, read_request, response, server, stalled_server,
    };
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn accounting_fixture(limit: u64) -> (FetchProgress, u64) {
        let (plan, progress) = progress_for(json!({"url":"http://example.org","max_bytes":limit}));
        (progress, plan.max_bytes)
    }

    #[test]
    fn byte_budget_rejection_and_bounded_reservation() {
        let (mut progress, limit) = accounting_fixture(5);
        observe_chunk(&mut progress, 0, limit).unwrap();
        observe_chunk(&mut progress, 5, limit).unwrap();
        assert_eq!(progress.received_bytes, 5);
        assert!(observe_chunk(&mut progress, 1, limit).is_err());
        assert_eq!(
            progress.received_bytes, 6,
            "rejecting chunk remains observable"
        );
        // Reservation caps growth and checks before allocation.
        let (_, limit) = accounting_fixture(100);
        let mut bytes = Vec::new();
        for _ in 0..100 {
            reserve_bounded(&mut bytes, 1, limit).unwrap();
            bytes.push(0);
            assert!(bytes.capacity() <= 100);
        }
        assert!(reserve_bounded(&mut bytes, 1, limit).is_err());
        assert!(reserve_bounded(&mut bytes, usize::MAX, limit).is_err());
        assert_eq!((bytes.len(), bytes.capacity()), (100, 100));
        let (_, small_limit) = accounting_fixture(1);
        let mut tiny = Vec::new();
        reserve_bounded(&mut tiny, 1, small_limit).unwrap();
        assert_eq!(tiny.capacity(), 1);
    }

    #[tokio::test]
    async fn extraction_failure_keeps_its_reason_and_the_response_payload() {
        let runtime = crate::tests::TestRuntime::new().await;
        let executor = executor(&runtime);
        let html = "<html><body></body></html>";
        let (url, task) = server(vec![response(
            "201 Created",
            "Content-Type: text/html\r\n",
            html,
        )])
        .await;
        let error = fetch(&runtime, &executor, json!({"url":url, "text":true}))
            .await
            .unwrap_err();
        task.await.unwrap();
        let output = error.into_tool_error().into_parts().1.unwrap().value;
        assert_eq!(output["status"], 201);
        assert_eq!(output["received_bytes"], html.len());
        assert_eq!(output["diagnostic"]["phase"], "extraction");
        let message = output["diagnostic"]["message"].as_str().unwrap();
        assert!(message.contains("parser could not extract"), "{message}");
    }

    #[test]
    fn download_staging_failure_identifies_destination_and_unchanged_effects() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("missing").join("download");
        let mut progress = progress_for(json!({"url":"http://example.org"})).1;
        let error = PendingDownload::new(destination.clone(), false, &mut progress)
            .err()
            .unwrap();
        let error = progress.failure(error);
        let context = error.diagnostic().context;
        assert_eq!(context.operation, Operation::CreateCapture);
        assert_eq!(context.subject, Subject::path(&destination));
        assert_eq!(context.effects, Effects::Unchanged);
        let (_, Some(output)) = error.into_parts() else {
            panic!("structured staging failure")
        };
        assert_eq!(output.value["diagnostic"]["phase"], "local_io");
        assert_eq!(output.value["diagnostic"]["os_error"]["kind"], "not_found");
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
                .write_all(response("200 OK", "", "replacement").as_bytes())
                .await
                .unwrap();
        });
        let executor = executor(&runtime);
        let error = tokio::time::timeout(
            Duration::from_secs(10),
            fetch(
                &runtime,
                &executor,
                json!({"url":url,"save_to":"raced-download"}),
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        let context = error.diagnostic().context;
        assert_eq!(context.operation, Operation::Rename);
        assert_eq!(context.subject, Subject::path(&destination));
        assert_eq!(context.effects, Effects::Unknown);
        assert!(
            error
                .to_string()
                .contains("server-side effects are unknown")
        );
        let output = error.into_tool_error().into_parts().1.unwrap().value;
        assert_eq!(output["status"], 200);
        assert_eq!(output["received_bytes"], 11);
        assert_eq!(output["diagnostic"]["phase"], "local_io");
        assert_eq!(
            tokio::fs::read(destination).await.unwrap(),
            b"keep original"
        );
    }

    #[cfg(unix)]
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
            assert_eq!(task.await.unwrap().len(), 1);
            assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"original");
        }
        let (url, task) = server(vec![response("200 OK", "", "replacement")]).await;
        let arguments = json!({"url":url,"save_to":"out","overwrite":true});
        let result = fetch(&runtime, &executor, arguments).await.unwrap();
        task.await.unwrap();
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
            fetch(&runtime, &executor, arguments).await.unwrap()["received_bytes"],
            0
        );
        task.await.unwrap();
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
        let diagnostic =
            serde_json::to_value(error.unwrap_err().into_diagnostic().unwrap()).unwrap();
        assert_eq!(
            (&diagnostic["phase"], &diagnostic["error_kind"]),
            (&json!("extraction"), &json!("extraction_failure"))
        );
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
                assert_eq!(output["received_bytes"], 4096);
                assert_eq!(output["body"]["text"], "A".repeat(4096));
            } else {
                assert!(result.is_err());
            }
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn cancellation_preserves_destination_and_cleans_temporary_download() {
        let runtime = crate::tests::TestRuntime::new().await;
        let destination = runtime.root.path().join("saved.txt");
        tokio::fs::write(&destination, b"original").await.unwrap();
        let (url, ready_rx, server_task) =
            stalled_server(b"HTTP/1.1 200 OK\r\nContent-Length: 10000\r\n\r\npartial").await;
        let executor = executor(&runtime);
        let agent = runtime.agent.clone();
        let arguments = json!({"url":url,"save_to":"saved.txt","overwrite":true});
        let pending =
            tokio::spawn(async move { executor.run_host(&agent, "fetch", arguments).await });
        tokio::time::timeout(Duration::from_secs(10), ready_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(runtime.jobs.cancel_all(&runtime.agent).await, 1);
        let result = tokio::time::timeout(Duration::from_secs(10), pending)
            .await
            .unwrap();
        assert!(result.unwrap().is_err());
        server_task.abort();
        let _ = server_task.await;
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
