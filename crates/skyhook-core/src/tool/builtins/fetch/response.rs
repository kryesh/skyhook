//! Bounded response streaming, atomic downloads, and response decoding.
use std::{collections::BTreeMap, path::PathBuf};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use reqwest::{Method, Url, header::HeaderMap};
use tokio::io::AsyncWriteExt;

use super::diagnostics::{FetchErrorKind, FetchPhase};
use super::progress::{FetchProgress, classified_error};
use super::{
    FetchArgs, ResponseBody, ResponseFormat, ToolContext, ToolError, failed, fetch_text, invalid,
    network,
};

pub(super) async fn read_body(
    context: &ToolContext,
    args: &FetchArgs,
    progress: &mut FetchProgress,
    response: reqwest::Response,
    destination: Option<PathBuf>,
    url: &Url,
    method: &Method,
) -> Result<ResponseBody, ToolError> {
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let mut received = 0u64;
    progress.phase = FetchPhase::ResponseBody;
    // content_length is decoded length when known; the streamed count remains authoritative.
    if *method != Method::HEAD
        && response
            .content_length()
            .is_some_and(|length| length > args.max_bytes)
    {
        return Err(classified_error(
            FetchPhase::ResponseBody,
            FetchErrorKind::SizeLimit,
            "response exceeds max_bytes",
        ));
    }
    progress.phase = FetchPhase::LocalIo;
    let mut temp = match &destination {
        Some(path) => Some(tempfile::NamedTempFile::new_in(
            path.parent()
                .ok_or_else(|| invalid("save_to has no parent"))?,
        )?),
        None => None,
    };
    let mut file = match &temp {
        Some(temp) => Some(tokio::fs::File::from_std(temp.as_file().try_clone()?)),
        None => None,
    };
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    loop {
        progress.phase = FetchPhase::ResponseBody;
        let Some(chunk) = stream.next().await else {
            break;
        };
        let chunk = chunk.map_err(|error| network(error, FetchPhase::ResponseBody))?;
        received = received
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| failed("response too large"))?;
        progress.received_bytes = received;
        if received > args.max_bytes {
            return Err(classified_error(
                FetchPhase::ResponseBody,
                FetchErrorKind::SizeLimit,
                "response exceeds max_bytes",
            ));
        }
        if let Some(file) = &mut file {
            progress.phase = FetchPhase::LocalIo;
            file.write_all(&chunk).await?;
        } else {
            bytes.extend_from_slice(&chunk);
        }
    }
    let response_body = if let Some(mut file) = file {
        progress.phase = FetchPhase::LocalIo;
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        if context.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let temp = temp.take().expect("download temporary file");
        let destination = destination.as_ref().expect("download destination");
        if args.overwrite {
            temp.persist(destination)
                .map_err(|e| ToolError::from(e.error))?;
        } else {
            temp.persist_noclobber(destination)
                .map_err(|e| ToolError::from(e.error))?;
        }
        ResponseBody::File {
            path: destination.to_string_lossy().into_owned(),
            bytes: received,
        }
    } else {
        progress.phase = if args.text {
            FetchPhase::Extraction
        } else {
            FetchPhase::Decode
        };
        response_body(bytes, content_type.as_deref(), url.as_str(), args).await?
    };
    Ok(response_body)
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
    url: &str,
    args: &FetchArgs,
) -> Result<ResponseBody, ToolError> {
    if bytes.is_empty() {
        return Ok(ResponseBody::Empty);
    }
    if args.text {
        if fetch_text::is_html(&bytes, content_type) {
            if bytes.len() > 10 * 1024 * 1024 {
                return Err(failed("HTML extraction input exceeds 10 MiB limit"));
            }
            let extracted =
                fetch_text::extract(fetch_text::decode(&bytes, content_type), url.to_owned())
                    .await?;
            return Ok(ResponseBody::Text {
                text: extracted.text,
                metadata: Some(extracted.metadata),
            });
        }
        if !fetch_text::is_textual(&bytes, content_type) {
            return Err(classified_error(
                FetchPhase::Extraction,
                FetchErrorKind::ExtractionFailure,
                "text extraction is unsupported for this binary content type",
            ));
        }
        return Ok(ResponseBody::Text {
            text: fetch_text::decode(&bytes, content_type),
            metadata: None,
        });
    }
    if args.response_format == ResponseFormat::Base64
        || (args.response_format == ResponseFormat::Auto
            && !fetch_text::is_textual(&bytes, content_type))
    {
        Ok(ResponseBody::Base64 {
            data: STANDARD.encode(bytes),
        })
    } else {
        Ok(ResponseBody::Text {
            text: fetch_text::decode(&bytes, content_type),
            metadata: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{args, executor, fetch, response, server, stalled_server};
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[tokio::test]
    async fn download_atomicity_limits_and_head() {
        let runtime = crate::tests::TestRuntime::new().await;
        let destination = runtime.root.path().join("out");
        tokio::fs::write(&destination, b"original").await.unwrap();
        let executor = executor(&runtime);
        let oversized = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\n1234\r\n4\r\n5678\r\n0\r\n\r\n".to_owned();
        let (url, task) = server(vec![oversized]).await;
        assert!(
            fetch(
                &runtime,
                &executor,
                json!({"url":url,"save_to":"out","overwrite":true,"max_bytes":5})
            )
            .await
            .is_err()
        );
        task.await.unwrap();
        assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"original");
        let (url, task) = server(vec![response("200 OK", "", "replacement")]).await;
        let result = fetch(
            &runtime,
            &executor,
            json!({"url":url,"save_to":"out","overwrite":true}),
        )
        .await
        .unwrap();
        task.await.unwrap();
        assert_eq!(result["body"]["kind"], "file");
        assert_eq!(result["body"]["bytes"], 11);
        assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"replacement");
        assert!(
            fetch(
                &runtime,
                &executor,
                json!({"url":"http://127.0.0.1:1","save_to":"out"})
            )
            .await
            .is_err()
        );
        let (url, task) = server(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\nConnection: close\r\n\r\n".to_owned(),
        ])
        .await;
        let result = fetch(
            &runtime,
            &executor,
            json!({"url":url,"method":"HEAD","max_bytes":1}),
        )
        .await
        .unwrap();
        assert_eq!(result["received_bytes"], 0);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn binary_and_charset_response_formats() {
        let a = args(json!({"url":"http://example.org"}));
        assert!(
            matches!(response_body(vec![0, 255, 3], Some("application/octet-stream"), &a.url, &a).await.unwrap(), ResponseBody::Base64 { data } if data == "AP8D")
        );
        assert!(
            matches!(response_body(vec![0xe9], Some("text/plain; charset=windows-1252"), &a.url, &a).await.unwrap(), ResponseBody::Text { text, .. } if text == "é")
        );
        let a = args(json!({"url":"http://example.org","text":true}));
        assert!(
            response_body(vec![0, 255], Some("application/pdf"), &a.url, &a)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn truncated_response_fails_without_retries() {
        let runtime = crate::tests::TestRuntime::new().await;
        let executor = executor(&runtime);
        let (url, task) = server(vec![
            "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 100\r\n\r\nshort".to_owned(),
        ])
        .await;
        assert!(
            fetch(&runtime, &executor, json!({"url":url}))
                .await
                .is_err()
        );
        assert_eq!(task.await.unwrap().len(), 1);
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
        let pending = tokio::spawn(async move {
            executor
                .execute(
                    agent,
                    "fetch",
                    json!({"url":url,"save_to":"saved.txt","overwrite":true}),
                    None,
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(10), ready_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(runtime.jobs.cancel_all(&runtime.agent).await, 1);
        assert!(
            tokio::time::timeout(Duration::from_secs(10), pending)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
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
