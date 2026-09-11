//! Explicit redirect policy: method rewrites, credential isolation, and failure metadata.
use std::time::Instant;

use reqwest::{Method, Url, header::HeaderMap};

use super::diagnostics::{FetchDiagnostic, FetchErrorKind, FetchPhase};
use super::response::collect_headers;
use super::{FetchOutput, Redirect, ResponseBody, ToolError, ToolOutput};

pub(super) fn redirect_method(status: u16, method: &Method) -> (Method, bool) {
    if (status == 303 && method != Method::HEAD)
        || (matches!(status, 301 | 302) && method == Method::POST)
    {
        (Method::GET, true)
    } else {
        (method.clone(), false)
    }
}
pub(super) fn strip_redirect_headers(
    headers: &mut HeaderMap,
    from: &Url,
    to: &Url,
    drop_body: bool,
) {
    headers.remove("host");
    if from.origin() != to.origin() {
        // Custom headers often contain API keys. Only carry demonstrably non-secret
        // negotiation headers to a different origin; never guess credential names.
        let safe: HeaderMap = headers
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
        *headers = safe;
    }
    if drop_body {
        for name in [
            "content-type",
            "content-length",
            "transfer-encoding",
            "content-encoding",
            "content-language",
            "content-location",
            "digest",
        ] {
            headers.remove(name);
        }
    }
}
pub(super) fn redirect_error(
    message: &'static str,
    response: &reqwest::Response,
    method: &Method,
    redirects: &[Redirect],
    started: Instant,
    include_headers: bool,
) -> ToolError {
    let output = FetchOutput {
        status: response.status().as_u16(),
        ok: response.status().is_success(),
        url: response.url().to_string(),
        method: method.to_string(),
        headers: include_headers.then(|| collect_headers(response.headers())),
        redirects: redirects.to_vec(),
        body: ResponseBody::Empty,
        received_bytes: 0,
        elapsed_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
    };
    let mut value = serde_json::to_value(output).expect("fetch output serializes");
    let mut diagnostic =
        FetchDiagnostic::new(FetchPhase::Redirect, FetchErrorKind::RedirectFailure);
    diagnostic.message = message.to_owned();
    value["diagnostic"] = serde_json::to_value(diagnostic).expect("fetch diagnostic serializes");
    ToolError::with_output(message, ToolOutput::new(value))
}

#[cfg(test)]
mod tests {
    use super::super::{
        tests::{args, executor, fetch, response, server},
        validation::{parse_url, request_headers},
    };
    use super::*;
    use serde_json::json;

    #[test]
    fn redirect_methods_and_credential_stripping() {
        for status in [301, 302, 303] {
            assert_eq!(redirect_method(status, &Method::POST), (Method::GET, true));
        }
        for status in [307, 308] {
            assert_eq!(
                redirect_method(status, &Method::POST),
                (Method::POST, false)
            );
        }
        assert_eq!(redirect_method(303, &Method::HEAD), (Method::HEAD, false));
        assert_eq!(redirect_method(302, &Method::PUT), (Method::PUT, false));
        let a = args(
            json!({"url":"https://a.example", "headers":{"Authorization":"secret", "X-Api-Key":"custom", "Cookie":"secret", "Host":"wrong", "Content-Type":"text/plain", "Accept":"text/plain"}}),
        );
        let mut headers = request_headers(&a).unwrap();
        strip_redirect_headers(
            &mut headers,
            &parse_url(&a.url).unwrap(),
            &parse_url("https://a.example/next").unwrap(),
            false,
        );
        assert!(headers.contains_key("authorization"));
        assert!(!headers.contains_key("host"));
        strip_redirect_headers(
            &mut headers,
            &parse_url(&a.url).unwrap(),
            &parse_url("https://b.example").unwrap(),
            true,
        );
        for name in ["authorization", "cookie", "x-api-key", "content-type"] {
            assert!(!headers.contains_key(name));
        }
        assert!(headers.contains_key("accept"));
    }

    #[tokio::test]
    async fn safe_manual_follow_and_cross_origin_credentials() {
        let runtime = crate::tests::TestRuntime::new().await;
        let executor = executor(&runtime);
        for policy in ["safe", "manual"] {
            let (url, task) = server(vec![response(
                "302 Found",
                "Location: /next\r\n",
                "redirect",
            )])
            .await;
            let result = fetch(&runtime, &executor, json!({"url":url,"method":"POST","body":{"kind":"text","value":"payload"},"redirects":policy})).await.unwrap();
            assert_eq!(result["status"], 302);
            assert_eq!(result["redirects"], json!([]));
            assert!(task.await.unwrap()[0].ends_with("payload"));
        }
        let (end, end_task) = server(vec![response(
            "200 OK",
            "Content-Type: text/plain\r\n",
            "done",
        )])
        .await;
        let (start, start_task) = server(vec![response(
            "303 See Other",
            &format!("Location: {end}/final\r\n"),
            "",
        )])
        .await;
        let result = fetch(&runtime, &executor, json!({"url":start,"method":"POST","body":{"kind":"text","value":"payload"},"redirects":"follow","auth":{"kind":"bearer","token":"secret"},"headers":{"Cookie":"private=1","X-Api-Key":"custom-secret"}})).await.unwrap();
        assert_eq!(result["method"], "GET");
        assert_eq!(result["redirects"].as_array().unwrap().len(), 1);
        assert!(start_task.await.unwrap()[0].contains("authorization: Bearer secret"));
        let request = &end_task.await.unwrap()[0];
        assert!(request.starts_with("GET /final HTTP/1.1"));
        for secret in ["secret", "private=1", "payload", "content-type"] {
            assert!(!request.contains(secret));
        }
    }

    #[tokio::test]
    async fn redirect_limit_fails_without_retries() {
        let runtime = crate::tests::TestRuntime::new().await;
        let executor = executor(&runtime);
        let (url, task) = server(vec![response("302 Found", "Location: /again\r\n", "")]).await;
        assert!(
            fetch(&runtime, &executor, json!({"url":url,"max_redirects":0}))
                .await
                .is_err()
        );
        assert_eq!(task.await.unwrap().len(), 1);
    }
}
