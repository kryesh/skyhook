//! Explicit redirect policy: method rewrites, credential isolation, and failure metadata.
use super::diagnostics::{
    DiagnosticMessage, FetchDiagnostic, FetchError, FetchErrorKind, FetchPhase,
};
use reqwest::{Method, header::HeaderMap};
use schemars::JsonSchema;
use serde::Deserialize;

use super::validation::HttpRequestUrl;

#[derive(Debug, Default, Clone, Copy, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum RedirectPolicy {
    #[default]
    Safe,
    Follow,
    Manual,
}

impl RedirectPolicy {
    /// Whether a request with `method` follows a redirect: safe redirects follow
    /// only GET and HEAD.
    pub(super) fn follows(self, method: &Method) -> bool {
        match self {
            Self::Safe => method == Method::GET || method == Method::HEAD,
            Self::Follow => true,
            Self::Manual => false,
        }
    }
}

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

pub(super) fn redirect_error(message: DiagnosticMessage) -> FetchError {
    FetchDiagnostic::classified(
        FetchPhase::Redirect,
        FetchErrorKind::RedirectFailure,
        message,
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::super::{
        tests::{args, executor, fetch, server},
        validation::FetchPlan,
    };
    use super::*;
    use crate::provider::http::transport::tests::reply;
    use serde_json::json;

    #[test]
    fn redirect_methods_and_credential_stripping() {
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
        let a = args(json!({"url":"https://a.example", "headers":{
            "Authorization":"secret", "X-Api-Key":"custom", "Cookie":"secret", "Host":"wrong",
            "Content-Type":"text/plain", "Digest":"secret", "Accept":"text/plain"
        }}));
        let from = HttpRequestUrl::parse(&a.url).unwrap();
        let mut headers = FetchPlan::try_from(a).unwrap().headers;
        redirect_headers(
            &mut headers,
            &from,
            &HttpRequestUrl::parse("https://a.example/next").unwrap(),
            false,
        );
        assert!(headers.contains_key("authorization") && !headers.contains_key("host"));
        redirect_headers(
            &mut headers,
            &from,
            &HttpRequestUrl::parse("https://b.example").unwrap(),
            true,
        );
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

    #[tokio::test]
    async fn safe_manual_follow_cross_origin_credentials_and_limits() {
        let runtime = crate::tests::TestRuntime::new().await;
        let executor = executor(&runtime);
        let payload = json!({"kind":"text","value":"payload"});
        for policy in ["safe", "manual"] {
            let (url, task) =
                server(vec![reply("302 Found", "Location: /next\r\n", "redirect")]).await;
            let arguments = json!({"url":url,"method":"POST","body":payload,"redirects":policy});
            let result = fetch(&runtime, &executor, arguments).await.unwrap();
            assert_eq!(result["status"], 302);
            assert!(result.get("redirects").is_none());
            assert!(task.finish().await[0].ends_with("payload"));
        }
        let (end, end_task) = server(vec![reply(
            "200 OK",
            "Content-Type: text/plain\r\n",
            "done",
        )])
        .await;
        let location = format!("Location: {end}/final\r\n");
        let (start, start_task) = server(vec![reply("303 See Other", &location, "")]).await;
        let arguments = json!({
            "url":start, "method":"POST", "body":payload, "redirects":"follow",
            "auth":{"kind":"bearer","token":"secret"}, "headers":{"Cookie":"private=1","X-Api-Key":"custom-secret"}
        });
        let result = fetch(&runtime, &executor, arguments).await.unwrap();
        let location = format!("{end}/final");
        assert_eq!(
            result["redirects"],
            json!([{"status":303, "location":location, "method":"POST"}])
        );
        assert!(start_task.finish().await[0].contains("authorization: Bearer secret"));
        let request = &end_task.finish().await[0];
        assert!(request.starts_with("GET /final HTTP/1.1"));
        for secret in ["secret", "private=1", "payload", "content-type"] {
            assert!(!request.contains(secret));
        }
        // An exhausted redirect limit fails without retries.
        let (url, task) = server(vec![reply("302 Found", "Location: /again\r\n", "")]).await;
        assert!(
            fetch(&runtime, &executor, json!({"url":url,"max_redirects":0}))
                .await
                .is_err()
        );
        assert_eq!(task.finish().await.len(), 1);
    }
}
