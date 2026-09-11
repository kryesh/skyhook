//! Validate URLs, request framing, authentication, and bounded fetch options.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::{
    Method, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};

use super::{Auth, FetchArgs, HeaderValues, MAX_BYTES, ResponseFormat, ToolError, invalid};

pub(super) fn parse_url(value: &str) -> Result<Url, ToolError> {
    let url = Url::parse(value).map_err(|_| invalid("invalid absolute URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(invalid("URL must use HTTP or HTTPS and have a host"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid(
            "embedded URL credentials are not supported; use auth",
        ));
    }
    Ok(url)
}
pub(super) fn validate(args: &FetchArgs) -> Result<(), ToolError> {
    parse_url(&args.url)?;
    Method::from_bytes(args.method.as_bytes()).map_err(invalid)?;
    if args.timeout == 0
        || args.timeout > 3600
        || args.connect_timeout == 0
        || args.connect_timeout > 3600
    {
        return Err(invalid("timeouts must be between 1 and 3600 seconds"));
    }
    if args.max_bytes == 0 || args.max_bytes > MAX_BYTES {
        return Err(invalid("max_bytes must be between 1 and 104857600"));
    }
    if args.max_redirects > 20 {
        return Err(invalid("max_redirects must not exceed 20"));
    }
    if args.text && (args.save_to.is_some() || args.response_format == ResponseFormat::Base64) {
        return Err(invalid(
            "text conflicts with save_to and response_format base64",
        ));
    }
    if args.overwrite && args.save_to.is_none() {
        return Err(invalid("overwrite requires save_to"));
    }
    request_headers(args)?;
    Ok(())
}

pub(super) fn request_headers(args: &FetchArgs) -> Result<HeaderMap, ToolError> {
    let mut headers = HeaderMap::new();
    for (key, values) in &args.headers {
        let name = HeaderName::from_bytes(key.as_bytes()).map_err(invalid)?;
        // Let the HTTP implementation compute framing; conflicting framing is unsafe.
        if matches!(name.as_str(), "content-length" | "transfer-encoding") {
            return Err(invalid(
                "content-length and transfer-encoding are managed by fetch",
            ));
        }
        let values = match values {
            HeaderValues::One(value) => std::slice::from_ref(value),
            HeaderValues::Many(values) => values.as_slice(),
        };
        for value in values {
            headers.append(name.clone(), HeaderValue::from_str(value).map_err(invalid)?);
        }
    }
    if let Some(auth) = &args.auth {
        if headers.contains_key("authorization") {
            return Err(invalid("auth conflicts with the authorization header"));
        }
        let value = match auth {
            Auth::Basic { username, password } => {
                if username.contains(':') {
                    return Err(invalid("basic auth username must not contain ':'"));
                }
                format!(
                    "Basic {}",
                    STANDARD.encode(format!("{username}:{password}"))
                )
            }
            Auth::Bearer { token } => format!("Bearer {token}"),
        };
        let mut value = HeaderValue::from_str(&value).map_err(invalid)?;
        value.set_sensitive(true);
        headers.insert("authorization", value);
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::super::{DEFAULT_MAX_BYTES, RedirectPolicy, tests::args};
    use super::*;
    use schemars::schema_for;
    use serde_json::json;

    #[test]
    fn defaults_and_validation() {
        let a = args(json!({"url":"https://example.org"}));
        validate(&a).unwrap();
        assert_eq!(a.method, "GET");
        assert_eq!(a.timeout, 30);
        assert_eq!(a.connect_timeout, 10);
        assert_eq!(a.max_bytes, DEFAULT_MAX_BYTES);
        assert_eq!(a.max_redirects, 5);
        assert!(!a.insecure);
        assert!(!a.include_headers);
        assert_eq!(a.redirects, RedirectPolicy::Safe);
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
            json!({"url":"http://example.org","headers":{"a":"bad\r\nheader"}}),
            json!({"url":"http://example.org","headers":{"content-length":"2"}}),
            json!({"url":"http://example.org","auth":{"kind":"bearer","token":"a"},"headers":{"Authorization":"b"}}),
        ] {
            assert!(validate(&args(value.clone())).is_err(), "{value}");
        }
        validate(&args(
            json!({"url":"http://example.org","method":"PROPFIND"}),
        ))
        .unwrap();
    }

    #[test]
    fn body_and_query_schemas_match_argument_deserialization() {
        let schema = serde_json::to_value(schema_for!(FetchArgs)).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        for body in [
            json!({"kind":"text", "value":"text"}),
            json!({"kind":"form", "fields":[["key", "value"]]}),
            json!({"kind":"base64", "value":"YQ=="}),
            json!({"kind":"file", "path":"upload.txt"}),
        ] {
            let input = json!({"url":"https://example.org", "body":body});
            assert!(validator.is_valid(&input), "{input}");
            assert!(serde_json::from_value::<FetchArgs>(input).is_ok());
        }
        for value in [
            json!(null),
            json!(true),
            json!(42),
            json!(1.5),
            json!("text"),
            json!([null, false, {"nested": [1, 2]}]),
            json!({"nested": {"key": "value"}}),
        ] {
            let input = json!({"url":"https://example.org", "body":{"kind":"json", "value":value}});
            assert!(validator.is_valid(&input), "{input}");
            assert!(serde_json::from_value::<FetchArgs>(input).is_ok());
        }
        let input = json!({"url":"https://example.org", "query":[["key", "one"], ["key", "two"]]});
        assert!(validator.is_valid(&input));
        assert_eq!(
            args(input).query,
            vec![("key".into(), "one".into()), ("key".into(), "two".into())]
        );
        for query in [
            json!({"key":"value"}),
            json!(["key", "value"]),
            json!([["key"]]),
            json!([["key", "value", "extra"]]),
            json!([[1, "value"]]),
            json!([["key", false]]),
        ] {
            let input = json!({"url":"https://example.org", "query":query});
            assert!(!validator.is_valid(&input), "{input}");
            assert!(serde_json::from_value::<FetchArgs>(input).is_err());
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
