//! Method rewrites and credential isolation across redirects.
use reqwest::{Method, header::HeaderMap};

use crate::op::HttpRequestUrl;

#[derive(Clone, Copy)]
pub(super) struct FollowableRedirectStatus(u16);

impl FollowableRedirectStatus {
    pub(super) fn new(status: u16) -> Option<Self> {
        matches!(status, 301 | 302 | 303 | 307 | 308).then_some(Self(status))
    }

    pub(super) fn get(self) -> u16 {
        self.0
    }

    /// Whether the hop drops the body and rewrites the method to GET.
    pub(super) fn rewrites_to_get(self, method: &Method) -> bool {
        (self.0 == 303 && method != Method::HEAD)
            || (matches!(self.0, 301 | 302) && method == Method::POST)
    }
}

/// Keep only what a hop to `to` may carry from `from`: another origin gets no
/// credentials, and a dropped body takes its framing headers with it.
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

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn redirect_statuses_and_method_rewrites() {
        for (status, method, rewrite) in [
            (301, Method::POST, true),
            (302, Method::POST, true),
            (303, Method::POST, true),
            (307, Method::POST, false),
            (308, Method::POST, false),
            (303, Method::HEAD, false),
            (302, Method::PUT, false),
        ] {
            let followable = FollowableRedirectStatus::new(status).unwrap();
            assert_eq!(
                followable.rewrites_to_get(&method),
                rewrite,
                "{status} {method}"
            );
        }
        for status in [200, 300, 304, 305, 306, 400] {
            assert!(FollowableRedirectStatus::new(status).is_none());
        }
    }

    #[test]
    fn redirect_credential_stripping() {
        let from = HttpRequestUrl::parse("https://a.example").unwrap();
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("authorization", "secret"),
            ("x-api-key", "custom"),
            ("cookie", "secret"),
            ("host", "wrong"),
            ("content-type", "text/plain"),
            ("digest", "secret"),
            ("accept", "text/plain"),
        ] {
            headers.insert(name, HeaderValue::from_static(value));
        }
        let next = HttpRequestUrl::parse("https://a.example/next").unwrap();
        redirect_headers(&mut headers, &from, &next, false);
        assert!(headers.contains_key("authorization") && !headers.contains_key("host"));
        let other = HttpRequestUrl::parse("https://b.example").unwrap();
        redirect_headers(&mut headers, &from, &other, true);
        for name in [
            "authorization",
            "cookie",
            "x-api-key",
            "content-type",
            "digest",
        ] {
            assert!(!headers.contains_key(name));
        }
        assert!(headers.contains_key("accept"));
    }
}
