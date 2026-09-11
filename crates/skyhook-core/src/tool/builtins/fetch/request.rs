//! Prepare replayable uploads and dispatch authorized HTTP requests.
use std::path::Path;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::{Method, Url, header::HeaderMap};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::super::workspace::{resolve_existing, resolve_writable};
use super::diagnostics::FetchPhase;
use super::progress::FetchProgress;
use super::redirects::{redirect_error, redirect_method, strip_redirect_headers};
use super::response::{collect_headers, read_body};
use super::tls::client;
use super::validation::{parse_url, request_headers};
use super::{
    FetchArgs, FetchOutput, MAX_UPLOAD_BYTES, Redirect, RedirectPolicy, RequestBody, ToolContext,
    ToolError, invalid, network,
};

struct PreparedBody {
    bytes: Vec<u8>,
    content_type: Option<String>,
}
struct FileUpload {
    snapshot: tempfile::NamedTempFile,
    length: u64,
}
impl FileUpload {
    fn len(&self) -> u64 {
        self.length
    }
    async fn body(&self) -> Result<reqwest::Body, ToolError> {
        let file = tokio::fs::File::open(self.snapshot.path()).await?;
        Ok(reqwest::Body::wrap_stream(
            tokio_util::io::ReaderStream::new(file),
        ))
    }
}
enum Upload {
    Bytes(PreparedBody),
    File(FileUpload),
}

async fn upload_file(
    workspace: &Path,
    path: &str,
    remaining: u64,
) -> Result<FileUpload, ToolError> {
    let path = resolve_existing(workspace, path).await?;
    if !tokio::fs::metadata(&path).await?.is_file() {
        return Err(invalid("upload path must be a regular file"));
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    // Avoid blocking if the path is swapped to a FIFO between metadata and open.
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);
    let file = options.open(&path).await?;
    let metadata = file.metadata().await?;
    if !metadata.is_file() {
        return Err(invalid("upload path must be a regular file"));
    }
    if metadata.len() > remaining {
        return Err(invalid("upload exceeds 100 MiB limit"));
    }
    // Snapshot once, with a bounded streaming copy. Redirect replays reopen this
    // immutable private snapshot, not a potentially changed source file.
    let snapshot = tempfile::NamedTempFile::new()?;
    let mut output = tokio::fs::File::from_std(snapshot.as_file().try_clone()?);
    let length = tokio::io::copy(&mut file.take(remaining + 1), &mut output).await?;
    output.flush().await?;
    if length > remaining {
        return Err(invalid("upload exceeds 100 MiB limit"));
    }
    Ok(FileUpload { snapshot, length })
}
fn check_upload_size(size: usize) -> Result<(), ToolError> {
    if size as u64 > MAX_UPLOAD_BYTES {
        Err(invalid("upload exceeds 100 MiB limit"))
    } else {
        Ok(())
    }
}
fn decode_base64(value: &str) -> Result<Vec<u8>, ToolError> {
    if value.len() as u64 > MAX_UPLOAD_BYTES.div_ceil(3) * 4 {
        return Err(invalid("upload exceeds 100 MiB limit"));
    }
    let bytes = STANDARD
        .decode(value)
        .map_err(|_| invalid("invalid base64 body"))?;
    check_upload_size(bytes.len())?;
    Ok(bytes)
}
async fn prepare_body(
    body: Option<&RequestBody>,
    workspace: &Path,
) -> Result<Option<Upload>, ToolError> {
    let Some(body) = body else { return Ok(None) };
    let (bytes, content_type) = match body {
        RequestBody::Text { value } => (
            value.as_bytes().to_vec(),
            Some("text/plain; charset=utf-8".into()),
        ),
        RequestBody::Json { value } => (
            serde_json::to_vec(value).map_err(invalid)?,
            Some("application/json".into()),
        ),
        RequestBody::Form { fields } => {
            let mut url = Url::parse("http://localhost").expect("static URL");
            url.query_pairs_mut()
                .extend_pairs(fields.iter().map(|(k, v)| (k, v)));
            (
                url.query().unwrap_or_default().as_bytes().to_vec(),
                Some("application/x-www-form-urlencoded".into()),
            )
        }
        RequestBody::Base64 { value } => (decode_base64(value)?, None),
        RequestBody::File { path } => {
            return Ok(Some(Upload::File(
                upload_file(workspace, path, MAX_UPLOAD_BYTES).await?,
            )));
        }
    };
    check_upload_size(bytes.len())?;
    Ok(Some(Upload::Bytes(PreparedBody {
        bytes,
        content_type,
    })))
}

async fn apply_body(
    mut request: reqwest::RequestBuilder,
    body: Option<&Upload>,
    headers: &HeaderMap,
) -> Result<reqwest::RequestBuilder, ToolError> {
    match body {
        None => {}
        Some(Upload::Bytes(body)) => {
            if !headers.contains_key("content-type")
                && let Some(content_type) = &body.content_type
            {
                request = request.header("content-type", content_type);
            }
            request = request.body(body.bytes.clone());
        }
        Some(Upload::File(data)) => {
            request = request
                .header("content-length", data.len())
                .body(data.body().await?);
        }
    }
    Ok(request)
}

pub(super) async fn execute(
    context: &ToolContext,
    args: &FetchArgs,
    progress: &mut FetchProgress,
) -> Result<FetchOutput, ToolError> {
    let started = progress.started;
    progress.phase = FetchPhase::LocalIo;
    let workspace = &context.execution_location.workspace;
    let destination = match &args.save_to {
        Some(path) => {
            let path = resolve_writable(workspace, path).await?;
            if tokio::fs::try_exists(&path).await? {
                if !args.overwrite {
                    return Err(invalid(
                        "save_to already exists; set overwrite to replace it",
                    ));
                }
                if !tokio::fs::metadata(&path).await?.is_file() {
                    return Err(invalid("save_to must be a regular file"));
                }
            }
            Some(path)
        }
        None => None,
    };
    progress.phase = FetchPhase::ClientPreparation;
    let client = client(args)?;
    let mut url = parse_url(&args.url)?;
    if !args.query.is_empty() {
        url.query_pairs_mut()
            .extend_pairs(args.query.iter().map(|(k, v)| (k, v)));
    }
    url.set_fragment(None);
    let mut method = Method::from_bytes(args.method.as_bytes()).map_err(invalid)?;
    let mut headers = request_headers(args)?;
    progress.phase = FetchPhase::LocalIo;
    let mut body = prepare_body(args.body.as_ref(), workspace).await?;
    let mut redirects = Vec::new();
    let mut authorized_origin = url.origin();
    let response = loop {
        if context.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        progress.begin_request(&url, &method);
        // The initial origin was authorized by argument_permissions. A changed origin
        // must be authorized before sending any headers or replaying any upload.
        if url.origin() != authorized_origin {
            progress.phase = FetchPhase::Authorization;
            authorize_url(context, &url).await?;
            authorized_origin = url.origin();
        }
        progress.phase = FetchPhase::Request;
        let request = client
            .request(method.clone(), url.clone())
            .headers(headers.clone());
        let response = apply_body(request, body.as_ref(), &headers)
            .await?
            .send()
            .await
            .map_err(|error| network(error, FetchPhase::Request))?;
        progress.response(&response);
        let status = response.status().as_u16();
        let follow = matches!(status, 301 | 302 | 303 | 307 | 308)
            && args.redirects != RedirectPolicy::Manual
            && (args.redirects == RedirectPolicy::Follow
                || method == Method::GET
                || method == Method::HEAD);
        if !follow {
            break response;
        }
        progress.phase = FetchPhase::Redirect;
        let Some(location) = response.headers().get("location") else {
            break response;
        };
        if redirects.len() >= args.max_redirects {
            return Err(redirect_error(
                "maximum redirects exceeded",
                &response,
                &method,
                &redirects,
                started,
                args.include_headers,
            ));
        }
        let location = location.to_str().map_err(|_| {
            redirect_error(
                "invalid redirect location header",
                &response,
                &method,
                &redirects,
                started,
                args.include_headers,
            )
        })?;
        let next = url.join(location).map_err(|_| {
            redirect_error(
                "invalid redirect URL",
                &response,
                &method,
                &redirects,
                started,
                args.include_headers,
            )
        })?;
        let mut next = parse_url(next.as_str()).map_err(|_| {
            redirect_error(
                "redirect URL must be HTTP(S) without embedded credentials",
                &response,
                &method,
                &redirects,
                started,
                args.include_headers,
            )
        })?;
        next.set_fragment(None);
        if url.scheme() == "https" && next.scheme() == "http" {
            return Err(redirect_error(
                "HTTPS to HTTP redirect blocked",
                &response,
                &method,
                &redirects,
                started,
                args.include_headers,
            ));
        }
        let (next_method, drop_body) = redirect_method(status, &method);
        strip_redirect_headers(&mut headers, &url, &next, drop_body);
        if drop_body {
            body = None;
        }
        redirects.push(Redirect {
            status,
            url: url.to_string(),
            location: next.to_string(),
            method: method.to_string(),
        });
        progress.redirects(&redirects);
        url = next;
        method = next_method;
    };
    let status = response.status();
    let headers = args
        .include_headers
        .then(|| collect_headers(response.headers()));
    let body = read_body(
        context,
        args,
        progress,
        response,
        destination,
        &url,
        &method,
    )
    .await?;
    Ok(FetchOutput {
        status: status.as_u16(),
        ok: status.is_success(),
        url: url.to_string(),
        method: method.to_string(),
        headers,
        redirects,
        body,
        received_bytes: progress.received_bytes,
        elapsed_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
    })
}

async fn authorize_url(context: &ToolContext, url: &Url) -> Result<(), ToolError> {
    context
        .authorize_network(&url.origin().ascii_serialization())
        .await
}

#[cfg(test)]
mod tests {
    use super::super::tests::{executor, fetch, response, server};
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn methods_and_body_encodings_reach_the_server() {
        let runtime = crate::tests::TestRuntime::new().await;
        tokio::fs::write(runtime.root.path().join("upload.txt"), b"file payload")
            .await
            .unwrap();
        let executor = executor(&runtime);
        let cases = [
            ("GET", None, ""),
            ("HEAD", None, ""),
            ("OPTIONS", None, ""),
            ("DELETE", None, ""),
            ("TRACE", None, ""),
            (
                "PROPFIND",
                Some(json!({"kind":"text","value":"custom payload"})),
                "custom payload",
            ),
            ("POST", Some(json!({"kind":"json","value":null})), "null"),
            (
                "PATCH",
                Some(json!({"kind":"json","value":{"enabled":true}})),
                "{\"enabled\":true}",
            ),
            (
                "POST",
                Some(json!({"kind":"form","fields":[["key","a b"],["key","&"]]})),
                "key=a+b&key=%26",
            ),
            (
                "PUT",
                Some(json!({"kind":"base64","value":"Ynl0ZXM="})),
                "bytes",
            ),
            (
                "PUT",
                Some(json!({"kind":"file","path":"upload.txt"})),
                "file payload",
            ),
        ];
        let (url, task) = server(
            cases
                .iter()
                .map(|_| response("200 OK", "Content-Type: text/plain\r\n", ""))
                .collect(),
        )
        .await;
        for (method, body, _) in &cases {
            let mut arguments = json!({"url":url,"method":method});
            if let Some(body) = body {
                arguments["body"] = body.clone();
            }
            let result = fetch(&runtime, &executor, arguments).await.unwrap();
            assert_eq!(result["status"], 200);
        }
        let requests = task.await.unwrap();
        for (request, (method, _, expected)) in requests.iter().zip(cases) {
            assert!(
                request.starts_with(&format!("{method} / HTTP/1.1\r\n")),
                "{request}"
            );
            assert_eq!(request.split_once("\r\n\r\n").unwrap().1, expected);
        }
    }

    #[tokio::test]
    async fn http_errors_query_repeated_headers_and_auth() {
        let (url, task) = server(vec![response(
            "404 Not Found",
            "Content-Type: text/plain\r\nSet-Cookie: one=1\r\nSet-Cookie: two=2\r\n",
            "missing",
        )])
        .await;
        let runtime = crate::tests::TestRuntime::new().await;
        let result = fetch(&runtime, &executor(&runtime), json!({"url":format!("{url}/p?z=0"),"query":[["q","a b"],["q","c"]], "include_headers":true, "headers":{"x-repeat":["one","two"]},"auth":{"kind":"basic","username":"u","password":"p"}})).await.unwrap();
        let output = result;
        assert_eq!(output["status"], 404);
        assert_eq!(output["ok"], false);
        assert_eq!(output["body"]["text"], "missing");
        assert_eq!(output["headers"]["set-cookie"], json!(["one=1", "two=2"]));
        let requests = task.await.unwrap();
        assert!(requests[0].starts_with("GET /p?z=0&q=a+b&q=c HTTP/1.1"));
        assert!(requests[0].contains("authorization: Basic dTpw"));
        assert!(requests[0].contains("x-repeat: one\r\nx-repeat: two"));
    }

    #[tokio::test]
    async fn follow_307_replays_file_uploads() {
        let runtime = crate::tests::TestRuntime::new().await;
        tokio::fs::write(runtime.root.path().join("upload.txt"), b"file payload")
            .await
            .unwrap();
        let executor = executor(&runtime);
        let (url, task) = server(vec![
            response("307 Temporary Redirect", "Location: /again\r\n", ""),
            response("200 OK", "Content-Type: text/plain\r\n", "done"),
        ])
        .await;
        fetch(&runtime, &executor, json!({"url":url,"method":"PUT","redirects":"follow","body":{"kind":"file","path":"upload.txt"}})).await.unwrap();
        for request in task.await.unwrap() {
            assert!(request.starts_with("PUT "));
            assert!(request.ends_with("file payload"));
        }
    }

    #[tokio::test]
    async fn file_upload_snapshot_is_bounded_and_immutable() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            upload_file(root.path(), ".", MAX_UPLOAD_BYTES)
                .await
                .is_err()
        );
        tokio::fs::write(root.path().join("source"), b"original")
            .await
            .unwrap();
        assert!(upload_file(root.path(), "source", 3).await.is_err());
        let data = upload_file(root.path(), "source", 100).await.unwrap();
        tokio::fs::write(root.path().join("source"), b"changed")
            .await
            .unwrap();
        let FileUpload { snapshot, length } = data;
        assert_eq!(length, 8);
        assert_eq!(tokio::fs::read(snapshot.path()).await.unwrap(), b"original");
    }
}
