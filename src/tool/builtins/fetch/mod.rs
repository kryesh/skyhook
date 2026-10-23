//! Bounded HTTP requests. Redirects are deliberately handled here, never by reqwest.
use crate::tool::ToolOptions;
use crate::tool::invocation::{LocalCatalogBuilder, LocalContext, LocalError};
use crate::tool::output::ProducedOutput;
use crate::tool::registry::Invocation;
use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Bounded, TimeoutSecs};
use crate::tool::{
    PathArgument, PathKind, RegistryError, ToolPlacement,
    policy::{Capability, PathAccess, PermissionUse, ResourceId},
};

mod budget;
mod markdown;
mod progress;
mod redirects;
mod request;
mod response;
mod text;
mod tls;
mod validation;

use progress::FetchProgress;
use redirects::RedirectPolicy;
use request::execute;
use validation::FetchPlan;

const MAX_BYTES: u64 = 100 * 1024 * 1024;
const MAX_UPLOAD_BYTES: u64 = MAX_BYTES;
type MaxBytes = Bounded<1, MAX_BYTES>;
type MaxRedirects = Bounded<0, 20>;
fn default_method() -> String {
    "GET".into()
}
const fn default_timeout() -> TimeoutSecs {
    Bounded::new::<30>()
}
const fn default_connect_timeout() -> TimeoutSecs {
    Bounded::new::<10>()
}
const fn default_max_bytes() -> MaxBytes {
    Bounded::new::<{ 10 * 1024 * 1024 }>()
}
const fn default_redirects() -> MaxRedirects {
    Bounded::new::<5>()
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct FetchArgs {
    /// HTTP(S), without URL credentials.
    url: String,
    #[serde(default = "default_method")]
    method: String,
    /// Appends to the URL query.
    #[serde(default)]
    query: Vec<(String, String)>,
    /// Request headers to send.
    #[serde(default)]
    headers: BTreeMap<String, HeaderValues>,
    /// Include response headers in the result.
    #[serde(default)]
    include_headers: bool,
    body: Option<RequestBody>,
    auth: Option<Auth>,
    #[serde(default)]
    response_format: ResponseFormat,
    save_to: Option<String>,
    #[serde(default)]
    overwrite: bool,
    /// Seconds.
    #[serde(default = "default_timeout")]
    timeout: TimeoutSecs,
    /// Seconds.
    #[serde(default = "default_connect_timeout")]
    connect_timeout: TimeoutSecs,
    /// Decoded response bytes; excess fails.
    #[serde(default = "default_max_bytes")]
    max_bytes: MaxBytes,
    #[serde(default)]
    redirects: RedirectPolicy,
    #[serde(default = "default_redirects")]
    max_redirects: MaxRedirects,
    proxy: Option<String>,
    /// Skip TLS certificate verification.
    #[serde(default)]
    insecure: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(untagged)]
pub(super) enum HeaderValues {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum RequestBody {
    Text { value: String },
    Json { value: Value },
    Form { fields: Vec<(String, String)> },
    Base64 { value: String },
    File { path: String },
}

#[derive(Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Auth {
    Basic {
        username: String,
        #[serde(default)]
        password: String,
    },
    Bearer {
        token: String,
    },
}

#[derive(Debug, Default, Clone, Copy, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum ResponseFormat {
    /// HTML as markdown, JSON as a value, other text as text, binary as base64.
    #[default]
    Auto,
    /// As auto, preferring HTML from the server and reading HTML served as any type.
    Readable,
    /// The decoded body as sent.
    Text,
    /// The raw bytes as base64.
    Base64,
}

impl ResponseFormat {
    /// The `Accept` sent when the caller sets none. HTML comes first, as a browser
    /// asks, since some sites answer anything else with an empty shell or a
    /// non-HTML variant; `auto` still ranks JSON above other types for APIs.
    fn accept(self) -> Option<&'static str> {
        match self {
            Self::Auto => Some("text/html,application/xhtml+xml,application/json;q=0.9,*/*;q=0.8"),
            Self::Readable => Some("text/html,application/xhtml+xml;q=0.9,*/*;q=0.8"),
            Self::Text | Self::Base64 => None,
        }
    }
}

#[serde_with::skip_serializing_none]
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct FetchOutput {
    status: u16,
    headers: Option<BTreeMap<String, Vec<String>>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    redirects: Vec<Redirect>,
    body: ResponseBody,
    /// Decoded entity bytes received (before extraction).
    size: u64,
    /// Milliseconds.
    duration: u64,
}
#[derive(Debug, Clone, Serialize, JsonSchema)]
struct Redirect {
    status: u16,
    location: String,
    method: String,
}
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ResponseBody {
    Text {
        text: String,
    },
    /// HTML read as markdown.
    Page(text::Page),
    /// HTML that could not be read as markdown, with its decoded source.
    Unreadable {
        reason: text::Unreadable,
        text: String,
    },
    /// A textual body that holds JSON, read as it.
    Json {
        value: serde_json::Value,
    },
    Base64 {
        data: String,
    },
    File {
        path: String,
        bytes: u64,
    },
    Empty,
}

pub(super) fn register(builder: &mut LocalCatalogBuilder) -> Result<(), RegistryError> {
    builder.register_checked(
        "fetch",
        "HTTP(S) from the selected target. HTTP error statuses are normal results. response_format auto and readable return HTML as readable markdown, or its source and a reason when unreadable; the status is never hidden. Safe redirects follow GET/HEAD only; no HTTPS downgrade or retries.",
        ToolOptions::new(vec![Capability::Network])
            .result::<FetchOutput>()
            .argument_permissions(|location, plan: &FetchPlan| {
                Ok(vec![PermissionUse::new(Capability::Network, ResourceId::network(&location.target, plan.url.origin().as_str()))])
            })
            .argument_paths(|plan| {
                let mut paths = Vec::new();
                if let validation::OutputPlan::Download { destination, .. } = &mut plan.output {
                    paths.push(PathArgument::new(destination, PathAccess::Write, PathKind::Writable).exact());
                }
                if let Some(RequestBody::File { path }) = &mut plan.body {
                    paths.push(PathArgument::new(path, PathAccess::Read, PathKind::Existing).exact());
                }
                paths
            })
            .placement(ToolPlacement::TargetedWorkspace).background().named(),
        |args: FetchArgs| FetchPlan::try_from(args),
        |plan| Invocation::new(|context| fetch(context, plan)),
    )?;
    Ok(())
}

async fn fetch(context: LocalContext, plan: FetchPlan) -> Result<ProducedOutput, LocalError> {
    let timeout = plan.client.timeout;
    let mut progress = FetchProgress::new();
    let outcome = tokio::select! {
        biased;
        () = context.cancelled() => Ok(Err(LocalError::cancelled())),
        result = tokio::time::timeout(timeout, execute(&context, plan, &mut progress)) => result,
    };
    match outcome {
        Ok(Ok(result)) => crate::tool::registry::serialized(result),
        Ok(Err(error)) => Err(progress.failure(error)),
        Err(_) => Err(progress.timeout(timeout.as_secs())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::http::transport::tests::{Plan, Server, reply};
    use crate::tool::ToolRegistryBuilder;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::json;

    pub(super) fn args(value: Value) -> FetchArgs {
        serde_json::from_value(value).unwrap()
    }

    pub(super) fn executor(
        runtime: &crate::tests::TestRuntime,
    ) -> crate::tool::executor::ToolExecutor {
        let mut builder = ToolRegistryBuilder::default();
        builder.register_local(register).unwrap();
        runtime.executor(builder)
    }

    pub(super) async fn fetch(
        runtime: &crate::tests::TestRuntime,
        executor: &crate::tool::executor::ToolExecutor,
        arguments: Value,
    ) -> Result<Value, crate::tool::ToolError> {
        let result = executor.run_host(&runtime.agent, "fetch", arguments).await;
        result.map(|result| result.output.value)
    }

    /// A server answering each connection with the next reply, and its root URL.
    pub(super) async fn server(replies: Vec<impl Into<Vec<u8>>>) -> (String, Server) {
        let server = Server::start(replies.into_iter().map(Plan::reply).collect()).await;
        (server.root(), server)
    }

    #[tokio::test]
    async fn json_bodies_are_read_as_values_unless_text_is_requested() {
        let runtime = crate::tests::TestRuntime::new().await;
        let executor = executor(&runtime);
        let (json, html) = (
            "Content-Type: application/json\r\n",
            "Content-Type: text/html\r\n",
        );
        for (format, headers, body, expected) in [
            (
                "auto",
                json,
                "{\"a\": [1, 2]}\n",
                json!({"kind":"json","value":{"a":[1,2]}}),
            ),
            // JSON labelled as HTML is still JSON.
            (
                "auto",
                html,
                "{\"a\":1}\n{\"a\":2}\n",
                json!({"kind":"json","value":[{"a":1},{"a":2}]}),
            ),
            (
                "readable",
                json,
                "{\"a\":1}\n{\"a\":2}\n",
                json!({"kind":"json","value":[{"a":1},{"a":2}]}),
            ),
            (
                "text",
                json,
                "{\"a\": [1, 2]}\n",
                json!({"kind":"text","text":"{\"a\": [1, 2]}\n"}),
            ),
        ] {
            let (url, task) = server(vec![reply("200 OK", headers, body)]).await;
            let arguments = json!({"url": url, "response_format": format});
            let output = fetch(&runtime, &executor, arguments).await.unwrap();
            task.finish().await;
            assert_eq!(output["body"], expected, "{format}: {body}");
        }
    }

    #[tokio::test]
    async fn response_body_payloads_are_truncated_in_the_view() {
        let runtime = crate::tests::TestRuntime::new().await;
        let executor = executor(&runtime);
        let payload = "body".repeat(crate::job::output::CONTENT_BYTES / 4 + 1);
        let header = "h".repeat(3000);
        let headers = format!("Content-Type: text/plain\r\nX-Details: {header}\r\n");
        for (format, field, expected) in [
            ("text", "text", payload.clone()),
            ("base64", "data", STANDARD.encode(payload.as_bytes())),
        ] {
            let (url, task) = server(vec![reply("200 OK", &headers, &payload)]).await;
            let arguments = json!({"url":url, "response_format":format, "include_headers":true});
            let call = executor
                .run_model(&runtime.agent, "fetch", arguments)
                .await
                .unwrap();
            task.finish().await;
            let view = call.output.value;
            let result = &view["result"];
            assert_eq!(view["state"], "completed");
            assert_eq!(result["status"], 200);
            assert_eq!(result["size"], payload.len());
            assert_eq!(result["headers"]["x-details"][0], header);
            assert_eq!(result["body"]["kind"], format);
            let prefix = result["body"][field].as_str().unwrap();
            assert!(prefix.len() < expected.len() && expected.starts_with(prefix));
            let markers = view["presentation"]["truncated"].as_array().unwrap();
            assert_eq!(markers[0]["field"], format!("/result/body/{field}"));
        }

        // Small payloads keep their original shape and need no truncation marker.
        let (url, task) = server(vec![reply("200 OK", "Content-Type: text/plain\r\n", "ok")]).await;
        let call = executor
            .run_model(&runtime.agent, "fetch", json!({"url":url}))
            .await
            .unwrap();
        task.finish().await;
        assert_eq!(
            call.output.value["result"]["body"],
            json!({"kind":"text", "text":"ok"})
        );
        assert!(call.output.value["result"].get("headers").is_none());
        assert!(call.output.value["meta"].is_null());
        assert!(call.output.value["presentation"].is_null());
    }
}
