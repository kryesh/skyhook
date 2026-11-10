//! Validate URLs, request framing, authentication, and bounded fetch options.
use crate::error::AdmissionError;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use http::{
    Method,
    header::{ACCEPT, HeaderMap, HeaderName, HeaderValue},
};

use std::num::NonZeroU64;

use super::{Auth, FetchArgs, Output, SaveTo, request::encode};
use skyhook_worker::op::{self, HttpRequestUrl};

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
        for value in values.as_slice() {
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
            Auth::Basic { username, password } => format!(
                "Basic {}",
                STANDARD.encode(format!("{username}:{password}"))
            ),
            Auth::Bearer { token } => format!("Bearer {token}"),
        };
        let mut value = HeaderValue::from_str(&value).map_err(AdmissionError::invalid_arguments)?;
        value.set_sensitive(true);
        headers.insert("authorization", value);
    }
    Ok(headers)
}

/// A fetch as admitted: the exchange its target runs, its response body going where
/// its caller chose.
pub type FetchPlan = op::Fetch<Output>;

impl TryFrom<FetchArgs> for FetchPlan {
    type Error = AdmissionError;
    fn try_from(args: FetchArgs) -> Result<Self, AdmissionError> {
        // Ranged arguments failed at parsing; the rest fail as URL, method, headers, body.
        let mut url =
            HttpRequestUrl::parse(&args.url).map_err(AdmissionError::invalid_arguments)?;
        let method = Method::from_bytes(args.method.as_bytes())
            .map_err(AdmissionError::invalid_arguments)?;
        let mut headers = request_headers(&args)?;
        url.append_query(&args.query);
        let body = args.body.map(encode).transpose()?;
        let output = match args.save_to {
            Some(SaveTo { path, overwrite }) => Output::Saved { path, overwrite },
            None => Output::Inline(args.response_format),
        };
        if let Output::Inline(format) = output
            && let Some(accept) = format.accept()
        {
            headers
                .entry(ACCEPT)
                .or_insert(HeaderValue::from_static(accept));
        }
        let seconds = |seconds: NonZeroU64| std::time::Duration::from_secs(seconds.get());
        Ok(Self {
            url,
            method: op::HttpMethod(method),
            headers: op::Headers(headers),
            body,
            client: op::ClientIdentity {
                connect_timeout: crate::builtins::duration(args.connect_timeout),
                proxy: args.proxy,
                insecure: args.insecure,
            },
            timeout: args.timeout.map(seconds),
            max_bytes: args.max_bytes.map(NonZeroU64::get),
            max_redirects: args.max_redirects.into(),
            redirects: args.redirects,
            include_headers: args.include_headers,
            output,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ResponseFormat, tests::args};
    use super::*;
    use crate::arguments::deserialize_arguments;
    use schemars::schema_for;
    use serde_json::json;
    use skyhook_worker::diagnostic::Subject;

    fn plan(value: serde_json::Value) -> Result<FetchPlan, AdmissionError> {
        FetchPlan::try_from(args(value))
    }

    #[test]
    fn defaults_and_validation() {
        let a = args(json!({"url":"https://example.org"}));
        let timeouts = (a.timeout.map(u64::from), u64::from(a.connect_timeout));
        assert_eq!((a.method.as_str(), timeouts), ("GET", (Some(30), 10)));
        assert!(!a.insecure && !a.include_headers);
        assert_eq!(a.redirects, op::RedirectPolicy::Safe);
        FetchPlan::try_from(a).unwrap();
        // Formats that extract HTML send an Accept, unless the caller chose one.
        for (value, accept) in [
            (json!({}), ResponseFormat::Auto.accept()),
            (json!({"response_format":"text"}), None),
            (json!({"save_to":{"path":"out"}}), None),
            (
                json!({"headers":{"Accept":"application/json"}}),
                Some("application/json"),
            ),
        ] {
            let mut value = value;
            value["url"] = json!("https://example.org");
            let plan = plan(value).unwrap();
            let sent = (plan.headers.0)
                .get(ACCEPT)
                .map(|value| value.to_str().unwrap());
            assert_eq!(sent, accept);
        }
        for value in [
            json!({"url":"file:///etc/passwd"}),
            json!({"url":"https://user:secret@example.org"}),
            json!({"url":"http://example.org","method":"bad method"}),
            json!({"url":"http://example.org","headers":{"a":"bad\r\nheader"}}),
            json!({"url":"http://example.org","headers":{"content-length":"2"}}),
            json!({"url":"http://example.org","headers":{"Transfer-Encoding":"chunked"}}),
            json!({"url":"http://example.org","auth":{"kind":"bearer","token":"a"},"headers":{"Authorization":"b"}}),
        ] {
            assert!(plan(value.clone()).is_err(), "{value}");
        }
        plan(json!({"url":"http://example.org","method":"PROPFIND"})).unwrap();
        // A body is encoded as it is admitted, so one that cannot be is refused before approval.
        let body = json!({"kind":"base64","value":"Ynl0ZXM"});
        let refused = plan(json!({"url":"http://example.org","body":body}))
            .err()
            .unwrap();
        assert_eq!(
            refused
                .diagnostic()
                .render(crate::diagnostic::TargetView::Hidden),
            "prepare HTTP request body failed: invalid base64 body"
        );
        // The retained plan keeps the appended query, sanitized origin and
        // header sensitivity proofs.
        let plan = plan(json!({
            "url":"https://example.org/p?x=first#discard", "query":[["x","second"]],
            "headers":{"X-Multi":["one","two"]}, "auth":{"kind":"bearer","token":"credential"}
        }))
        .unwrap();
        let (url, headers) = (&plan.url, &plan.headers.0);
        assert_eq!(url.as_str(), "https://example.org/p?x=first&x=second");
        assert!(url.join("ftp://example.org/file").is_err());
        assert_eq!(headers.get_all("x-multi").iter().count(), 2);
        assert!(headers.get("authorization").unwrap().is_sensitive());
    }

    /// An explicit null lifts the deadline and leaves the response unlimited.
    #[test]
    fn schema_bounds_limits() {
        let schema = serde_json::to_value(schema_for!(FetchArgs)).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let unlimited =
            args(json!({"url":"https://example.org", "timeout":null, "max_bytes":null}));
        assert_eq!((unlimited.timeout, unlimited.max_bytes), (None, None));
        for (key, max) in [("connect_timeout", 3600u64), ("max_redirects", 20)] {
            for n in [0, 1, max, max + 1] {
                let input = json!({"url":"https://example.org", key: n});
                let expected = n <= max && (n > 0 || key == "max_redirects");
                assert_eq!(validator.is_valid(&input), expected, "schema {input}");
                let admitted =
                    deserialize_arguments::<FetchArgs>(&schema, input.as_object().unwrap());
                match admitted {
                    Ok(_) => assert!(expected, "{input}"),
                    Err(error) => assert_eq!(
                        (expected, error.diagnostic().context.subject),
                        (false, Subject::argument([key]))
                    ),
                }
            }
        }
    }
}
