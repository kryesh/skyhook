//! Per-operation context retained even when the outer deadline cancels a request.
use std::time::Instant;

use reqwest::Url;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

use super::diagnostics::{FetchDiagnostic, FetchErrorKind, FetchPhase, FetchTimeoutKind};
use super::response::collect_headers;
use super::{FetchArgs, Redirect, ResponseBody, ToolError, ToolOutput};

/// Failures add diagnostics and only include HTTP response fields when a response
/// actually arrived. Response headers also require explicit opt-in.
#[derive(Serialize, JsonSchema)]
pub(super) struct FetchFailureOutput {
    method: String,
    origin: String,
    elapsed_ms: u64,
    received_bytes: u64,
    redirects: Vec<Redirect>,
    diagnostic: FetchDiagnostic,
    /// Explicitly configured proxy only; omission does not rule out an environment proxy.
    #[serde(skip_serializing_if = "Option::is_none")]
    proxy_origin: Option<String>,
    #[serde(flatten)]
    response: Option<FailureResponse>,
}

#[derive(Clone, Serialize, JsonSchema)]
struct FailureResponse {
    status: u16,
    ok: bool,
    url: String,
    /// Response headers, present only when include_headers is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    headers: Option<std::collections::BTreeMap<String, Vec<String>>>,
    body: ResponseBody,
}

pub(super) struct FetchProgress {
    pub started: Instant,
    pub phase: FetchPhase,
    pub received_bytes: u64,
    method: String,
    origin: String,
    redirects: Vec<Redirect>,
    response: Option<FailureResponse>,
    proxy_origin: Option<String>,
    connect_timeout_ms: u64,
    include_headers: bool,
}

impl FetchProgress {
    pub fn new(args: &FetchArgs) -> Self {
        Self {
            started: Instant::now(),
            phase: FetchPhase::ClientPreparation,
            received_bytes: 0,
            method: args.method.clone(),
            origin: safe_origin(&args.url),
            redirects: Vec::new(),
            response: None,
            proxy_origin: args.proxy.as_deref().map(safe_origin),
            connect_timeout_ms: args.connect_timeout.saturating_mul(1000),
            include_headers: args.include_headers,
        }
    }

    pub fn begin_request(&mut self, url: &Url, method: &reqwest::Method) {
        self.origin = url.origin().ascii_serialization();
        self.method = method.to_string();
        self.response = None;
        self.received_bytes = 0;
        self.phase = FetchPhase::Request;
    }

    pub fn response(&mut self, response: &reqwest::Response) {
        self.response = Some(FailureResponse {
            status: response.status().as_u16(),
            ok: response.status().is_success(),
            // Keep HTTP metadata, but never copy secret query parameters into
            // failure URL fields. The original request is already job input.
            url: response.url().origin().ascii_serialization(),
            headers: self
                .include_headers
                .then(|| collect_headers(response.headers())),
            body: ResponseBody::Empty,
        });
    }

    pub fn redirects(&mut self, redirects: &[Redirect]) {
        self.redirects = redirects
            .iter()
            .map(|hop| Redirect {
                status: hop.status,
                url: safe_origin(&hop.url),
                location: safe_origin(&hop.location),
                method: hop.method.clone(),
            })
            .collect();
    }

    pub fn timeout(&self, limit_secs: u64) -> ToolError {
        self.diagnostic_failure(
            FetchDiagnostic::timeout(
                self.phase,
                FetchTimeoutKind::Total,
                limit_secs.saturating_mul(1000),
            ),
            None,
        )
    }

    pub fn failure(&self, error: ToolError) -> ToolError {
        match error {
            // These are not HTTP transport failures and keep their existing contracts.
            ToolError::Cancelled
            | ToolError::Denied(_)
            | ToolError::InvalidArguments(_)
            | ToolError::ArgumentsMustBeObject
            | ToolError::InvalidBackground
            | ToolError::BackgroundUnsupported(_)
            | ToolError::InputClosed => error,
            ToolError::Io(error) => {
                self.diagnostic_failure(FetchDiagnostic::from_io(&error, self.phase), None)
            }
            ToolError::FailedWithOutput { output, .. } => {
                let diagnostic = output
                    .value
                    .get("diagnostic")
                    .cloned()
                    .and_then(|value| serde_json::from_value(value).ok())
                    .unwrap_or_else(|| self.fallback());
                self.diagnostic_failure(diagnostic, Some(*output))
            }
            ToolError::Failed(_) | ToolError::Json(_) => {
                self.diagnostic_failure(self.fallback(), None)
            }
        }
    }

    fn fallback(&self) -> FetchDiagnostic {
        let kind = match self.phase {
            FetchPhase::ClientPreparation => FetchErrorKind::ClientConfiguration,
            FetchPhase::ResponseBody => FetchErrorKind::ResponseBodyFailure,
            FetchPhase::Decode => FetchErrorKind::DecodeFailure,
            FetchPhase::Extraction => FetchErrorKind::ExtractionFailure,
            FetchPhase::LocalIo => FetchErrorKind::LocalIo,
            FetchPhase::Redirect => FetchErrorKind::RedirectFailure,
            _ => FetchErrorKind::Transport,
        };
        FetchDiagnostic::new(self.phase, kind)
    }

    fn diagnostic_failure(
        &self,
        mut diagnostic: FetchDiagnostic,
        previous: Option<ToolOutput>,
    ) -> ToolError {
        if let Some(timeout) = &mut diagnostic.timeout
            && timeout.kind == FetchTimeoutKind::Connect
            && timeout.limit_ms.is_none()
        {
            timeout.limit_ms = Some(self.connect_timeout_ms);
        }
        let elapsed_ms = self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let mut summary = format!(
            "HTTP {} {}: {} ({} ms)",
            self.method, self.origin, diagnostic.message, elapsed_ms
        );
        if let Some(os) = &diagnostic.os_error
            && let Some(code) = os.code
        {
            summary.push_str(&format!("; OS error {code} on {}", os.platform));
        }
        let report = FetchFailureOutput {
            method: self.method.clone(),
            origin: self.origin.clone(),
            elapsed_ms,
            received_bytes: self.received_bytes,
            redirects: self.redirects.clone(),
            diagnostic,
            proxy_origin: self.proxy_origin.clone(),
            response: self.response.clone(),
        };
        let mut value = serde_json::to_value(report).expect("fetch diagnostics serialize");
        let mut images = Vec::new();
        if let Some(previous) = previous {
            images = previous.images;
            if let (Some(result), Value::Object(previous)) = (value.as_object_mut(), previous.value)
            {
                for (key, value) in previous {
                    result.entry(key).or_insert(value);
                }
            }
        }
        if !self.include_headers {
            // Prior structured errors must not reintroduce headers during merging.
            value
                .as_object_mut()
                .expect("fetch failure object")
                .remove("headers");
        }
        ToolError::with_output(summary, ToolOutput::new(value).with_images(images))
    }
}

fn safe_origin(value: &str) -> String {
    Url::parse(value)
        .ok()
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_else(|| "unknown origin".to_owned())
}

pub(super) fn diagnostic_error(diagnostic: FetchDiagnostic) -> ToolError {
    ToolError::with_output(
        diagnostic.message.clone(),
        ToolOutput::new(serde_json::json!({"diagnostic":diagnostic})),
    )
}

pub(super) fn classified_error(
    phase: FetchPhase,
    kind: FetchErrorKind,
    message: &'static str,
) -> ToolError {
    let mut diagnostic = FetchDiagnostic::new(phase, kind);
    diagnostic.message = message.to_owned();
    diagnostic_error(diagnostic)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{args, executor, fetch, response, server, stalled_server};
    use super::*;
    use serde_json::json;
    use std::time::Duration;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn response_headers_are_opt_in_for_http_results_and_failures() {
        let runtime = crate::tests::TestRuntime::new().await;
        let executor = executor(&runtime);
        for include_headers in [None, Some(false), Some(true)] {
            for (status, extra, options, error_kind) in [
                ("200 OK", "", json!({}), None),
                ("404 Not Found", "", json!({}), None),
                ("200 OK", "", json!({"max_bytes":1}), Some("size_limit")),
                (
                    "201 Created",
                    "",
                    json!({"method":"POST","text":true}),
                    Some("extraction_failure"),
                ),
                (
                    "302 Found",
                    "Location: /again\r\n",
                    json!({"max_redirects":0}),
                    Some("redirect_failure"),
                ),
            ] {
                let (url, task) = server(vec![response(
                    status,
                    &format!(
                        "Content-Type: application/pdf\r\nX-Result: one\r\nX-Result: two\r\n{extra}"
                    ),
                    "body",
                )])
                .await;
                let mut arguments = options;
                arguments["url"] = json!(url);
                if let Some(include) = include_headers {
                    arguments["include_headers"] = json!(include);
                }
                let result = fetch(&runtime, &executor, arguments).await;
                let output = if let Some(kind) = error_kind {
                    let output = result.unwrap_err().into_failure().output.unwrap().value;
                    assert_eq!(output["diagnostic"]["error_kind"], kind);
                    if kind == "extraction_failure" {
                        assert_eq!(output["method"], "POST");
                        assert_eq!(output["received_bytes"], 4);
                    }
                    output
                } else {
                    let output = result.unwrap();
                    assert!(output.get("diagnostic").is_none());
                    assert_eq!(output["body"], json!({"kind":"base64", "data":"Ym9keQ=="}));
                    output
                };
                let status: u16 = status.split_whitespace().next().unwrap().parse().unwrap();
                assert_eq!(output["status"], status);
                assert_eq!(output["ok"], (200..300).contains(&status));
                if include_headers == Some(true) {
                    assert_eq!(output["headers"]["x-result"], json!(["one", "two"]));
                } else {
                    assert!(output.get("headers").is_none(), "{output}");
                }
                task.await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn transport_failures_have_no_response_headers_even_with_opt_in() {
        let runtime = crate::tests::TestRuntime::new().await;
        let executor = executor(&runtime);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        for include_headers in [None, Some(false), Some(true)] {
            let mut arguments = json!({"url":url});
            if let Some(include) = include_headers {
                arguments["include_headers"] = json!(include);
            }
            let error = fetch(&runtime, &executor, arguments).await.unwrap_err();
            let output = error.into_failure().output.unwrap().value;
            assert_eq!(output["diagnostic"]["error_kind"], "connection_refused");
            assert!(output.get("status").is_none());
            assert!(output.get("headers").is_none());
        }
    }

    #[tokio::test]
    async fn response_body_deadline_preserves_opt_in_headers() {
        let runtime = crate::tests::TestRuntime::new().await;
        let executor = executor(&runtime);
        for include_headers in [false, true] {
            let (url, _ready, task) = stalled_server(
                b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nX-Result: waiting\r\n\r\n",
            )
            .await;
            let mut arguments = json!({"url":url,"timeout":1});
            if include_headers {
                arguments["include_headers"] = json!(true);
            }
            let error = fetch(&runtime, &executor, arguments).await.unwrap_err();
            let output = error.into_failure().output.unwrap().value;
            assert_eq!(output["status"], 200);
            assert_eq!(output["diagnostic"]["timeout"]["kind"], "total");
            if include_headers {
                assert_eq!(output["headers"]["x-result"], json!(["waiting"]));
            } else {
                assert!(output.get("headers").is_none());
            }
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[test]
    fn failure_progress_does_not_restore_headers_from_previous_output_by_default() {
        let progress = FetchProgress::new(&args(json!({"url":"https://example.org"})));
        let error = progress.failure(ToolError::with_output(
            "previous failure",
            ToolOutput::new(json!({"headers":{"x-result":["hidden"]}, "detail":"retained"})),
        ));
        let ToolError::FailedWithOutput { output, .. } = error else {
            panic!("expected structured failure");
        };
        assert!(output.value.get("headers").is_none());
        assert_eq!(output.value["detail"], "retained");
    }
}
