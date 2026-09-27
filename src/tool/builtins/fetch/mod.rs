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

mod diagnostics;
mod progress;
mod redirects;
mod request;
mod response;
mod text;
mod tls;
mod validation;

use progress::{FetchFailureOutput, FetchProgress};
use redirects::RedirectPolicy;
use request::execute;
use validation::FetchPlan;

#[derive(JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
enum FetchResultSchema {
    Response(FetchOutput),
    Failure(FetchFailureOutput),
}

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
    /// Extract readable HTML text.
    #[serde(default)]
    text: bool,
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
    #[default]
    Auto,
    Text,
    Base64,
}

#[serde_with::skip_serializing_none]
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct FetchOutput {
    status: u16,
    #[schemars(with = "BTreeMap<String, Vec<String>>")]
    headers: Option<BTreeMap<String, Vec<String>>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    redirects: Vec<Redirect>,
    body: ResponseBody,
    /// Decoded entity bytes received (before text extraction).
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
#[serde_with::skip_serializing_none]
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ResponseBody {
    Text {
        text: String,
        #[schemars(with = "text::ExtractionMetadata")]
        metadata: Option<text::ExtractionMetadata>,
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
        "HTTP(S) from the selected target. HTTP error statuses are normal results. Safe redirects follow GET/HEAD only; no HTTPS downgrade or retries.",
        ToolOptions::new(vec![Capability::Network])
            .result::<FetchResultSchema>()
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
    let mut progress = FetchProgress::new(
        &plan.url,
        &plan.method,
        plan.include_headers,
        plan.client.connect_timeout,
        plan.client.proxy_origin(),
    );
    let outcome = tokio::select! {
        biased;
        () = context.cancelled() => Ok(Err(diagnostics::FetchError::Passthrough(LocalError::cancelled()))),
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
    use crate::job::CancellationToken;
    use crate::provider::http::transport::tests::{Plan, Server, reply};
    use crate::tool::ToolRegistryBuilder;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::json;

    pub(super) fn args(value: Value) -> FetchArgs {
        serde_json::from_value(value).unwrap()
    }

    /// An admitted plan and the progress a real fetch would begin with.
    pub(super) fn progress_for(value: Value) -> (FetchPlan, FetchProgress) {
        let plan = FetchPlan::try_from(args(value)).unwrap();
        let client = &plan.client;
        let progress = FetchProgress::new(
            &plan.url,
            &plan.method,
            plan.include_headers,
            client.connect_timeout,
            client.proxy_origin(),
        );
        (plan, progress)
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
        let json = "Content-Type: application/json\r\n";
        for (format, body, expected) in [
            (
                "auto",
                "{\"a\": [1, 2]}\n",
                json!({"kind":"json","value":{"a":[1,2]}}),
            ),
            (
                "auto",
                "{\"a\":1}\n{\"a\":2}\n",
                json!({"kind":"json","value":[{"a":1},{"a":2}]}),
            ),
            (
                "text",
                "{\"a\": [1, 2]}\n",
                json!({"kind":"text","text":"{\"a\": [1, 2]}\n"}),
            ),
            // A number a double cannot hold as written stays in its text.
            (
                "auto",
                "{\"id\":9007199254740993}",
                json!({"kind":"text","text":"{\"id\":9007199254740993}"}),
            ),
        ] {
            let (url, task) = server(vec![reply("200 OK", json, body)]).await;
            let arguments = json!({"url": url, "response_format": format});
            let output = fetch(&runtime, &executor, arguments).await.unwrap();
            task.finish().await;
            assert_eq!(output["body"], expected, "{format}: {body}");
        }
    }

    #[tokio::test]
    async fn response_body_payloads_are_truncated_but_full_output_is_retrievable() {
        use crate::job::JobOutputQuery;

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
            let pointer = format!("/result/body/{field}");
            let marker = json!([{"field":pointer, "next_start":1, "next_offset":prefix.len()}]);
            let markers = view["presentation"]["truncated"].as_array().unwrap();
            let projected: Vec<_> = markers
                .iter()
                .map(|m| json!({"field":m["field"], "next_start":m["next_start"], "next_offset":m["next_offset"]}))
                .collect();
            assert_eq!(json!(projected), marker);

            // Paging from the start, and from where the preview stops, reads the
            // body exactly.
            let read = async |start: usize, offset: usize| {
                let mut query = JobOutputQuery::new(call.job);
                query.field = Some(pointer.parse().unwrap());
                (query.start, query.offset) = (Some(start), Some(offset));
                let (mut text, mut pages) = (String::new(), 0);
                loop {
                    let page = runtime
                        .jobs
                        .inspect_output(
                            query.clone(),
                            CancellationToken::new(),
                            &Default::default(),
                        )
                        .await
                        .unwrap();
                    let preview = &page["presentation"]["preview"];
                    let lines = preview["lines"].as_array().unwrap();
                    assert_eq!(lines.len(), 1);
                    text.push_str(lines[0].as_str().unwrap());
                    pages += 1;
                    let Some(start) = preview["next_start"].as_u64() else {
                        return (text, pages);
                    };
                    query.start = Some(start as usize);
                    query.offset = Some(preview["next_offset"].as_u64().unwrap() as usize);
                }
            };
            let (full, pages) = read(1, 0).await;
            assert!(pages > 1);
            assert_eq!(full, expected);
            assert_eq!(read(1, prefix.len()).await.0, expected[prefix.len()..]);
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
        assert!(call.output.value["meta"].is_null());
        assert!(call.output.value["presentation"].is_null());
    }
}
