//! Validate URLs, request framing, authentication, and bounded fetch options.
use crate::tool::invocation::AdmissionError;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::{
    Method, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};

use super::diagnostics::DiagnosticMessage;
use super::{Auth, FetchArgs, HeaderValues, MAX_BYTES, ResponseFormat};

/// Request endpoints are not proxy endpoints: credentials are never accepted.
#[derive(Clone, Debug)]
pub(in crate::tool::builtins) struct HttpRequestUrl(Url);
impl HttpRequestUrl {
    pub(in crate::tool::builtins) fn parse(value: &str) -> Result<Self, AdmissionError> {
        let url = Url::parse(value)
            .map_err(|_| AdmissionError::invalid_arguments("invalid absolute URL"))?;
        Self::admit(url)
    }
    fn admit(mut url: Url) -> Result<Self, AdmissionError> {
        check_url(&url)?;
        url.set_fragment(None);
        Ok(Self(url))
    }
    /// URL syntax versus scheme/credentials keep distinct redirect diagnostics.
    pub(super) fn join(&self, location: &str) -> Result<Self, DiagnosticMessage> {
        let url = self
            .0
            .join(location)
            .map_err(|_| DiagnosticMessage::InvalidRedirectUrl)?;
        Self::admit(url).map_err(|_| DiagnosticMessage::RedirectUrlNotHttp)
    }
    pub(in crate::tool::builtins) fn as_str(&self) -> &str {
        self.0.as_str()
    }
    pub(super) fn url(&self) -> &Url {
        &self.0
    }
    pub(super) fn origin(&self) -> SanitizedOrigin {
        SanitizedOrigin::from_url(&self.0)
    }
    fn append_query(&mut self, query: &[(String, String)]) {
        if !query.is_empty() {
            self.0
                .query_pairs_mut()
                .extend_pairs(query.iter().map(|(k, v)| (k, v)));
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(transparent)]
pub(super) struct SanitizedOrigin(String);
impl SanitizedOrigin {
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
    pub(super) fn from_url(url: &Url) -> Self {
        Self(url.origin().ascii_serialization())
    }
    // Proxy schemes and credentials follow reqwest's distinct endpoint contract.
    pub(super) fn proxy(value: &str) -> Self {
        Url::parse(value)
            .map(|url| Self::from_url(&url))
            .unwrap_or_else(|_| Self("unknown origin".into()))
    }
}

fn check_url(url: &Url) -> Result<(), AdmissionError> {
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(AdmissionError::invalid_arguments(
            "URL must use HTTP or HTTPS and have a host",
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(AdmissionError::invalid_arguments(
            "embedded URL credentials are not supported; use auth",
        ));
    }
    Ok(())
}
fn validate_options(args: &FetchArgs) -> Result<(), AdmissionError> {
    if args.timeout == 0
        || args.timeout > 3600
        || args.connect_timeout == 0
        || args.connect_timeout > 3600
    {
        return Err(AdmissionError::invalid_arguments(
            "timeouts must be between 1 and 3600 seconds",
        ));
    }
    if args.max_bytes == 0 || args.max_bytes > MAX_BYTES {
        return Err(AdmissionError::invalid_arguments(
            "max_bytes must be between 1 and 104857600",
        ));
    }
    if args.max_redirects > 20 {
        return Err(AdmissionError::invalid_arguments(
            "max_redirects must not exceed 20",
        ));
    }
    if args.text && (args.save_to.is_some() || args.response_format == ResponseFormat::Base64) {
        return Err(AdmissionError::invalid_arguments(
            "text conflicts with save_to and response_format base64",
        ));
    }
    if args.overwrite && args.save_to.is_none() {
        return Err(AdmissionError::invalid_arguments(
            "overwrite requires save_to",
        ));
    }
    Ok(())
}

fn request_headers(args: &FetchArgs) -> Result<HeaderMap, AdmissionError> {
    let mut headers = HeaderMap::new();
    for (key, values) in &args.headers {
        let name =
            HeaderName::from_bytes(key.as_bytes()).map_err(AdmissionError::invalid_arguments)?;
        // Let the HTTP implementation compute framing; conflicting framing is unsafe.
        if matches!(name.as_str(), "content-length" | "transfer-encoding") {
            return Err(AdmissionError::invalid_arguments(
                "content-length and transfer-encoding are managed by fetch",
            ));
        }
        let values = match values {
            HeaderValues::One(value) => std::slice::from_ref(value),
            HeaderValues::Many(values) => values.as_slice(),
        };
        for value in values {
            headers.append(
                name.clone(),
                HeaderValue::from_str(value).map_err(AdmissionError::invalid_arguments)?,
            );
        }
    }
    if let Some(auth) = &args.auth {
        if headers.contains_key("authorization") {
            return Err(AdmissionError::invalid_arguments(
                "auth conflicts with the authorization header",
            ));
        }
        let value = match auth {
            Auth::Basic { username, password } => {
                if username.contains(':') {
                    return Err(AdmissionError::invalid_arguments(
                        "basic auth username must not contain ':'",
                    ));
                }
                format!(
                    "Basic {}",
                    STANDARD.encode(format!("{username}:{password}"))
                )
            }
            Auth::Bearer { token } => format!("Bearer {token}"),
        };
        let mut value = HeaderValue::from_str(&value).map_err(AdmissionError::invalid_arguments)?;
        value.set_sensitive(true);
        headers.insert("authorization", value);
    }
    Ok(headers)
}

pub(super) struct ClientSettings {
    pub(super) timeout: Duration,
    pub(super) connect_timeout: Duration,
    pub(super) proxy: Option<String>,
    pub(super) insecure: bool,
}
impl ClientSettings {
    pub(super) fn proxy_origin(&self) -> Option<SanitizedOrigin> {
        self.proxy.as_deref().map(SanitizedOrigin::proxy)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InlineMode {
    Auto,
    Text,
    Base64,
    ExtractText,
}
/// A download cannot be paired with inline decoding or extraction options.
#[derive(Debug)]
pub(super) enum OutputPlan {
    Inline(InlineMode),
    Download {
        destination: std::path::PathBuf,
        overwrite: bool,
    },
}

pub(super) fn redirect_headers(
    headers: &mut HeaderMap,
    from: &HttpRequestUrl,
    to: &HttpRequestUrl,
    drop_body: bool,
) {
    headers.remove("host");
    if from.origin() != to.origin() {
        *headers = headers
            .iter()
            .filter(|(name, _)| {
                matches!(
                    name.as_str(),
                    "accept"
                        | "accept-language"
                        | "accept-encoding"
                        | "user-agent"
                        | "content-type"
                )
            })
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
    }
    if drop_body {
        for name in [
            "content-type",
            "content-encoding",
            "content-language",
            "content-location",
            "digest",
        ] {
            headers.remove(name);
        }
    }
}

/// The retained owner of validated execution inputs, separate from wire/schema DTOs.
/// Checked values are constructed only while admitting the execution plan.
pub(super) struct FetchPlan {
    pub(super) url: HttpRequestUrl,
    pub(super) method: Method,
    pub(super) headers: HeaderMap,
    pub(super) client: ClientSettings,
    pub(super) max_bytes: u64,
    pub(super) max_redirects: usize,
    pub(super) redirects: super::RedirectPolicy,
    pub(super) output: OutputPlan,
    pub(super) body: Option<super::RequestBody>,
    pub(super) include_headers: bool,
}
/// Check everything a plan cannot take from its arguments unchanged: the URL,
/// method and headers it returns, and the options.
pub(super) fn check_request(
    args: &FetchArgs,
) -> Result<(HttpRequestUrl, Method, HeaderMap), AdmissionError> {
    // Preserve the admission error ordering of URL, method, options and headers.
    let url = HttpRequestUrl::parse(&args.url)?;
    let method =
        Method::from_bytes(args.method.as_bytes()).map_err(AdmissionError::invalid_arguments)?;
    validate_options(args)?;
    let headers = request_headers(args)?;
    Ok((url, method, headers))
}

impl TryFrom<FetchArgs> for FetchPlan {
    type Error = AdmissionError;
    fn try_from(args: FetchArgs) -> Result<Self, AdmissionError> {
        let (mut url, method, headers) = check_request(&args)?;
        url.append_query(&args.query);
        let output = match args.save_to {
            Some(destination) => OutputPlan::Download {
                destination: destination.into(),
                overwrite: args.overwrite,
            },
            None => OutputPlan::Inline(if args.text {
                InlineMode::ExtractText
            } else {
                match args.response_format {
                    ResponseFormat::Auto => InlineMode::Auto,
                    ResponseFormat::Text => InlineMode::Text,
                    ResponseFormat::Base64 => InlineMode::Base64,
                }
            }),
        };
        Ok(Self {
            url,
            method,
            headers,
            client: ClientSettings {
                timeout: Duration::from_secs(args.timeout),
                connect_timeout: Duration::from_secs(args.connect_timeout),
                proxy: args.proxy,
                insecure: args.insecure,
            },
            max_bytes: args.max_bytes,
            max_redirects: args.max_redirects,
            redirects: args.redirects,
            output,
            body: args.body,
            include_headers: args.include_headers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{DEFAULT_MAX_BYTES, RedirectPolicy, tests::args};
    use super::*;
    use schemars::schema_for;
    use serde_json::json;

    fn plan(value: serde_json::Value) -> Result<FetchPlan, impl std::fmt::Debug> {
        FetchPlan::try_from(args(value))
    }

    #[test]
    fn defaults_and_validation() {
        let a = args(json!({"url":"https://example.org"}));
        assert_eq!(
            (a.method.as_str(), a.timeout, a.connect_timeout),
            ("GET", 30, 10)
        );
        assert_eq!((a.max_bytes, a.max_redirects), (DEFAULT_MAX_BYTES, 5));
        assert!(!a.insecure && !a.include_headers);
        assert_eq!(a.redirects, RedirectPolicy::Safe);
        FetchPlan::try_from(a).unwrap();
        for value in [
            json!({"url":"file:///etc/passwd"}),
            json!({"url":"https://user:secret@example.org"}),
            json!({"url":"http://example.org","method":"bad method"}),
            json!({"url":"http://example.org","max_bytes":0}),
            json!({"url":"http://example.org","max_bytes":MAX_BYTES+1}),
            json!({"url":"http://example.org","timeout":0}),
            json!({"url":"http://example.org","max_redirects":21}),
            json!({"url":"http://example.org","text":true,"save_to":"out"}),
            json!({"url":"http://example.org","text":true,"response_format":"base64"}),
            json!({"url":"http://example.org","overwrite":true}),
            json!({"url":"http://example.org","headers":{"a":"bad\r\nheader"}}),
            json!({"url":"http://example.org","headers":{"content-length":"2"}}),
            json!({"url":"http://example.org","headers":{"Transfer-Encoding":"chunked"}}),
            json!({"url":"http://example.org","auth":{"kind":"bearer","token":"a"},"headers":{"Authorization":"b"}}),
        ] {
            assert!(plan(value.clone()).is_err(), "{value}");
        }
        plan(json!({"url":"http://example.org","method":"PROPFIND"})).unwrap();
        // The retained plan keeps the appended query, sanitized origin and
        // header sensitivity proofs.
        let plan = plan(json!({
            "url":"https://example.org/p?x=first#discard", "query":[["x","second"]],
            "headers":{"X-Multi":["one","two"]}, "auth":{"kind":"bearer","token":"credential"},
            "text":true
        }))
        .unwrap();
        assert_eq!(plan.url.as_str(), "https://example.org/p?x=first&x=second");
        assert!(matches!(
            plan.url.join("ftp://example.org/file"),
            Err(DiagnosticMessage::RedirectUrlNotHttp)
        ));
        assert_eq!(plan.headers.get_all("x-multi").iter().count(), 2);
        assert!(plan.headers.get("authorization").unwrap().is_sensitive());
        assert!(matches!(
            plan.output,
            OutputPlan::Inline(InlineMode::ExtractText)
        ));
    }

    #[test]
    fn schema_bounds_limits_and_rejects_unknown_fields_and_body_kinds() {
        let schema = serde_json::to_value(schema_for!(FetchArgs)).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        for (key, max) in [
            ("timeout", 3600u64),
            ("connect_timeout", 3600),
            ("max_bytes", MAX_BYTES),
            ("max_redirects", 20),
        ] {
            for n in [0, 1, max, max + 1] {
                let input = json!({"url":"https://example.org", key: n});
                let expected = n <= max && (n > 0 || key == "max_redirects");
                assert_eq!(validator.is_valid(&input), expected, "schema {input}");
            }
        }
        for input in [
            json!({"url":"https://example.org", "unknown":true}),
            json!({"url":"https://example.org", "body":{"kind":"multipart", "parts":[]}}),
        ] {
            assert!(!validator.is_valid(&input), "{input}");
            assert!(serde_json::from_value::<FetchArgs>(input).is_err());
        }
    }
}
