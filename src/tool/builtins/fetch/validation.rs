//! Validate URLs, request framing, authentication, and bounded fetch options.
use crate::tool::invocation::AdmissionError;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::{
    Method, Url,
    header::{ACCEPT, HeaderMap, HeaderName, HeaderValue},
};

use super::{Auth, FetchArgs, HeaderValues, LocalError, ResponseFormat};

/// Request endpoints are not proxy endpoints: credentials are never accepted.
#[derive(Clone, Debug)]
pub(super) struct HttpRequestUrl(Url);
impl HttpRequestUrl {
    pub(super) fn parse(value: &str) -> Result<Self, AdmissionError> {
        let url = Url::parse(value)
            .map_err(|_| AdmissionError::invalid_arguments("invalid absolute URL"))?;
        Self::admit(url)
    }
    fn admit(mut url: Url) -> Result<Self, AdmissionError> {
        check_url(&url)?;
        url.set_fragment(None);
        Ok(Self(url))
    }
    pub(super) fn join(&self, location: &str) -> Result<Self, LocalError> {
        let url = self
            .0
            .join(location)
            .map_err(|_| LocalError::failed("invalid redirect URL"))?;
        Self::admit(url).map_err(|_| {
            LocalError::failed("redirect URL must be HTTP(S) without embedded credentials")
        })
    }
    pub(super) fn as_str(&self) -> &str {
        self.0.as_str()
    }
    pub(super) fn url(&self) -> &Url {
        &self.0
    }
    pub(super) fn origin(&self) -> SanitizedOrigin {
        SanitizedOrigin(self.0.origin().ascii_serialization())
    }
    fn append_query(&mut self, query: &[(String, String)]) {
        if !query.is_empty() {
            self.0
                .query_pairs_mut()
                .extend_pairs(query.iter().map(|(k, v)| (k, v)));
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SanitizedOrigin(String);
impl SanitizedOrigin {
    pub(super) fn as_str(&self) -> &str {
        &self.0
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
    pub(super) client: ClientIdentity,
}

/// The settings an HTTP client is built from, which the calls sharing it have in common.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct ClientIdentity {
    pub(super) connect_timeout: Duration,
    pub(super) proxy: Option<String>,
    pub(super) insecure: bool,
}

/// A download keeps the bytes as sent, so it has no response format.
#[derive(Debug)]
pub(super) enum OutputPlan {
    Inline(ResponseFormat),
    Download {
        destination: String,
        overwrite: bool,
    },
}

/// The retained owner of validated execution inputs, separate from wire/schema DTOs.
/// Checked values are constructed only while admitting the execution plan.
pub(super) struct FetchPlan {
    pub(super) url: HttpRequestUrl,
    pub(super) method: Method,
    pub(super) headers: HeaderMap,
    pub(super) client: ClientSettings,
    pub(super) max_bytes: u64,
    pub(super) max_redirects: u64,
    pub(super) redirects: super::redirects::RedirectPolicy,
    pub(super) output: OutputPlan,
    pub(super) body: Option<super::RequestBody>,
    pub(super) include_headers: bool,
}
impl TryFrom<FetchArgs> for FetchPlan {
    type Error = AdmissionError;
    fn try_from(args: FetchArgs) -> Result<Self, AdmissionError> {
        // Ranged arguments failed at parsing; the rest fail as URL, method, options, headers.
        let mut url = HttpRequestUrl::parse(&args.url)?;
        let method = Method::from_bytes(args.method.as_bytes())
            .map_err(AdmissionError::invalid_arguments)?;
        validate_options(&args)?;
        let mut headers = request_headers(&args)?;
        url.append_query(&args.query);
        let output = match args.save_to {
            Some(destination) => OutputPlan::Download {
                destination,
                overwrite: args.overwrite,
            },
            None => OutputPlan::Inline(args.response_format),
        };
        if let OutputPlan::Inline(format) = &output
            && let Some(accept) = format.accept()
        {
            headers
                .entry(ACCEPT)
                .or_insert(HeaderValue::from_static(accept));
        }
        Ok(Self {
            url,
            method,
            headers,
            client: ClientSettings {
                timeout: args.timeout.duration(),
                client: ClientIdentity {
                    connect_timeout: args.connect_timeout.duration(),
                    proxy: args.proxy,
                    insecure: args.insecure,
                },
            },
            max_bytes: args.max_bytes.into(),
            max_redirects: args.max_redirects.into(),
            redirects: args.redirects,
            output,
            body: args.body,
            include_headers: args.include_headers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{MAX_BYTES, redirects::RedirectPolicy, tests::args};
    use super::*;
    use crate::tool::diagnostic::{Subject, deserialize_arguments};
    use schemars::schema_for;
    use serde_json::json;

    fn plan(value: serde_json::Value) -> Result<FetchPlan, impl std::fmt::Debug> {
        FetchPlan::try_from(args(value))
    }

    #[test]
    fn defaults_and_validation() {
        let a = args(json!({"url":"https://example.org"}));
        let timeouts = (u64::from(a.timeout), u64::from(a.connect_timeout));
        assert_eq!((a.method.as_str(), timeouts), ("GET", (30, 10)));
        assert!(!a.insecure && !a.include_headers);
        assert_eq!(a.redirects, RedirectPolicy::Safe);
        FetchPlan::try_from(a).unwrap();
        // Formats that extract HTML send an Accept, unless the caller chose one.
        for (value, accept) in [
            (json!({}), ResponseFormat::Auto.accept()),
            (json!({"response_format":"text"}), None),
            (json!({"save_to":"out"}), None),
            (
                json!({"headers":{"Accept":"application/json"}}),
                Some("application/json"),
            ),
        ] {
            let mut value = value;
            value["url"] = json!("https://example.org");
            let plan = plan(value).unwrap();
            let sent = plan
                .headers
                .get(ACCEPT)
                .map(|value| value.to_str().unwrap());
            assert_eq!(sent, accept);
        }
        for value in [
            json!({"url":"file:///etc/passwd"}),
            json!({"url":"https://user:secret@example.org"}),
            json!({"url":"http://example.org","method":"bad method"}),
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
            "headers":{"X-Multi":["one","two"]}, "auth":{"kind":"bearer","token":"credential"}
        }))
        .unwrap();
        assert_eq!(plan.url.as_str(), "https://example.org/p?x=first&x=second");
        assert!(plan.url.join("ftp://example.org/file").is_err());
        assert_eq!(plan.headers.get_all("x-multi").iter().count(), 2);
        assert!(plan.headers.get("authorization").unwrap().is_sensitive());
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
                let admitted = deserialize_arguments::<FetchArgs>(input.as_object().unwrap());
                match admitted {
                    Ok(_) => assert!(expected, "{input}"),
                    Err(error) => assert_eq!(
                        (expected, error.diagnostic().context.subject),
                        (false, Subject::argument([key]))
                    ),
                }
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
