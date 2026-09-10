//! Per-operation context retained even when the outer deadline cancels a request.
use std::time::Instant;

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;

use super::diagnostics::{FetchDiagnostic, FetchErrorKind, FetchPhase, FetchTimeoutKind};
use super::{FetchArgs, Redirect, ResponseBody, ToolError, ToolOutput, Url, collect_headers};

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
