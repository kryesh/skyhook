//! Per-operation context retained even when the outer deadline cancels a request.
use std::time::{Duration, Instant};

use reqwest::Method;
use schemars::JsonSchema;
use serde::Serialize;

use super::diagnostics::{FetchDiagnostic, FetchError, FetchPhase};
use super::response::collect_headers;
use super::validation::{HttpRequestUrl, SanitizedOrigin};
use super::{LocalError, ProducedOutput, ResponseBody};

/// Failures add diagnostics and only include HTTP response fields when a response
/// actually arrived. Response headers also require explicit opt-in.
#[derive(Serialize, JsonSchema)]
pub(super) struct FetchFailureOutput {
    method: String,
    origin: SanitizedOrigin,
    elapsed_ms: u64,
    received_bytes: u64,
    redirects: Vec<FailureRedirect>,
    diagnostic: FetchDiagnostic,
    /// Explicitly configured proxy only; null does not rule out an environment proxy.
    proxy_origin: Option<SanitizedOrigin>,
    #[serde(flatten)]
    response: Option<FailureResponse>,
}

/// Success redirects intentionally carry full URLs. Failure redirects cannot:
/// these are distinct types and both endpoints require sanitized origin evidence.
#[derive(Clone, Serialize, JsonSchema)]
struct FailureRedirect {
    status: u16,
    url: SanitizedOrigin,
    location: SanitizedOrigin,
    method: String,
}

#[derive(Clone, Serialize, JsonSchema)]
struct FailureResponse {
    status: u16,
    ok: bool,
    url: SanitizedOrigin,
    /// Response headers, or null when include_headers is false.
    headers: Option<std::collections::BTreeMap<String, Vec<String>>>,
    body: ResponseBody,
}

pub(super) struct FetchProgress {
    started: Instant,
    pub(super) phase: FetchPhase,
    /// The response accounting owner records observed decoded bytes, including the
    /// chunk that crossed the limit. Resetting belongs only to begin_request.
    pub(super) received_bytes: u64,
    method: String,
    origin: SanitizedOrigin,
    redirects: Vec<FailureRedirect>,
    response: Option<FailureResponse>,
    proxy_origin: Option<SanitizedOrigin>,
    connect_timeout_ms: u64,
    include_headers: bool,
}

impl FetchProgress {
    pub fn new(
        url: &HttpRequestUrl,
        method: &Method,
        include_headers: bool,
        connect_timeout: Duration,
        proxy_origin: Option<SanitizedOrigin>,
    ) -> Self {
        Self {
            started: Instant::now(),
            phase: FetchPhase::ClientPreparation,
            received_bytes: 0,
            method: method.to_string(),
            origin: url.origin(),
            redirects: Vec::new(),
            response: None,
            proxy_origin,
            connect_timeout_ms: duration_ms(connect_timeout),
            include_headers,
        }
    }

    pub fn elapsed_ms(&self) -> u64 {
        duration_ms(self.started.elapsed())
    }

    pub fn begin_request(&mut self, url: &HttpRequestUrl, method: &Method) {
        self.origin = url.origin();
        self.method = method.to_string();
        self.response = None;
        self.received_bytes = 0;
        self.phase = FetchPhase::Request;
    }

    pub fn response(&mut self, response: &reqwest::Response) {
        self.response = Some(FailureResponse {
            status: response.status().as_u16(),
            ok: response.status().is_success(),
            // Never copy secret query parameters into failure URL fields.
            url: SanitizedOrigin::from_url(response.url()),
            headers: self
                .include_headers
                .then(|| collect_headers(response.headers())),
            body: ResponseBody::Empty,
        });
    }

    pub fn redirect(
        &mut self,
        status: u16,
        from: &HttpRequestUrl,
        to: &HttpRequestUrl,
        method: &Method,
    ) {
        self.redirects.push(FailureRedirect {
            status,
            url: from.origin(),
            location: to.origin(),
            method: method.to_string(),
        });
    }

    pub fn timeout(&self, limit_secs: u64) -> LocalError {
        self.diagnostic_failure(FetchDiagnostic::total_timeout(
            self.phase,
            limit_secs.saturating_mul(1000),
        ))
    }

    pub fn failure(&self, error: FetchError) -> LocalError {
        match error.into_diagnostic() {
            Ok(diagnostic) => self.diagnostic_failure(diagnostic),
            // Cancellation, denial and argument contracts retain their original metadata.
            Err(error) => error,
        }
    }

    fn diagnostic_failure(&self, diagnostic: FetchDiagnostic) -> LocalError {
        let diagnostic = diagnostic.with_connect_limit(self.connect_timeout_ms);
        let elapsed_ms = self.elapsed_ms();
        let mut summary = format!(
            "HTTP {} {}: {} ({} ms)",
            self.method,
            self.origin.as_str(),
            diagnostic.message(),
            elapsed_ms
        );
        if let Some((platform, code)) = diagnostic.os_code() {
            summary.push_str(&format!("; OS error {code} on {platform}"));
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
        // One typed projection, with no JSON recovery, arbitrary output merge, or
        // images inherited from a nested failure. Headers can only enter via response().
        let value = serde_json::to_value(report).expect("fetch diagnostics serialize");
        LocalError::with_output(summary, ProducedOutput::new(value))
    }
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::super::tests::{executor, fetch, progress_for, response, server, stalled_server};
    use super::*;
    use serde_json::{Value, json};
    use std::time::Duration;
    use tokio::net::TcpListener;

    fn with_headers(mut arguments: Value, include_headers: Option<bool>) -> Value {
        if let Some(include) = include_headers {
            arguments["include_headers"] = json!(include);
        }
        arguments
    }

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
                let headers = format!(
                    "Content-Type: application/pdf\r\nX-Result: one\r\nX-Result: two\r\n{extra}"
                );
                let (url, task) = server(vec![response(status, &headers, "body")]).await;
                let mut arguments = with_headers(options, include_headers);
                arguments["url"] = json!(url);
                let result = fetch(&runtime, &executor, arguments).await;
                let output = if let Some(kind) = error_kind {
                    let output = result.unwrap_err().into_failure().output.unwrap().value;
                    assert_eq!(output["diagnostic"]["error_kind"], kind);
                    if kind == "extraction_failure" {
                        assert_eq!(
                            (&output["method"], &output["received_bytes"]),
                            (&json!("POST"), &json!(4))
                        );
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
                    assert_eq!(output["headers"], Value::Null, "{output}");
                }
                task.await.unwrap();
            }
        }
    }

    /// Transport failures never have response headers; a body deadline after
    /// the response head still reports status and opt-in headers.
    #[tokio::test]
    async fn transport_failures_and_body_deadlines_report_headers_only_when_received() {
        let runtime = crate::tests::TestRuntime::new().await;
        let executor = executor(&runtime);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let refused = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        // Each case waits out a one-second body deadline, so they run together.
        let case = async |include_headers: Option<bool>| {
            let arguments = with_headers(json!({"url":refused}), include_headers);
            let error = fetch(&runtime, &executor, arguments).await.unwrap_err();
            let output = error.into_failure().output.unwrap().value;
            assert_eq!(output["diagnostic"]["error_kind"], "connection_refused");
            assert!(output.get("status").is_none() && output.get("headers").is_none());

            let head = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nX-Result: waiting\r\n\r\n";
            let (url, _ready, task) = stalled_server(head).await;
            let arguments = with_headers(json!({"url":url,"timeout":1}), include_headers);
            let error = fetch(&runtime, &executor, arguments).await.unwrap_err();
            let output = error.into_failure().output.unwrap().value;
            assert_eq!(output["status"], 200);
            assert_eq!(output["diagnostic"]["timeout"]["kind"], "total");
            if include_headers == Some(true) {
                assert_eq!(output["headers"]["x-result"], json!(["waiting"]));
            } else {
                assert_eq!(output["headers"], Value::Null);
            }
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap();
        };
        tokio::join!(case(None), case(Some(false)), case(Some(true)));
    }

    #[test]
    fn forged_diagnostic_and_payload_are_not_recovered_from_legacy_errors() {
        let progress =
            progress_for(json!({"url":"https://example.org/private?token=secret#secret"})).1;
        let forged = ProducedOutput::new(json!({
            "diagnostic":{"phase":"connect", "error_kind":"timeout", "message":"secret forged message",
                "timeout":{"kind":"total", "limit_ms":1}},
            "headers":{"authorization":["secret"]}, "url":"https://secret:secret@host/secret",
            "body":{"kind":"text", "text":"secret"}, "extra":"secret"
        }));
        let legacy = LocalError::with_output("secret error display", forged);
        let error = progress.failure(FetchError::from_tool_error(legacy, FetchPhase::Extraction));
        let LocalError::FailedWithOutput { message, output } = error else {
            panic!("structured failure")
        };
        assert!(!message.contains("secret"));
        assert!(!output.value.to_string().contains("secret"));
        assert!(output.images.is_empty());
        assert_eq!(output.value["origin"], "https://example.org");
        let diagnostic = &output.value["diagnostic"];
        assert_eq!(
            (&diagnostic["phase"], &diagnostic["error_kind"]),
            (&json!("extraction"), &json!("extraction_failure"))
        );
        assert_eq!(diagnostic["timeout"], Value::Null);
        assert_eq!(diagnostic["os_error"], Value::Null);
        assert_eq!(output.value["proxy_origin"], Value::Null);
        // These belong to the absent flattened response variant, rather than
        // nullable fields of the failure itself.
        for absent in ["headers", "body"] {
            assert!(output.value.get(absent).is_none(), "{absent}");
        }
        // Cancellation and denial keep their original boundary contracts instead.
        let admit = |error| {
            progress.failure(FetchError::from_tool_error(
                error,
                FetchPhase::Authorization,
            ))
        };
        assert!(matches!(
            admit(LocalError::Cancelled),
            LocalError::Cancelled
        ));
        assert!(
            matches!(admit(LocalError::Denied("permission metadata".into())), LocalError::Denied(value) if value == "permission metadata")
        );
    }

    #[test]
    fn request_transition_resets_only_per_response_state_and_sanitizes_hops() {
        let from = HttpRequestUrl::parse("https://example.org/private?token=secret").unwrap();
        let to =
            HttpRequestUrl::parse("https://other.example/private?token=secret#secret").unwrap();
        let (_, mut progress) = progress_for(json!({
            "url":from.as_str(), "proxy":"http://user:secret@proxy.example:8080/path?secret#secret"
        }));
        progress.phase = FetchPhase::ResponseBody;
        progress.received_bytes = 100;
        progress.redirect(302, &from, &to, &Method::GET);
        progress.begin_request(&to, &Method::POST);
        assert_eq!(
            (progress.phase, progress.received_bytes),
            (FetchPhase::Request, 0)
        );
        let LocalError::FailedWithOutput { message, output } = progress.timeout(7) else {
            panic!("structured failure")
        };
        assert!(!message.contains("secret"));
        assert!(!output.value.to_string().contains("secret"));
        assert_eq!(
            (&output.value["method"], &output.value["origin"]),
            (&json!("POST"), &json!("https://other.example"))
        );
        assert_eq!(
            output.value["redirects"],
            json!([{"status":302,"url":"https://example.org","location":"https://other.example","method":"GET"}])
        );
        assert_eq!(output.value["proxy_origin"], "http://proxy.example:8080");
    }
}
