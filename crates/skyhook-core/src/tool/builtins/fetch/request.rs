//! Prepare replayable uploads and dispatch authorized HTTP requests.
use std::path::Path;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use reqwest::{
    Method, Url,
    header::{HeaderMap, HeaderValue},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::diagnostics::{DiagnosticMessage, FetchDiagnostic, FetchError, FetchPhase};
use super::progress::FetchProgress;
use super::redirects::{FollowableRedirectStatus, redirect_error};
use super::response::{collect_headers, read_body};
use super::tls::client;
use super::validation::{FetchPlan, HttpRequestUrl, OutputPlan, redirect_headers};
use super::{
    FetchOutput, MAX_UPLOAD_BYTES, Redirect, RedirectPolicy, RequestBody, ToolContext, ToolError,
    invalid,
};

struct PreparedBody {
    bytes: Bytes,
    content_type: Option<HeaderValue>,
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

async fn snapshot_file(path: &Path, remaining: u64) -> Result<FileUpload, ToolError> {
    let sentinel = remaining + 1;
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
    let length = tokio::io::copy(&mut file.take(sentinel), &mut output).await?;
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
async fn prepare_body(body: Option<&RequestBody>) -> Result<Option<Upload>, ToolError> {
    let Some(body) = body else { return Ok(None) };
    let (bytes, content_type) = match body {
        RequestBody::Text { value } => (
            value.as_bytes().to_vec(),
            Some(HeaderValue::from_static("text/plain; charset=utf-8")),
        ),
        RequestBody::Json { value } => (
            serde_json::to_vec(value).map_err(invalid)?,
            Some(HeaderValue::from_static("application/json")),
        ),
        RequestBody::Form { fields } => {
            let mut url = Url::parse("http://localhost").expect("static URL");
            url.query_pairs_mut()
                .extend_pairs(fields.iter().map(|(k, v)| (k, v)));
            (
                url.query().unwrap_or_default().as_bytes().to_vec(),
                Some(HeaderValue::from_static(
                    "application/x-www-form-urlencoded",
                )),
            )
        }
        RequestBody::Base64 { value } => (decode_base64(value)?, None),
        RequestBody::File { path } => {
            return Ok(Some(Upload::File(
                snapshot_file(Path::new(path), MAX_UPLOAD_BYTES).await?,
            )));
        }
    };
    check_upload_size(bytes.len())?;
    Ok(Some(Upload::Bytes(PreparedBody {
        bytes: Bytes::from(bytes),
        content_type,
    })))
}

async fn apply_body(
    mut request: reqwest::RequestBuilder,
    body: Option<&Upload>,
    headers: &HeaderMap,
) -> Result<reqwest::RequestBuilder, FetchError> {
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
                .body(data.body().await.map_err(local_error)?);
        }
    }
    Ok(request)
}

/// A redirect consumes the complete request state, so method/header/body rewrites
/// cannot be applied independently or reused after the next hop is formed.
struct RequestState {
    url: HttpRequestUrl,
    method: Method,
    headers: HeaderMap,
    body: Option<Upload>,
}
impl RequestState {
    fn transition(
        mut self,
        status: FollowableRedirectStatus,
        next: HttpRequestUrl,
        progress: &mut FetchProgress,
    ) -> (Self, Redirect) {
        let hop = Redirect {
            status: status.get(),
            url: self.url.as_str().into(),
            location: next.as_str().into(),
            method: self.method.to_string(),
        };
        progress.redirect(status.get(), &self.url, &next, &self.method);
        let drop_body = status.rewrites_to_get(&self.method);
        redirect_headers(&mut self.headers, &self.url, &next, drop_body);
        if drop_body {
            self.method = Method::GET;
            self.body = None;
        }
        self.url = next;
        (self, hop)
    }
}
fn local_error(error: ToolError) -> FetchError {
    FetchError::from_tool_error(error, FetchPhase::LocalIo)
}

pub(super) async fn execute(
    context: &ToolContext,
    plan: FetchPlan,
    progress: &mut FetchProgress,
) -> Result<FetchOutput, FetchError> {
    progress.phase = FetchPhase::LocalIo;
    let local_io = |error: std::io::Error| FetchDiagnostic::from_io(&error, FetchPhase::LocalIo);
    if let OutputPlan::Download {
        destination,
        overwrite,
    } = &plan.output
        && tokio::fs::try_exists(destination).await.map_err(local_io)?
    {
        if !overwrite {
            return Err(local_error(invalid(
                "save_to already exists; set overwrite to replace it",
            )));
        }
        if !tokio::fs::metadata(destination)
            .await
            .map_err(local_io)?
            .is_file()
        {
            return Err(local_error(invalid("save_to must be a regular file")));
        }
    }
    progress.phase = FetchPhase::ClientPreparation;
    let client = client(&plan.client)?;
    progress.phase = FetchPhase::LocalIo;
    let body = prepare_body(plan.body.as_ref())
        .await
        .map_err(local_error)?;
    let mut state = RequestState {
        url: plan.url,
        method: plan.method,
        headers: plan.headers,
        body,
    };
    let mut redirects = Vec::new();
    // Admission-time initial authorization is still enforced by the registry.
    // This local comparison is sequencing, not invocation-bound authority evidence.
    let mut authorized_origin = state.url.origin();
    let response = loop {
        if context.is_cancelled() {
            return Err(FetchError::from_tool_error(
                ToolError::Cancelled,
                progress.phase,
            ));
        }
        progress.begin_request(&state.url, &state.method);
        if state.url.origin() != authorized_origin {
            progress.phase = FetchPhase::Authorization;
            context
                .authorize_network(state.url.origin().as_str())
                .await
                .map_err(|error| FetchError::from_tool_error(error, FetchPhase::Authorization))?;
            authorized_origin = state.url.origin();
        }
        progress.phase = FetchPhase::Request;
        let request = client
            .request(state.method.clone(), state.url.url().clone())
            .headers(state.headers.clone());
        // Reopening a snapshot is local I/O, including an outer deadline that
        // expires while open is pending; only the send is a network request.
        progress.phase = FetchPhase::LocalIo;
        let request = apply_body(request, state.body.as_ref(), &state.headers).await?;
        progress.phase = FetchPhase::Request;
        let response = request
            .send()
            .await
            .map_err(|error| FetchDiagnostic::from_reqwest(&error, FetchPhase::Request))?;
        progress.response(&response);
        let Some(status) = FollowableRedirectStatus::new(response.status().as_u16()) else {
            break response;
        };
        if plan.redirects == RedirectPolicy::Manual
            || (plan.redirects == RedirectPolicy::Safe
                && state.method != Method::GET
                && state.method != Method::HEAD)
        {
            break response;
        }
        progress.phase = FetchPhase::Redirect;
        let Some(location) = response.headers().get("location") else {
            break response;
        };
        if redirects.len() >= plan.max_redirects {
            return Err(redirect_error(DiagnosticMessage::MaximumRedirectsExceeded));
        }
        let location = location
            .to_str()
            .map_err(|_| redirect_error(DiagnosticMessage::InvalidRedirectLocationHeader))?;
        let next = state.url.join(location).map_err(redirect_error)?;
        if state.url.url().scheme() == "https" && next.url().scheme() == "http" {
            return Err(redirect_error(DiagnosticMessage::HttpsDowngradeBlocked));
        }
        let (next, hop) = state.transition(status, next, progress);
        state = next;
        redirects.push(hop);
    };
    let status = response.status();
    let headers = plan
        .include_headers
        .then(|| collect_headers(response.headers()));
    let body = read_body(
        context,
        progress,
        response,
        plan.output,
        plan.max_bytes,
        &state.url,
        &state.method,
    )
    .await?;
    Ok(FetchOutput {
        status: status.as_u16(),
        ok: status.is_success(),
        url: state.url.as_str().into(),
        method: state.method.to_string(),
        headers,
        redirects,
        body,
        received_bytes: progress.received_bytes,
        elapsed_ms: progress.elapsed_ms(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{executor, fetch, response, server};
    use super::*;
    use serde_json::json;

    const OK: &str = "Content-Type: text/plain\r\n";

    #[tokio::test]
    async fn methods_body_encodings_and_307_file_replays_reach_the_server() {
        let runtime = crate::tests::TestRuntime::new().await;
        tokio::fs::write(runtime.root.path().join("upload.txt"), b"file payload")
            .await
            .unwrap();
        let executor = executor(&runtime);
        let file = json!({"kind":"file","path":"upload.txt"});
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
            ("PUT", Some(file.clone()), "file payload"),
        ];
        let (url, task) = server(cases.iter().map(|_| response("200 OK", OK, "")).collect()).await;
        for (method, body, _) in &cases {
            let mut arguments = json!({"url":url,"method":method});
            if let Some(body) = body {
                arguments["body"] = body.clone();
            }
            assert_eq!(
                fetch(&runtime, &executor, arguments).await.unwrap()["status"],
                200
            );
        }
        for (request, (method, _, expected)) in task.await.unwrap().iter().zip(cases) {
            assert!(
                request.starts_with(&format!("{method} / HTTP/1.1\r\n")),
                "{request}"
            );
            assert_eq!(request.split_once("\r\n\r\n").unwrap().1, expected);
        }
        // A followed 307 replays the file upload with its method.
        let (url, task) = server(vec![
            response("307 Temporary Redirect", "Location: /again\r\n", ""),
            response("200 OK", OK, "done"),
        ])
        .await;
        let arguments = json!({"url":url,"method":"PUT","redirects":"follow","body":file});
        fetch(&runtime, &executor, arguments).await.unwrap();
        for request in task.await.unwrap() {
            assert!(request.starts_with("PUT ") && request.ends_with("file payload"));
        }
    }

    #[tokio::test]
    async fn http_errors_query_repeated_headers_and_auth() {
        let headers = "Content-Type: text/plain\r\nSet-Cookie: one=1\r\nSet-Cookie: two=2\r\n";
        let (url, task) = server(vec![response("404 Not Found", headers, "missing")]).await;
        let runtime = crate::tests::TestRuntime::new().await;
        let arguments = json!({
            "url":format!("{url}/p?z=0"), "query":[["q","a b"],["q","c"]], "include_headers":true,
            "headers":{"x-repeat":["one","two"]}, "auth":{"kind":"basic","username":"u","password":"p"}
        });
        let output = fetch(&runtime, &executor(&runtime), arguments)
            .await
            .unwrap();
        assert_eq!(
            (&output["status"], &output["ok"]),
            (&json!(404), &json!(false))
        );
        assert_eq!(output["body"]["text"], "missing");
        assert_eq!(output["headers"]["set-cookie"], json!(["one=1", "two=2"]));
        let requests = task.await.unwrap();
        assert!(requests[0].starts_with("GET /p?z=0&q=a+b&q=c HTTP/1.1"));
        assert!(requests[0].contains("authorization: Basic dTpw"));
        assert!(requests[0].contains("x-repeat: one\r\nx-repeat: two"));
    }

    #[tokio::test]
    async fn file_upload_snapshot_is_bounded_and_immutable() {
        let root = tempfile::tempdir().unwrap();
        assert!(snapshot_file(root.path(), MAX_UPLOAD_BYTES).await.is_err());
        let source = root.path().join("source");
        tokio::fs::write(&source, b"original").await.unwrap();
        assert!(snapshot_file(&source, 3).await.is_err());
        let FileUpload { snapshot, length } = snapshot_file(&source, 100).await.unwrap();
        tokio::fs::write(&source, b"changed").await.unwrap();
        assert_eq!(length, 8);
        assert_eq!(tokio::fs::read(snapshot.path()).await.unwrap(), b"original");
    }
}
