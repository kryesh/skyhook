//! HTTP requests.
use crate::output::ProducedOutput;
use crate::spec::TargetTool;
use crate::{
    error::{AdmissionError, CallError},
    spec::{Call, ended},
};
use std::{collections::BTreeMap, num::NonZeroU64};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Bounded, OneOrMany, TimeoutSecs};
use crate::path::{PathAccess, PathArgument};
use skyhook_worker::op::PathKind;

mod entity;
mod html;
mod progress;
mod request;
mod response;
mod validation;

use progress::FetchProgress;
use response::response_body;
use skyhook_worker::diagnostic::{Operation, Subject};
use skyhook_worker::{
    op,
    op::{Redirect, RedirectPolicy},
};
use validation::FetchPlan;

type MaxRedirects = Bounded<0, 20>;
fn default_method() -> String {
    "GET".into()
}
const fn default_timeout() -> Option<NonZeroU64> {
    NonZeroU64::new(30)
}
const fn default_connect_timeout() -> TimeoutSecs {
    Bounded::new::<10>()
}
const fn default_redirects() -> MaxRedirects {
    Bounded::new::<5>()
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FetchArgs {
    url: String,
    #[serde(default = "default_method")]
    method: String,
    /// Appends to the URL query
    #[serde(default)]
    query: Vec<(String, String)>,
    #[serde(default)]
    headers: BTreeMap<String, OneOrMany<String>>,
    #[serde(default)]
    include_headers: bool,
    body: Option<RequestBody>,
    auth: Option<Auth>,
    #[serde(default)]
    response_format: ResponseFormat,
    /// Offered only with the write capability: see [`Fetch::save_to_schema`].
    #[schemars(skip)]
    save_to: Option<SaveTo>,
    /// Seconds for the whole exchange; null means no deadline
    #[serde(default = "default_timeout")]
    timeout: Option<NonZeroU64>,
    /// Seconds
    #[serde(default = "default_connect_timeout")]
    connect_timeout: TimeoutSecs,
    /// Decoded response bytes; excess fails
    max_bytes: Option<NonZeroU64>,
    #[serde(default)]
    redirects: RedirectPolicy,
    #[serde(default = "default_redirects")]
    max_redirects: MaxRedirects,
    proxy: Option<String>,
    /// Skip TLS certificate verification
    #[serde(default)]
    insecure: bool,
}

// The file a response is saved to instead of returned, replacing an existing one
// only with `overwrite`. Documentation here would become the argument's description.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(inline)]
pub(super) struct SaveTo {
    path: op::TargetPath,
    #[serde(default)]
    overwrite: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum RequestBody {
    Text { value: String },
    Json { value: Value },
    Form { fields: Vec<(String, String)> },
    Base64 { value: String },
    File { path: op::TargetPath },
}

#[derive(Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Auth {
    Basic {
        #[schemars(regex(pattern = "^[^:]*$"))]
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
pub enum ResponseFormat {
    /// HTML as markdown, JSON as a value, other text as text, binary as base64
    #[default]
    Auto,
    /// As auto, preferring HTML from the server and reading HTML served as any type
    Readable,
    /// The decoded body as sent
    Text,
    /// The raw bytes as base64
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
    /// Decoded entity bytes received (before extraction)
    size: u64,
    /// Milliseconds
    duration: u64,
}
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ResponseBody {
    Text {
        text: String,
    },
    /// HTML read as markdown
    Page(html::Page),
    /// HTML that could not be read as markdown, with its decoded source
    Unreadable {
        reason: html::Unreadable,
        text: String,
    },
    /// A textual body that holds JSON, read as it
    Json {
        value: serde_json::Value,
    },
    Base64 {
        data: String,
    },
    /// The body in a file: at save_to, or a new one when it is over 4 MiB
    File {
        path: op::TargetPath,
        bytes: u64,
    },
    Empty,
}

/// An HTTP(S) request from where the call runs.
pub struct Fetch;

impl Fetch {
    /// The `save_to` argument, which the input schema leaves to whoever offers the
    /// tool to add where its caller may write.
    pub fn save_to_schema() -> Value {
        let mut generator = crate::schema::input_settings().into_generator();
        generator.subschema_for::<Option<SaveTo>>().to_value()
    }
}

impl TargetTool for Fetch {
    const NAME: &'static str = "fetch";
    const DESCRIPTION: &'static str = "Make an HTTP(S) request. HTTP error statuses are normal results. When HTML is unreadable, auto and readable return its source and a reason. Safe redirects follow GET/HEAD only";
    type Input = FetchArgs;
    type Admitted = FetchPlan;
    type Op = op::Fetch;
    type Rendering = Rendering;

    fn result_schema() -> Option<Value> {
        Some(crate::schema::result_schema::<FetchOutput>())
    }

    fn admit(args: FetchArgs) -> Result<FetchPlan, AdmissionError> {
        FetchPlan::try_from(args)
    }

    fn paths(plan: &mut FetchPlan) -> Vec<PathArgument<'_>> {
        let mut paths = Vec::new();
        if let Output::Saved { path, .. } = &mut plan.output {
            paths.push(PathArgument::new(path, PathAccess::Write, PathKind::Writable).exact());
        }
        if let Some(op::FetchBody::File { path }) = &mut plan.body {
            paths.push(PathArgument::new(path, PathAccess::Read, PathKind::Existing).exact());
        }
        paths
    }

    fn op(plan: &mut FetchPlan, call: &Call) -> Result<op::Fetch, CallError> {
        let body = plan.body.as_mut().map(|body| match body {
            op::FetchBody::Bytes {
                content_type,
                length,
                input,
            } => op::FetchBody::Bytes {
                content_type: *content_type,
                length: *length,
                input: std::mem::take(input),
            },
            op::FetchBody::File { path } => op::FetchBody::File { path: path.clone() },
        });
        let output = match &plan.output {
            Output::Inline(_) => op::FetchOutput::Return {
                inline: call.spills().inline,
                spill: call.spills().name("body"),
            },
            Output::Saved { path, overwrite } => op::FetchOutput::Save {
                path: path.clone(),
                overwrite: *overwrite,
            },
        };
        Ok(op::Fetch {
            url: plan.url.clone(),
            method: plan.method.clone(),
            headers: std::mem::take(&mut plan.headers),
            body,
            client: plan.client.clone(),
            timeout: plan.timeout,
            max_bytes: plan.max_bytes,
            max_redirects: plan.max_redirects,
            redirects: plan.redirects,
            include_headers: plan.include_headers,
            output,
        })
    }

    fn rendering(plan: FetchPlan) -> Rendering {
        let (progress, output) = (FetchProgress::new(), plan.output);
        Rendering { progress, output }
    }

    async fn render<U: Into<CallError> + Send + 'static>(
        rendering: Rendering,
        mut running: op::Running<op::Fetch, U>,
        call: &Call,
    ) -> Result<ProducedOutput, CallError> {
        let Rendering {
            mut progress,
            output,
        } = rendering;
        let mut received = Vec::new();
        loop {
            let event = tokio::select! {
                biased;
                () = call.cancelled() => return Err(progress.failure(CallError::cancelled())),
                event = running.event() => event,
            };
            match event {
                None => break,
                Some(op::FetchEvent::Step(step)) => progress.reach(step),
                Some(op::FetchEvent::CreatedParents) => progress.parents = op::Directories::Created,
                Some(op::FetchEvent::Body(chunk)) => received.extend_from_slice(&chunk),
            }
        }
        let fetched = ended(running).await?;
        let fetched = fetched.map_err(|failure| progress.failure(failure.into()))?;
        let bytes = fetched.size;
        let mut spilled = Vec::new();
        let body = match (output, fetched.spilled) {
            (Output::Saved { path, .. }, _) => ResponseBody::File { path, bytes },
            (Output::Inline(_), Some(file)) => {
                let path = file.path.clone();
                spilled.push(file);
                ResponseBody::File { path, bytes }
            }
            (Output::Inline(format), None) => {
                progress.step(
                    Operation::Deserialize,
                    Subject::Label("HTTP response body".into()),
                );
                let content_type = fetched.content_type.as_deref();
                let url = &fetched.url;
                response_body(received, content_type, url, format, &mut progress).await
            }
        };
        let mut output = ProducedOutput::serialized(FetchOutput {
            status: fetched.status,
            headers: fetched.headers,
            redirects: fetched.redirects,
            body,
            size: fetched.size,
            duration: fetched.duration,
        })?;
        output.spilled = spilled;
        Ok(output)
    }
}

/// A started fetch, as its events and end render: the steps it has reached, and
/// whether its body returns in a format or was saved.
pub struct Rendering {
    progress: FetchProgress,
    output: Output,
}

/// Where a fetch's body goes: returned in a format, when the target sends it back,
/// or saved to a path, replacing a file there only with `overwrite`.
pub enum Output {
    Inline(ResponseFormat),
    Saved {
        path: op::TargetPath,
        overwrite: bool,
    },
}

#[cfg(test)]
mod tests {
    use std::{assert_matches, sync::Arc, time::Duration};

    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::json;
    use tokio::io::AsyncWriteExt as _;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::diagnostic::{Cause, RequestEffects};
    use crate::tests::{bounded, read_request};
    use crate::{error::OperationError, spec::tests::Workspace};
    use skyhook_worker::diagnostic::{Effects, Operation, Subject};
    use skyhook_worker::op::IoKind;

    pub(super) fn args(value: Value) -> FetchArgs {
        serde_json::from_value(value).unwrap()
    }

    fn reply(status: &str, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}",
            body.len()
        )
    }

    /// How a scripted connection answers the request it read.
    enum Answer {
        /// Write these bytes and close.
        Reply(Vec<u8>),
        /// Write these bytes, then hold the connection open.
        Stall(Vec<u8>),
    }

    /// A loopback server answering each connection with its next answer.
    struct Server {
        address: std::net::SocketAddr,
        requests: tokio::sync::mpsc::UnboundedReceiver<String>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Server {
        async fn start(answers: Vec<Answer>) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (sent, requests) = tokio::sync::mpsc::unbounded_channel();
            let task = tokio::spawn(async move {
                let answers = Arc::new(std::sync::Mutex::new(answers.into_iter()));
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let (sent, answers) = (sent.clone(), answers.clone());
                    connections.spawn(async move {
                        let request = read_request(&mut socket).await;
                        let answer = answers.lock().unwrap().next();
                        sent.send(request).unwrap();
                        let (wire, close) = match answer {
                            Some(Answer::Reply(wire)) => (wire, true),
                            Some(Answer::Stall(wire)) => (wire, false),
                            None => (Vec::new(), false),
                        };
                        // Timed-out attempts may close the connection before writing.
                        let _ = socket.write_all(&wire).await;
                        if !close {
                            std::future::pending::<()>().await;
                        }
                    });
                }
            });
            Self {
                address,
                requests,
                task,
            }
        }

        fn root(&self) -> String {
            format!("http://{}", self.address)
        }

        async fn request(&mut self) -> String {
            bounded(self.requests.recv()).await.unwrap()
        }

        /// Requests not yet taken, once the client's calls have returned.
        fn finish(mut self) -> Vec<String> {
            std::iter::from_fn(|| self.requests.try_recv().ok()).collect()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    /// A server answering each connection with the next reply, and its root URL.
    async fn server(replies: Vec<impl Into<Vec<u8>>>) -> (String, Server) {
        let answers = replies.into_iter().map(|reply| Answer::Reply(reply.into()));
        let server = Server::start(answers.collect()).await;
        (server.root(), server)
    }

    async fn fetch(
        workspace: &Workspace,
        arguments: Value,
    ) -> Result<Value, OperationError<Value>> {
        workspace.run("fetch", arguments).await
    }

    /// Drive `operation` until `reached` completes, then fire every timer due
    /// within `limit` at once, so the deadline under test expires only after the
    /// state it guards is reached. Current-thread only.
    async fn expire<T, R>(
        operation: impl Future<Output = T>,
        reached: impl Future<Output = R>,
        limit: Duration,
    ) -> (T, R) {
        let mut operation = std::pin::pin!(operation);
        let reached = tokio::select! {
            _ = &mut operation => panic!("finished before its deadline expired"),
            reached = reached => reached,
        };
        tokio::time::pause();
        // Timers fire on whole milliseconds: a deadline `limit` after a moment
        // inside the current one rounds up past `limit`.
        tokio::time::advance(limit + Duration::from_millis(1)).await;
        tokio::time::resume();
        (bounded(operation).await, reached)
    }

    /// Wait until a download's staging file holds `bytes`: the client has read
    /// the response head and that much of its body.
    async fn staged(directory: &std::path::Path, bytes: u64) {
        let holds = || {
            (std::fs::read_dir(directory).into_iter().flatten())
                .flatten()
                .any(|entry| {
                    entry.file_name().to_string_lossy().starts_with(".skyhook-")
                        && (entry.metadata()).is_ok_and(|metadata| metadata.len() == bytes)
                })
        };
        bounded(async {
            while !holds() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
    }

    /// Failures carry no structured result, the operation in progress, and a
    /// typed transport cause.
    #[tokio::test]
    async fn transport_failures_and_deadlines_name_the_operation_in_progress() {
        let workspace = Workspace::new();
        let hour = Duration::from_secs(3600);
        // A dropped listener's port can be rebound by another test's server
        // before the connect; the privileged port is never listened on.
        let url = "http://127.0.0.1:1/private?token=secret#secret";
        let error = fetch(&workspace, json!({"url":url})).await.unwrap_err();
        let (diagnostic, output) = error.into_parts();
        assert!(output.is_none());
        assert_matches!(
            diagnostic.cause,
            Cause::Io(skyhook_worker::op::OsError {
                kind: IoKind::ConnectionRefused,
                code: Some(_),
                ..
            })
        );
        assert_eq!(
            diagnostic.context.subject,
            Subject::Label("HTTP GET http://127.0.0.1:1".into())
        );

        // The deadline elapses after the head and part of the body arrived.
        let partial = "partial";
        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{partial}");
        let server = Server::start(vec![Answer::Stall(head.into_bytes())]).await;
        let arguments = json!({"url":server.root(), "timeout":3600, "save_to":{"path":"download"}});
        let fetched = fetch(&workspace, arguments);
        let received = staged(workspace.path(), partial.len() as u64);
        let (result, ()) = expire(fetched, received, hour).await;
        let diagnostic = result.unwrap_err().diagnostic();
        assert_eq!(diagnostic.context.operation, Operation::Receive);
        assert_eq!(
            diagnostic.context.subject,
            Subject::path(crate::tests::text(workspace.path().join("download")))
        );
        assert_eq!(diagnostic.context.effects, Effects::Unchanged);
        assert_eq!(diagnostic.context.request, RequestEffects::MayHaveExecuted);
        assert_eq!(
            diagnostic.cause,
            Cause::Message("the 3600 s timeout elapsed".into())
        );
        assert!(!workspace.path().join("download").exists());

        // The deadline elapses once the server holds the request it never answers.
        let mut server = Server::start(vec![Answer::Stall(Vec::new())]).await;
        let arguments = json!({"url":server.root(), "timeout":3600});
        let fetched = fetch(&workspace, arguments);
        let (result, _) = expire(fetched, server.request(), hour).await;
        let context = result.unwrap_err().diagnostic().context;
        assert_eq!(context.operation, Operation::Send);
        assert_eq!(context.request, RequestEffects::MayHaveExecuted);
    }

    /// Cancelled once the body is being staged, a download names the staging
    /// step, leaves the destination as it was and removes what it staged.
    #[tokio::test]
    async fn cancellation_while_staging_names_the_download_and_leaves_the_destination() {
        let workspace = Workspace::new();
        let destination = workspace.path().join("saved.txt");
        tokio::fs::write(&destination, b"original").await.unwrap();
        let partial = "partial";
        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{partial}");
        let server = Server::start(vec![Answer::Stall(head.into_bytes())]).await;
        let arguments =
            json!({"url":server.root(), "save_to":{"path":"saved.txt", "overwrite":true}});
        let (sink, cancellation) = (Arc::default(), CancellationToken::new());
        let run = workspace.run_with("fetch", arguments, &sink, cancellation.clone());
        let (result, ()) = tokio::join!(bounded(run), async {
            staged(workspace.path(), partial.len() as u64).await;
            cancellation.cancel();
        });
        let diagnostic = result.unwrap_err().diagnostic();
        assert_eq!(diagnostic.cause, Cause::Cancelled);
        assert_eq!(diagnostic.context.operation, Operation::Receive);
        assert_eq!(
            diagnostic.context.subject,
            Subject::path(crate::tests::text(&destination))
        );
        assert_eq!(diagnostic.context.effects, Effects::Unchanged);
        assert_eq!(diagnostic.context.request, RequestEffects::MayHaveExecuted);
        assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"original");
        // The stopped fetch removes what it staged as its target drops it.
        let staging = || {
            let mut entries = std::fs::read_dir(workspace.path()).unwrap().flatten();
            entries.any(|entry| entry.file_name().to_string_lossy().starts_with(".skyhook-"))
        };
        bounded(async {
            while staging() {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }

    /// A returned body within the limit comes back in its format; one byte more and
    /// it is kept whole in a file of its user's alone in the target's spill
    /// directory, named in place of its content.
    #[tokio::test]
    async fn a_body_over_the_limit_is_kept_in_a_spill_file() {
        let workspace = Workspace::new().spilling_past(8);
        let (url, _task) = server(vec![reply("200 OK", OK, "12345678")]).await;
        let within = fetch(&workspace, json!({"url":url})).await.unwrap();
        assert_eq!(within["body"], json!({"kind":"text", "text":"12345678"}));
        assert!(workspace.spilled().is_empty());

        let (url, _task) = server(vec![reply("200 OK", OK, "123456789")]).await;
        let over = fetch(&workspace, json!({"url":url})).await.unwrap();
        let [file] = &workspace.spilled()[..] else {
            panic!("one spill file")
        };
        let body = json!({"kind":"file", "path":crate::tests::text(file), "bytes":9});
        assert_eq!((&over["body"], &over["size"]), (&body, &json!(9)));
        assert_eq!(tokio::fs::read(file).await.unwrap(), b"123456789");
        let permissions = file.metadata().unwrap().permissions();
        let mode = std::os::unix::fs::PermissionsExt::mode(&permissions);
        assert_eq!(mode & 0o777, 0o600);
    }

    /// A spill that fails or is cancelled part way leaves no file, failing as the
    /// response body it was receiving, and one whose directory cannot be made fails.
    #[tokio::test]
    async fn an_unfinished_spill_leaves_no_file_and_an_unwritable_directory_fails() {
        let workspace = Workspace::new().spilling_past(4);
        let spills = workspace.path().join("spills/session");
        let truncated =
            "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 100\r\n\r\npartial";
        let (url, _task) = server(vec![truncated]).await;
        let cut = fetch(&workspace, json!({"url":url})).await.unwrap_err();
        let context = cut.diagnostic().context;
        let body = Subject::Label("HTTP response body".into());
        assert_eq!(
            (context.operation, context.subject),
            (Operation::Receive, body)
        );
        assert_eq!(std::fs::read_dir(&spills).unwrap().count(), 0);

        let stalled = "HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial";
        let stalling = Server::start(vec![Answer::Stall(stalled.into())]).await;
        let (sink, cancellation) = (Arc::default(), CancellationToken::new());
        let arguments = json!({"url":stalling.root()});
        let run = workspace.run_with("fetch", arguments, &sink, cancellation.clone());
        let (result, ()) = tokio::join!(bounded(run), async {
            staged(&spills, "partial".len() as u64).await;
            cancellation.cancel();
        });
        assert_eq!(result.unwrap_err().diagnostic().cause, Cause::Cancelled);
        // The stopped fetch removes what it staged as its target drops it.
        bounded(async {
            while std::fs::read_dir(&spills).unwrap().count() > 0 {
                tokio::task::yield_now().await;
            }
        })
        .await;

        let workspace = Workspace::new().spilling_past(4);
        std::fs::write(workspace.path().join("spills"), b"").unwrap();
        let (url, _task) = server(vec![reply("200 OK", OK, "12345")]).await;
        let failed = fetch(&workspace, json!({"url":url})).await.unwrap_err();
        let context = failed.diagnostic().context;
        assert_eq!(context.operation, Operation::CreateDirectories);
    }

    #[tokio::test]
    async fn safe_manual_follow_cross_origin_credentials_and_limits() {
        let workspace = Workspace::new();
        let payload = json!({"kind":"text","value":"payload"});
        for policy in ["safe", "manual"] {
            let (url, task) =
                server(vec![reply("302 Found", "Location: /next\r\n", "redirect")]).await;
            let arguments = json!({"url":url,"method":"POST","body":payload,"redirects":policy});
            let result = fetch(&workspace, arguments).await.unwrap();
            assert_eq!(result["status"], 302);
            assert!(result.get("redirects").is_none());
            assert!(task.finish()[0].ends_with("payload"));
        }
        let (end, end_task) = server(vec![reply("200 OK", OK, "done")]).await;
        let location = format!("Location: {end}/final\r\n");
        let (start, start_task) = server(vec![reply("303 See Other", &location, "")]).await;
        let arguments = json!({
            "url":start, "method":"POST", "body":payload, "redirects":"follow",
            "auth":{"kind":"bearer","token":"secret"},
            "headers":{"Cookie":"private=1","X-Api-Key":"custom-secret"}
        });
        let result = fetch(&workspace, arguments).await.unwrap();
        let location = format!("{end}/final");
        assert_eq!(
            result["redirects"],
            json!([{"status":303, "location":location, "method":"POST"}])
        );
        assert!(start_task.finish()[0].contains("authorization: Bearer secret"));
        let request = &end_task.finish()[0];
        assert!(request.starts_with("GET /final HTTP/1.1"));
        for secret in ["secret", "private=1", "payload", "content-type"] {
            assert!(!request.contains(secret));
        }
        // An exhausted redirect limit fails.
        let (url, _task) = server(vec![reply("302 Found", "Location: /again\r\n", "")]).await;
        let limited = fetch(&workspace, json!({"url":url,"max_redirects":0})).await;
        assert!(limited.is_err());
    }

    const OK: &str = "Content-Type: text/plain\r\n";

    #[tokio::test]
    async fn methods_body_encodings_and_307_file_replays_reach_the_server() {
        let workspace = Workspace::new();
        tokio::fs::write(workspace.path().join("upload.txt"), b"file payload")
            .await
            .unwrap();
        let file = json!({"kind":"file","path":"upload.txt"});
        let cases = [
            ("GET", None, ""),
            ("HEAD", None, ""),
            ("OPTIONS", None, ""),
            ("DELETE", None, ""),
            ("TRACE", None, ""),
            (
                "PROPFIND",
                Some(json!({"kind":"text","value":"custom payload"})),
                "custom payload",
            ),
            ("POST", Some(json!({"kind":"json","value":null})), "null"),
            (
                "PATCH",
                Some(json!({"kind":"json","value":{"enabled":true}})),
                "{\"enabled\":true}",
            ),
            (
                "POST",
                Some(json!({"kind":"form","fields":[["key","a b"],["key","&"]]})),
                "key=a+b&key=%26",
            ),
            (
                "PUT",
                Some(json!({"kind":"base64","value":"Ynl0ZXM="})),
                "bytes",
            ),
            ("PUT", Some(file.clone()), "file payload"),
        ];
        let (url, task) = server(cases.iter().map(|_| reply("200 OK", OK, "")).collect()).await;
        for (method, body, _) in &cases {
            let mut arguments =
                json!({"url":url,"method":method,"headers":{"User-Agent":"CustomClient/2"}});
            if let Some(body) = body {
                arguments["body"] = body.clone();
            }
            assert_eq!(fetch(&workspace, arguments).await.unwrap()["status"], 200);
        }
        for (request, (method, _, expected)) in task.finish().iter().zip(cases) {
            assert!(
                request.starts_with(&format!("{method} / HTTP/1.1\r\n")),
                "{request}"
            );
            let (head, body) = request.split_once("\r\n\r\n").unwrap();
            assert_eq!(body, expected);
            assert!(head.contains("user-agent: CustomClient/2") && !head.contains("Skyhook/"));
        }
        // A followed 307 replays the file upload with its method.
        let (url, task) = server(vec![
            reply("307 Temporary Redirect", "Location: /again\r\n", ""),
            reply("200 OK", OK, "done"),
        ])
        .await;
        let arguments = json!({"url":url,"method":"PUT","redirects":"follow","body":file});
        fetch(&workspace, arguments).await.unwrap();
        for request in task.finish() {
            assert!(request.starts_with("PUT ") && request.ends_with("file payload"));
        }
    }

    #[tokio::test]
    async fn http_errors_query_repeated_headers_and_auth() {
        let headers = "Content-Type: text/plain\r\nSet-Cookie: one=1\r\nSet-Cookie: two=2\r\n";
        let (url, task) = server(vec![reply("404 Not Found", headers, "missing")]).await;
        let arguments = json!({
            "url":format!("{url}/p?z=0"), "query":[["q","a b"],["q","c"]], "include_headers":true,
            "headers":{"x-repeat":["one","two"]}, "auth":{"kind":"basic","username":"u","password":"p"}
        });
        let output = fetch(&Workspace::new(), arguments).await.unwrap();
        assert_eq!(output["status"], 404);
        assert_eq!(output["body"]["text"], "missing");
        assert_eq!(output["headers"]["set-cookie"], json!(["one=1", "two=2"]));
        let requests = task.finish();
        assert!(requests[0].starts_with("GET /p?z=0&q=a+b&q=c HTTP/1.1"));
        assert!(requests[0].contains("authorization: Basic dTpw"));
        let user_agent = concat!("user-agent: Skyhook/", env!("CARGO_PKG_VERSION"), "\r\n");
        assert!(requests[0].contains(user_agent));
        assert!(requests[0].contains("x-repeat: one\r\nx-repeat: two"));
    }

    #[tokio::test]
    async fn download_publication_failure_keeps_response_and_existing_destination() {
        let workspace = Workspace::new();
        let destination = workspace.path().join("raced-download");
        let raced = destination.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            // The observed request proves destination preflight has finished;
            // publication must still reject a newly created destination.
            tokio::fs::write(raced, b"keep original").await.unwrap();
            socket
                .write_all(reply("200 OK", "", "replacement").as_bytes())
                .await
                .unwrap();
        });
        let arguments = json!({"url":url,"save_to":{"path":"raced-download"}});
        let error = bounded(fetch(&workspace, arguments)).await.unwrap_err();
        bounded(task).await.unwrap();
        let context = error.diagnostic().context;
        assert_eq!(context.operation, Operation::Rename);
        assert_eq!(
            context.subject,
            Subject::path(crate::tests::text(&destination))
        );
        assert_eq!(context.request, RequestEffects::MayHaveExecuted);
        let kept = tokio::fs::read(destination).await.unwrap();
        assert_eq!(kept, b"keep original");
    }

    /// A download creates the directories that hold it, which a failure leaves.
    #[tokio::test]
    async fn download_creates_missing_parents_which_a_failure_leaves() {
        let workspace = Workspace::new();
        let truncated = "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 100\r\n\r\nshort";
        let (url, _task) = server(vec![truncated.to_owned()]).await;
        let arguments = json!({"url":url, "save_to":{"path":"missing/nested/download"}});
        let error = fetch(&workspace, arguments).await.unwrap_err();
        assert_eq!(error.diagnostic().context.effects, Effects::PartialChange);
        assert!(workspace.path().join("missing/nested").is_dir());
        assert!(!workspace.path().join("missing/nested/download").exists());
    }

    #[tokio::test]
    async fn download_atomicity_limits_truncation_and_head() {
        let workspace = Workspace::new();
        let destination = workspace.path().join("out");
        tokio::fs::write(&destination, b"original").await.unwrap();
        let oversized = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\n1234\r\n4\r\n5678\r\n0\r\n\r\n";
        // A truncated response fails.
        let truncated = "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 100\r\n\r\nshort";
        for (raw, mut arguments) in [
            (
                oversized,
                json!({"save_to":{"path":"out","overwrite":true},"max_bytes":5}),
            ),
            (truncated, json!({})),
        ] {
            let (url, _task) = server(vec![raw.to_owned()]).await;
            arguments["url"] = json!(url);
            assert!(fetch(&workspace, arguments).await.is_err());
            assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"original");
        }
        let (url, _task) = server(vec![reply("200 OK", "", "replacement")]).await;
        let arguments = json!({"url":url,"save_to":{"path":"out","overwrite":true}});
        let result = fetch(&workspace, arguments).await.unwrap();
        assert_eq!(
            (&result["body"]["kind"], &result["body"]["bytes"]),
            (&json!("file"), &json!(11))
        );
        assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"replacement");
        let unreachable = json!({"url":"http://127.0.0.1:1","save_to":{"path":"out"}});
        assert!(fetch(&workspace, unreachable).await.is_err());
        let head = "HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\nConnection: close\r\n\r\n";
        let (url, _task) = server(vec![head.to_owned()]).await;
        let arguments = json!({"url":url,"method":"HEAD","max_bytes":1});
        assert_eq!(fetch(&workspace, arguments).await.unwrap()["size"], 0);
    }

    #[tokio::test]
    async fn decompressed_bytes_are_counted_and_limited() {
        // Gzip of 4096 ASCII A bytes, with deterministic zero mtime.
        let compressed = STANDARD
            .decode("H4sIAAAAAAAC/+3BAQ0AAADCoGzvX8oeDigAAADg3QBANKb+ABAAAA==")
            .unwrap();
        let mut response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", compressed.len()).into_bytes();
        response.extend_from_slice(&compressed);
        let workspace = Workspace::new();
        for max_bytes in [4096, 100] {
            let (url, _task) = server(vec![response.clone()]).await;
            let result = fetch(&workspace, json!({"url":url,"max_bytes":max_bytes})).await;
            if max_bytes == 4096 {
                let output = result.unwrap();
                assert_eq!(output["size"], 4096);
                assert_eq!(output["body"]["text"], "A".repeat(4096));
            } else {
                assert!(result.is_err());
            }
        }
    }

    #[tokio::test]
    async fn json_bodies_are_read_as_values_unless_text_is_requested() {
        const UNREADABLE: &str = "<html><body><div id='app'></div></body></html>";
        let workspace = Workspace::new();
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
            (
                "auto",
                html,
                UNREADABLE,
                json!({"kind":"unreadable", "reason":"no_text", "text":UNREADABLE}),
            ),
        ] {
            let (url, _task) = server(vec![reply("200 OK", headers, body)]).await;
            let arguments = json!({"url": url, "response_format": format});
            let output = fetch(&workspace, arguments).await.unwrap();
            assert_eq!(output["body"], expected, "{format}: {body}");
        }
    }

    /// Bodies are captured whole in their requested format; headers appear only
    /// when asked for.
    #[tokio::test]
    async fn response_bodies_are_captured_whole_and_headers_only_on_request() {
        let workspace = Workspace::new();
        let payload = "body".repeat(64 * 1024);
        let header = "h".repeat(3000);
        let headers = format!("Content-Type: text/plain\r\nX-Details: {header}\r\n");
        for (format, field, expected) in [
            ("text", "text", payload.clone()),
            ("base64", "data", STANDARD.encode(payload.as_bytes())),
        ] {
            let (url, _task) = server(vec![reply("200 OK", &headers, &payload)]).await;
            let arguments = json!({"url":url, "response_format":format, "include_headers":true});
            let result = fetch(&workspace, arguments).await.unwrap();
            assert_eq!(result["status"], 200);
            assert_eq!(result["size"], payload.len());
            assert_eq!(result["headers"]["x-details"][0], header);
            assert_eq!(result["body"]["kind"], format);
            assert_eq!(result["body"][field], expected);
        }
        let (url, _task) = server(vec![reply("200 OK", OK, "ok")]).await;
        let result = fetch(&workspace, json!({"url":url})).await.unwrap();
        assert_eq!(result["body"], json!({"kind":"text", "text":"ok"}));
        assert!(result.get("headers").is_none());
    }
}
