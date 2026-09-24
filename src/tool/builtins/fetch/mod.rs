//! Bounded HTTP requests. Redirects are deliberately handled here, never by reqwest.
use crate::tool::ToolOptions;
use crate::tool::diagnostic::deserialize_arguments;
use crate::tool::invocation::{LocalCatalogBuilder, LocalContext, LocalError};
use crate::tool::output::ProducedOutput;
use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::fetch_text;
use crate::tool::{
    PathArgument, PathKind, RegistryError, ToolPlacement,
    policy::{Capability, PathAccess, PermissionUse, ResourceId},
};

mod diagnostics;
mod progress;
mod redirects;
mod request;
mod response;
mod tls;
mod validation;

use progress::{FetchFailureOutput, FetchProgress};
use request::execute;
use validation::FetchPlan;
pub(super) use validation::HttpRequestUrl;

#[derive(JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
enum FetchResultSchema {
    Response(FetchOutput),
    Failure(FetchFailureOutput),
}

const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024;
const MAX_BYTES: u64 = 100 * 1024 * 1024;
const MAX_UPLOAD_BYTES: u64 = MAX_BYTES;
fn default_method() -> String {
    "GET".into()
}
const fn default_timeout() -> u64 {
    30
}
const fn default_connect_timeout() -> u64 {
    10
}
const fn default_max_bytes() -> u64 {
    DEFAULT_MAX_BYTES
}
const fn default_redirects() -> usize {
    5
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct FetchArgs {
    /// HTTP(S), without URL credentials.
    pub url: String,
    #[serde(default = "default_method")]
    pub method: String,
    /// Appends to the URL query.
    #[serde(default)]
    pub query: Vec<(String, String)>,
    /// Request headers to send.
    #[serde(default)]
    pub headers: BTreeMap<String, HeaderValues>,
    /// Include response headers in the result (null by default).
    #[serde(default)]
    pub include_headers: bool,
    pub body: Option<RequestBody>,
    pub auth: Option<Auth>,
    /// Extract readable HTML text.
    #[serde(default)]
    pub text: bool,
    #[serde(default)]
    pub response_format: ResponseFormat,
    pub save_to: Option<String>,
    #[serde(default)]
    pub overwrite: bool,
    /// Seconds.
    #[serde(default = "default_timeout")]
    #[schemars(range(min = 1, max = 3600))]
    pub timeout: u64,
    /// Seconds.
    #[serde(default = "default_connect_timeout")]
    #[schemars(range(min = 1, max = 3600))]
    pub connect_timeout: u64,
    /// Decoded response bytes; excess fails.
    #[serde(default = "default_max_bytes")]
    #[schemars(range(min = 1, max = 104857600))]
    pub max_bytes: u64,
    #[serde(default)]
    pub redirects: RedirectPolicy,
    #[serde(default = "default_redirects")]
    #[schemars(range(min = 0, max = 20))]
    pub max_redirects: usize,
    pub proxy: Option<String>,
    /// Skip TLS certificate verification.
    #[serde(default)]
    pub insecure: bool,
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
#[derive(Debug, Default, Clone, Copy, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum RedirectPolicy {
    #[default]
    Safe,
    Follow,
    Manual,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct FetchOutput {
    status: u16,
    ok: bool,
    url: String,
    method: String,
    /// Response headers, or null when include_headers is false.
    headers: Option<BTreeMap<String, Vec<String>>>,
    redirects: Vec<Redirect>,
    body: ResponseBody,
    /// Decoded entity bytes received (before text extraction).
    received_bytes: u64,
    elapsed_ms: u64,
}
#[derive(Debug, Clone, Serialize, JsonSchema)]
struct Redirect {
    status: u16,
    url: String,
    location: String,
    method: String,
}
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ResponseBody {
    Text {
        #[schemars(extend("x-skyhook-truncatable" = true))]
        text: String,
        metadata: Option<fetch_text::ExtractionMetadata>,
    },
    Base64 {
        #[schemars(extend("x-skyhook-truncatable" = true))]
        data: String,
    },
    File {
        path: String,
        bytes: u64,
    },
    Empty,
}

pub(super) fn register(builder: &mut LocalCatalogBuilder) -> Result<(), RegistryError> {
    builder.register_product::<FetchArgs, FetchResultSchema, _, _>(
        "fetch",
        "HTTP(S) from the selected target. HTTP error statuses are normal results. Safe redirects follow GET/HEAD only; no HTTPS downgrade or retries.",
        ToolOptions::new(vec![Capability::Network])
            .argument_validator(|arguments| {
                let args: FetchArgs = deserialize_arguments(arguments.clone())?;
                FetchPlan::try_from(args).map(drop)
            })
            // These callbacks still parse wire data to discover permissions and paths;
            // they do not construct or discard the retained domain plan.
            .argument_permissions(|location, arguments| {
                let args: FetchArgs = deserialize_arguments(arguments.clone())?;
                let url = HttpRequestUrl::parse(&args.url)?;
                Ok(vec![PermissionUse::new(Capability::Network, ResourceId::network(&location.target, url.origin().as_str()))])
            })
            .argument_paths(|arguments| {
                let args: FetchArgs = deserialize_arguments(arguments.clone())?;
                let mut paths = Vec::new();
                if args.save_to.is_some() { paths.push(PathArgument::pointer("/save_to", PathAccess::Write, PathKind::Writable)); }
                if matches!(args.body, Some(RequestBody::File { .. })) {
                    paths.push(PathArgument::pointer("/body/path", PathAccess::Read, PathKind::Existing));
                }
                Ok(paths)
            })
            .placement(ToolPlacement::TargetedWorkspace).background().named(),
        |context, args| async move {
            let plan = FetchPlan::try_from(args)?;
            let timeout = plan.client.timeout;
            let mut progress = FetchProgress::new(&plan.url, &plan.method, plan.include_headers,
                plan.client.connect_timeout, plan.client.proxy_origin());
            let outcome = tokio::select! {
                biased;
                () = context.cancelled() => Ok(Err(diagnostics::FetchError::Passthrough(LocalError::cancelled()))),
                result = tokio::time::timeout(timeout, execute(&context, plan, &mut progress)) => result,
            };
            match outcome {
                Ok(Ok(result)) => Ok(ProducedOutput::new(serde_json::to_value(result).map_err(LocalError::failed)?)),
                Ok(Err(error)) => Err(progress.failure(error)),
                Err(_) => Err(progress.timeout(timeout.as_secs())),
            }
        },
    )?;
    Ok(())
}

fn invalid(error: impl std::fmt::Display) -> LocalError {
    LocalError::invalid_arguments(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::CancellationToken;
    use crate::tool::ToolRegistryBuilder;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::{net::TcpListener, task::JoinHandle};

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
            false,
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
    ) -> Result<Value, crate::tool::executor::ExecutionError> {
        let result = executor.run_host(&runtime.agent, "fetch", arguments).await;
        result.map(|result| result.output.value)
    }

    pub(super) async fn read_request(stream: &mut (impl tokio::io::AsyncRead + Unpin)) -> String {
        let mut request = Vec::new();
        let mut read_more = async |request: &mut Vec<u8>| {
            let mut buffer = [0u8; 4096];
            let count = stream.read(&mut buffer).await.unwrap();
            assert!(count > 0);
            request.extend_from_slice(&buffer[..count]);
        };
        let header_end = loop {
            read_more(&mut request).await;
            if let Some(end) = request.windows(4).position(|v| v == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
        let length: usize = headers
            .lines()
            .find_map(|line| {
                line.strip_prefix("content-length:")
                    .map(|n| n.trim().parse().unwrap())
            })
            .unwrap_or(0);
        while request.len() < header_end + length {
            read_more(&mut request).await;
        }
        String::from_utf8(request).unwrap()
    }

    async fn listen() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        (listener, url)
    }

    pub(super) async fn server(
        responses: Vec<impl AsRef<[u8]> + Send + 'static>,
    ) -> (String, JoinHandle<Vec<String>>) {
        let (listener, url) = listen().await;
        // No deadline of its own: the client's timeout and the runner's bound it.
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                requests.push(read_request(&mut socket).await);
                socket.write_all(response.as_ref()).await.unwrap();
                // A client that has already closed makes this fail harmlessly.
                let _ = socket.shutdown().await;
            }
            requests
        });
        (url, task)
    }

    /// Send an incomplete response and keep the socket open until the client closes.
    pub(super) async fn stalled_server(
        response: &'static [u8],
    ) -> (String, tokio::sync::oneshot::Receiver<()>, JoinHandle<()>) {
        let (listener, url) = listen().await;
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            socket.write_all(response).await.unwrap();
            let _ = ready_tx.send(());
            let _ = socket.read(&mut [0]).await;
        });
        (url, ready_rx, task)
    }

    pub(super) fn response(status: &str, headers: &str, body: &str) -> String {
        let length = body.len();
        format!(
            "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {length}\r\n{headers}\r\n{body}"
        )
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
            let (url, task) = server(vec![response("200 OK", &headers, &payload)]).await;
            let arguments = json!({"url":url, "response_format":format, "include_headers":true});
            let call = executor
                .run_model(&runtime.agent, "fetch", arguments)
                .await
                .unwrap();
            task.await.unwrap();
            let view = call.output.value;
            let result = &view["result"];
            assert_eq!(view["state"], "completed");
            assert_eq!(
                (&result["status"], &result["url"]),
                (&json!(200), &json!(format!("{url}/")))
            );
            assert_eq!(result["received_bytes"], payload.len());
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

            let mut query = JobOutputQuery::new(call.job);
            query.field = Some(pointer.parse().unwrap());
            let mut full = String::new();
            let mut pages = 0;
            loop {
                let page = runtime
                    .jobs
                    .inspect_output(query.clone(), CancellationToken::new(), &Default::default())
                    .await
                    .unwrap();
                let preview = &page["presentation"]["preview"];
                let lines = preview["lines"].as_array().unwrap();
                assert_eq!(lines.len(), 1);
                full.push_str(lines[0].as_str().unwrap());
                pages += 1;
                let Some(start) = preview["next_start"].as_u64() else {
                    break;
                };
                query.start = Some(start as usize);
                query.offset = Some(preview["next_offset"].as_u64().unwrap() as usize);
            }
            assert!(pages > 1);
            assert_eq!(full, expected);
            (query.start, query.offset) = (Some(1), Some(prefix.len()));
            let remainder = runtime
                .jobs
                .inspect_output(query, CancellationToken::new(), &Default::default())
                .await
                .unwrap();
            assert_eq!(
                remainder["presentation"]["preview"]["lines"],
                json!([&expected[prefix.len()..]])
            );
        }

        // Small payloads keep their original shape and need no truncation marker.
        let (url, task) = server(vec![response(
            "200 OK",
            "Content-Type: text/plain\r\n",
            "ok",
        )])
        .await;
        let call = executor
            .run_model(&runtime.agent, "fetch", json!({"url":url}))
            .await
            .unwrap();
        task.await.unwrap();
        assert_eq!(
            call.output.value["result"]["body"],
            json!({"kind":"text", "text":"ok", "metadata":null})
        );
        assert!(call.output.value["meta"].is_null());
        assert!(call.output.value["presentation"].is_null());
    }
}
