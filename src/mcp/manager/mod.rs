//! Session-scoped MCP clients and an immutable, startup-discovered tool catalog.
//!
//! Only the root runtime constructs a manager. Children share its `Arc`; they
//! neither initialize servers nor acquire ownership of server processes.

mod catalog;
mod connection;

use super::config::McpServerConfig;
use crate::tool::policy::{Capability, CapabilitySet};
use catalog::bounded_json_size;
use connection::{Server, connect_one};
use futures_util::{StreamExt, stream};
use rmcp::{
    RoleClient,
    model::{
        CallToolRequest, CallToolRequestParams, CallToolResult, ClientRequest, ServerResult, Tool,
    },
    service::{PeerRequestOptions, RequestHandle, ServiceError},
};
use serde_json::{Map, Value};
use std::{collections::BTreeMap, time::Duration};
use tokio_util::sync::CancellationToken;

const MAX_RESULT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("invalid MCP configuration: {0}")]
    Configuration(String),
    #[error("MCP startup failed: {0}")]
    Startup(String),
    #[error("MCP operation cancelled; an in-flight call may have executed")]
    Cancelled,
    #[error("MCP operation timed out; an in-flight call may have executed")]
    Timeout,
    #[error("MCP manager is shut down")]
    Closed,
    #[error("MCP tool is not in the startup catalog: {server}/{tool}")]
    UnknownTool { server: String, tool: String },
    #[error("MCP request failed: {0}")]
    Request(String),
}

#[derive(Debug, Clone)]
pub struct DiscoveredTool {
    pub(crate) server: String,
    pub(crate) tool: Tool,
    /// Frozen registration requirements from the configuration admitted at startup.
    pub(crate) capabilities: Vec<Capability>,
}

/// Startup outcome of one configured server, frozen with the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerStatus {
    Connected {
        tools: usize,
    },
    Failed(String),
    /// Never started: excluded by capability policy, or startup was cancelled.
    Skipped,
}

// rmcp request handles do not cancel on drop. Keep ownership until a response
// arrives, so aborting the caller's future also sends bounded best-effort cancel.
struct CancelOnDrop(Option<RequestHandle<RoleClient>>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                let _ = tokio::time::timeout(
                    Duration::from_millis(100),
                    handle.cancel(Some("Skyhook caller cancelled or timed out".into())),
                )
                .await;
            });
        }
    }
}

pub struct McpManager {
    catalog: Vec<DiscoveredTool>,
    warnings: Vec<String>,
    servers: BTreeMap<String, Server>,
    statuses: BTreeMap<String, McpServerStatus>,
    closed: CancellationToken,
}

impl McpManager {
    /// Filter before creating startup futures: even secret lookup or a failed
    /// connection for an ineligible server would cross the capability boundary.
    /// An individual eligible server failure is a warning, not fatal.
    pub async fn connect(
        configs: &BTreeMap<String, McpServerConfig>,
        capabilities: &CapabilitySet,
        cancel: CancellationToken,
    ) -> Self {
        let mut manager = Self {
            catalog: Vec::new(),
            warnings: Vec::new(),
            servers: BTreeMap::new(),
            statuses: configs
                .keys()
                .map(|name| (name.clone(), McpServerStatus::Skipped))
                .collect(),
            closed: CancellationToken::new(),
        };
        // Own entries before creating futures: no borrowed iterator/closure
        // lifetime leaks into the public Send future (including cross-crate use).
        let entries: Vec<_> = configs
            .iter()
            .filter(|(_, config)| {
                capabilities.contains(Capability::Mcp)
                    && config
                        .capabilities()
                        .iter()
                        .all(|cap| capabilities.contains(*cap))
            })
            .map(|(name, config)| (name.clone(), config.clone()))
            .collect();
        let futures: Vec<_> = entries
            .into_iter()
            .map(|(name, config)| {
                let cancel = cancel.clone();
                async move {
                    let policy = config.capabilities().to_vec();
                    (policy, connect_one(name, config, cancel).await)
                }
            })
            .collect();
        let mut pending = stream::iter(futures).buffer_unordered(4);
        while let Some((policy, result)) = pending.next().await {
            let Some((name, result)) = result else {
                continue;
            };
            match result {
                Ok((tools, server)) => {
                    let status = McpServerStatus::Connected { tools: tools.len() };
                    manager.statuses.insert(name.clone(), status);
                    manager
                        .catalog
                        .extend(tools.into_iter().map(|tool| DiscoveredTool {
                            server: name.clone(),
                            tool,
                            capabilities: policy.clone(),
                        }));
                    manager.servers.insert(name, server);
                }
                Err(error) => {
                    manager
                        .warnings
                        .push(format!("MCP server {name:?} unavailable: {error}"));
                    manager
                        .statuses
                        .insert(name, McpServerStatus::Failed(error.to_string()));
                }
            }
        }
        manager
            .catalog
            .sort_by(|a, b| (&a.server, &a.tool.name).cmp(&(&b.server, &b.tool.name)));
        manager.warnings.sort();
        manager
    }

    /// Frozen for the lifetime of the session. List-changed notifications do not
    /// alter this catalog or inject new tools into an already running agent.
    pub fn catalog(&self) -> &[DiscoveredTool] {
        &self.catalog
    }
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
    /// Every configured server, including those that failed or were skipped.
    pub fn servers(&self) -> &BTreeMap<String, McpServerStatus> {
        &self.statuses
    }

    /// Send exactly one request. Cancellation and timeout report an uncertain
    /// outcome; neither this manager nor its HTTP transport retries a tool call.
    pub async fn call(
        &self,
        server: &str,
        tool: &str,
        arguments: Map<String, Value>,
        cancel: CancellationToken,
    ) -> Result<CallToolResult, McpError> {
        if self.closed.is_cancelled() {
            return Err(McpError::Closed);
        }
        if cancel.is_cancelled() {
            return Err(McpError::Cancelled);
        }
        if !self
            .catalog
            .iter()
            .any(|entry| entry.server == server && entry.tool.name == tool)
        {
            return Err(McpError::UnknownTool {
                server: server.into(),
                tool: tool.into(),
            });
        }
        let server = self.servers.get(server).expect("catalog server exists");
        let deadline = tokio::time::Instant::now() + server.timeout;
        let _permit = guarded(&self.closed, &cancel, deadline, server.calls.acquire())
            .await?
            .map_err(|_| McpError::Closed)?;
        if bounded_json_size(&arguments, MAX_RESULT_BYTES).is_none() {
            return Err(McpError::Request("tool arguments exceed size limit".into()));
        }
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(
            CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments),
        ));
        let send = server
            .peer
            .send_cancellable_request(request, PeerRequestOptions::default());
        let handle = guarded(&self.closed, &cancel, deadline, send)
            .await?
            .map_err(request_error)?;
        let mut handle = CancelOnDrop(Some(handle));
        let response = &mut handle.0.as_mut().expect("active request").rx;
        let result = match guarded(&self.closed, &cancel, deadline, response).await {
            Ok(Ok(Ok(ServerResult::CallToolResult(result)))) => {
                if bounded_json_size(&result, MAX_RESULT_BYTES).is_none() {
                    Err(McpError::Request("tool result exceeds size limit".into()))
                } else {
                    Ok(result)
                }
            }
            Ok(Ok(Ok(_))) => Err(McpError::Request(
                "unexpected response to tools/call".into(),
            )),
            Ok(Ok(Err(error))) => Err(request_error(error)),
            Ok(Err(_)) => Err(McpError::Request("transport closed".into())),
            Err(error) => Err(error),
        };
        if !matches!(
            result,
            Err(McpError::Cancelled | McpError::Timeout | McpError::Closed)
        ) {
            handle.0.take(); // Completed: never send a spurious cancellation.
        }
        result
    }

    /// Idempotent. Close all sessions, and kill/reap only processes we launched.
    /// Externally managed HTTP servers never have an OwnedProcess.
    pub async fn shutdown(&self) {
        self.closed.cancel();
        for server in self.servers.values() {
            server.shutdown().await;
        }
    }
}

impl Drop for McpManager {
    fn drop(&mut self) {
        self.closed.cancel();
    }
}

/// Runs `future` until the manager closes, the caller cancels, or the deadline passes.
async fn guarded<T>(
    closed: &CancellationToken,
    cancel: &CancellationToken,
    deadline: tokio::time::Instant,
    future: impl Future<Output = T>,
) -> Result<T, McpError> {
    tokio::select! {
        biased;
        () = closed.cancelled() => Err(McpError::Closed),
        () = cancel.cancelled() => Err(McpError::Cancelled),
        () = tokio::time::sleep_until(deadline) => Err(McpError::Timeout),
        value = future => Ok(value),
    }
}

// SDK transport diagnostics can contain credential-bearing URLs. Never expose
// them to the model or logs; only bounded remote JSON-RPC messages are retained.
fn request_error(error: ServiceError) -> McpError {
    match error {
        ServiceError::McpError(error) => {
            let message: String = error.message.chars().take(1024).collect();
            McpError::Request(format!("server JSON-RPC error {}: {message}", error.code.0))
        }
        ServiceError::Timeout { .. } => McpError::Timeout,
        ServiceError::Cancelled { .. } => McpError::Cancelled,
        _ => McpError::Request("MCP transport or protocol failure".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{config::McpServerConfig, manager::McpManager};
    use serde_json::{Map, Value, json};
    use std::{collections::BTreeMap, path::Path, time::Duration};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    const FIXTURE: &str = r#"
import json, os, sys, threading, time

with open(os.environ['MCP_TEST_PID'] + '.tmp', 'w') as f:
    f.write(str(os.getpid()))
os.replace(os.environ['MCP_TEST_PID'] + '.tmp', os.environ['MCP_TEST_PID'])
lock = threading.Lock()
def send(message):
    with lock:
        print(json.dumps(message), flush=True)
def tool(name):
    return {'name': name, 'description': 'fixture ' + name,
            'inputSchema': {'type': 'object', 'additionalProperties': True}}
def handle(request):
    if request.get('method') == 'notifications/cancelled':
        with open(os.environ['MCP_TEST_CANCELLED'], 'w') as f:
            f.write(json.dumps(request['params']))
    if 'id' not in request:
        return
    ident = request['id']
    method = request['method']
    params = request.get('params', {})
    if method == 'initialize':
        with open(os.environ['MCP_TEST_INITIALIZE'], 'w') as f:
            f.write(json.dumps(params))
        if os.environ.get('MCP_TEST_OVERSIZED_FRAME'):
            sys.stdout.write('x' * (8 * 1024 * 1024 + 1))
            sys.stdout.flush()
            return
        if os.environ.get('MCP_TEST_HANG_INITIALIZE') == '1':
            time.sleep(60)
        result = {'protocolVersion': params['protocolVersion'],
                  'capabilities': ({'resources': {}} if os.environ.get('MCP_TEST_NO_TOOLS') else {'tools': {}}),
                  'serverInfo': {'name': 'skyhook-test', 'version': '1'}}
    elif method == 'tools/list':
        with open(os.environ['MCP_TEST_LIST'], 'w') as f:
            f.write('called')
        mode = os.environ.get('MCP_TEST_DISCOVERY')
        if mode == 'oversized_schema':
            result = {'tools': [dict(tool('huge'), description='x' * (256 * 1024))]}
            send({'jsonrpc': '2.0', 'id': ident, 'result': result})
            return
        if mode == 'many_tools':
            result = {'tools': [tool('tool_' + str(i)) for i in range(1025)]}
            send({'jsonrpc': '2.0', 'id': ident, 'result': result})
            return
        if mode == 'repeated_cursor':
            result = {'tools': [], 'nextCursor': 'repeat'}
            send({'jsonrpc': '2.0', 'id': ident, 'result': result})
            return
        cursor = params.get('cursor')
        if cursor is None:
            result = {'tools': [tool('echo')], 'nextCursor': 'second-page'}
        elif cursor == 'second-page':
            result = {'tools': [tool('tool_error'), tool('rpc_error'), tool('slow')]}
        else:
            send({'jsonrpc': '2.0', 'id': ident,
                  'error': {'code': -32602, 'message': 'unexpected cursor'}})
            return
    elif method == 'tools/call':
        name = params['name']
        if os.environ.get('MCP_TEST_LARGE_RESULT') == '1':
            send({'jsonrpc': '2.0', 'id': ident, 'result': {'content': [{'type': 'text', 'text': 'x' * (4 * 1024 * 1024)}]}})
            return
        if name == 'rpc_error':
            send({'jsonrpc': '2.0', 'id': ident,
                  'error': {'code': -32602, 'message': 'fixture protocol error'}})
            return
        if name == 'slow':
            with open(os.environ['MCP_TEST_STARTED'], 'w') as f:
                f.write('started')
            time.sleep(60)
        if name == 'tool_error':
            result = {'content': [{'type': 'text', 'text': 'fixture tool error'}],
                      'isError': True}
        else:
            payload = {'arguments': params.get('arguments', {}),
                       'cwd': os.getcwd(), 'env': os.environ.get('MCP_TEST_VALUE')}
            result = {'content': [{'type': 'text', 'text': json.dumps(payload)}],
                      'isError': False}
    elif method == 'ping':
        result = {}
    else:
        send({'jsonrpc': '2.0', 'id': ident,
              'error': {'code': -32601, 'message': 'unknown method'}})
        return
    send({'jsonrpc': '2.0', 'id': ident, 'result': result})

# Daemon request workers let a cancelled call remain pending without preventing
# subsequent requests, or keeping the process alive once the client closes stdin.
for line in sys.stdin:
    threading.Thread(target=handle, args=(json.loads(line),), daemon=True).start()
"#;

    pub(super) struct Fixture {
        pub(super) directory: TempDir,
    }

    impl Fixture {
        pub(super) fn new() -> Option<Self> {
            let python = std::process::Command::new("python3")
                .arg("--version")
                .output();
            if !python.is_ok_and(|output| output.status.success()) {
                eprintln!("skipping MCP subprocess test: python3 is unavailable");
                return None;
            }
            let directory = tempfile::tempdir().expect("fixture directory");
            Some(Self { directory })
        }

        pub(super) fn config(&self) -> crate::mcp::config::RawMcpServerConfig {
            let mut env: BTreeMap<_, _> = ["pid", "started", "cancelled", "initialize", "list"]
                .into_iter()
                .map(|name| (format!("MCP_TEST_{}", name.to_uppercase()), self.path(name)))
                .collect();
            env.insert("MCP_TEST_VALUE".into(), "configured child value".into());
            serde_json::from_value(json!({
                "transport": "stdio", "start_command": ["python3", "-u", "-c", FIXTURE],
                "startup_timeout_secs": 5, "call_timeout_secs": 5,
                "cwd": self.directory.path(), "env": env
            }))
            .unwrap()
        }

        pub(super) fn path(&self, name: &str) -> String {
            self.directory
                .path()
                .join(name)
                .to_str()
                .unwrap()
                .to_owned()
        }

        #[cfg(unix)]
        pub(super) fn pid(&self) -> libc::pid_t {
            let pid = std::fs::read_to_string(self.path("pid")).expect("fixture wrote its pid");
            pid.parse().expect("fixture pid is numeric")
        }
    }

    // Process handles (manager-owned or explicit test Children) have kill-on-drop
    // fallbacks. Do not kill by saved PID: a reaped PID can be reused.

    pub(super) async fn connect(config: crate::mcp::config::RawMcpServerConfig) -> McpManager {
        connect_admitted(config.try_into().unwrap()).await
    }

    async fn connect_admitted(config: McpServerConfig) -> McpManager {
        let configs = BTreeMap::from([("fixture".into(), config)]);
        let capabilities = CapabilitySet::default();
        let connect = McpManager::connect(&configs, &capabilities, CancellationToken::new());
        tokio::time::timeout(Duration::from_secs(10), connect)
            .await
            .expect("MCP startup is bounded")
    }

    pub(super) async fn fixture_call(
        manager: &McpManager,
        tool: &str,
    ) -> Result<CallToolResult, McpError> {
        manager
            .call("fixture", tool, Map::new(), CancellationToken::new())
            .await
    }

    pub(super) async fn shutdown(manager: &McpManager) {
        // Leave room for the client's five-second graceful-close deadline plus
        // process teardown; this is a deadlock guard, not a timing assertion.
        tokio::time::timeout(Duration::from_secs(10), manager.shutdown())
            .await
            .expect("MCP shutdown is bounded");
    }

    pub(super) async fn wait_for_file(path: &Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fixture received request");
    }

    #[cfg(unix)]
    pub(super) async fn assert_process_reaped(pid: libc::pid_t) {
        tokio::time::timeout(Duration::from_secs(5), async {
            // SAFETY: signal zero only checks whether the recorded child exists.
            while unsafe { libc::kill(pid, 0) } != -1 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let error = std::io::Error::last_os_error().raw_os_error();
            assert_eq!(error, Some(libc::ESRCH));
        })
        .await
        .expect("owned MCP child must be terminated and reaped, not left as a zombie");
    }

    #[tokio::test]
    async fn ineligible_servers_have_no_startup_effects_or_failure_warnings() {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let mut stdio = fixture.config();
        stdio.capabilities = vec![Capability::Read, Capability::Exec];
        let http: McpServerConfig = serde_json::from_value(json!({
            "transport": "streamable_http", "url": format!("http://{address}/mcp"),
            "start_command": stdio.start_command,
            "env": stdio.env, "cwd": stdio.cwd,
            "capabilities": ["read", "exec"],
            "headers_env": {"Authorization": "SKYHOOK_MCP_TEST_MISSING_HEADER_90212829"}
        }))
        .unwrap();
        let configs = BTreeMap::from([
            ("stdio".into(), stdio.try_into().unwrap()),
            ("http".into(), http),
        ]);
        for missing in [Capability::Mcp, Capability::Read, Capability::Exec] {
            let mut capabilities = CapabilitySet::default();
            capabilities.remove(missing);
            let manager =
                McpManager::connect(&configs, &capabilities, CancellationToken::new()).await;
            assert!(manager.catalog().is_empty());
            // Missing header resolution or command startup would produce a warning.
            assert!(manager.warnings().is_empty(), "{:?}", manager.warnings());
            assert!(
                manager
                    .servers()
                    .values()
                    .eq([&McpServerStatus::Skipped; 2])
            );
            assert!(!fixture.directory.path().join("pid").exists());
            let accepted = listener.accept().unwrap_err();
            assert_eq!(accepted.kind(), std::io::ErrorKind::WouldBlock);
            shutdown(&manager).await;
        }
    }

    #[tokio::test]
    async fn discovers_all_pages_and_passes_arguments_cwd_and_env() {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let manager = connect(fixture.config()).await;
        assert!(manager.warnings().is_empty(), "{:?}", manager.warnings());
        let catalog = manager.catalog();
        let names: Vec<_> = catalog
            .iter()
            .map(|entry| entry.tool.name.as_ref())
            .collect();
        assert_eq!(names, ["echo", "rpc_error", "slow", "tool_error"]);

        let arguments = json!({"message": "hello", "nested": {"number": 7}, "list": [true, null]});
        let object = arguments.as_object().unwrap().clone();
        let call = manager.call("fixture", "echo", object, CancellationToken::new());
        let result = serde_json::to_value(call.await.expect("echo call succeeds")).unwrap();
        let payload: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(payload["arguments"], arguments);
        assert_eq!(payload["env"], "configured child value");
        let cwd = std::fs::canonicalize(payload["cwd"].as_str().unwrap()).unwrap();
        assert_eq!(
            cwd,
            std::fs::canonicalize(fixture.directory.path()).unwrap()
        );
        shutdown(&manager).await;
    }

    #[tokio::test]
    async fn call_errors_cancellation_and_unknown_tools_do_not_poison_the_session() {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let manager = connect(fixture.config()).await;
        assert_eq!(manager.catalog().len(), 4, "{:?}", manager.warnings());
        let result = fixture_call(&manager, "tool_error")
            .await
            .expect("isError is a valid MCP result, not a transport failure");
        let result = serde_json::to_value(result).unwrap();
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["text"], "fixture tool error");
        let error = fixture_call(&manager, "rpc_error")
            .await
            .expect_err("JSON-RPC error must fail the call");
        assert!(
            error.to_string().contains("fixture protocol error"),
            "{error}"
        );
        // A failed call must not poison a healthy server session.
        assert!(fixture_call(&manager, "echo").await.is_ok());
        // Pre-cancelled and unknown calls are not sent to the server.
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = manager.call("fixture", "slow", Map::new(), cancel).await;
        assert!(matches!(result, Err(McpError::Cancelled)), "{result:?}");
        assert!(!fixture.directory.path().join("started").exists());
        let result = fixture_call(&manager, "not-advertised").await;
        assert!(
            matches!(result, Err(McpError::UnknownTool { .. })),
            "{result:?}"
        );
        shutdown(&manager).await;
        let result = fixture_call(&manager, "echo").await;
        assert!(matches!(result, Err(McpError::Closed)), "{result:?}");
    }

    #[tokio::test]
    async fn cancelled_and_dropped_inflight_calls_notify_the_server_and_keep_the_session() {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let manager = std::sync::Arc::new(connect(fixture.config()).await);
        let (started, cancelled) = (fixture.path("started"), fixture.path("cancelled"));
        let cancel = CancellationToken::new();
        let call = manager.call("fixture", "slow", Map::new(), cancel.clone());
        let cancel_when_started = async {
            wait_for_file(Path::new(&started)).await;
            cancel.cancel();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(7), async {
            tokio::join!(call, cancel_when_started)
        })
        .await
        .expect("cancellation interrupts the pending tool call");
        assert!(matches!(result, Err(McpError::Cancelled)), "{result:?}");
        wait_for_file(Path::new(&cancelled)).await;
        fixture_call(&manager, "echo").await.unwrap();

        // Dropping the caller's future has the same effect as its token.
        std::fs::remove_file(&started).unwrap();
        std::fs::remove_file(&cancelled).unwrap();
        let call_manager = manager.clone();
        let task = tokio::spawn(async move { fixture_call(&call_manager, "slow").await });
        wait_for_file(Path::new(&started)).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        wait_for_file(Path::new(&cancelled)).await;
        fixture_call(&manager, "echo").await.unwrap();
        shutdown(&manager).await;
    }

    #[tokio::test]
    async fn call_timeout_and_result_size_limits_are_enforced() {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let config = McpServerConfig::try_from(fixture.config()).unwrap();
        let config = config.with_call_timeout(Duration::from_millis(250));
        let manager = connect_admitted(config).await;
        let call = fixture_call(&manager, "slow");
        let result = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .expect("configured timeout must stop a pending call");
        assert!(matches!(result, Err(McpError::Timeout)), "{result:?}");
        // The timed-out call was already sent.
        wait_for_file(&fixture.directory.path().join("started")).await;
        // The session stays usable; under load an echo may itself exceed the short timeout.
        let echo = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match fixture_call(&manager, "echo").await {
                    Err(McpError::Timeout) => {}
                    other => break other,
                }
            }
        });
        let echo = echo.await.expect("session wedged after a timed-out call");
        assert!(echo.is_ok(), "{echo:?}");
        shutdown(&manager).await;
        // Oversized results are rejected without exposing content.
        let mut config = fixture.config();
        config
            .env
            .insert("MCP_TEST_LARGE_RESULT".into(), "1".into());
        let manager = connect(config).await;
        let error = fixture_call(&manager, "echo").await.unwrap_err();
        assert!(
            error.to_string().contains("result exceeds size limit"),
            "{error}"
        );
        shutdown(&manager).await;
    }

    #[tokio::test]
    async fn one_failed_server_does_not_hide_healthy_server_tools() {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let mut bad = fixture.config();
        bad.start_command = Some(vec![fixture.path("nonexistent-executable")]);
        let configs = BTreeMap::from([
            ("broken".into(), bad.try_into().unwrap()),
            ("fixture".into(), fixture.config().try_into().unwrap()),
        ]);
        let capabilities = CapabilitySet::default();
        let connect = McpManager::connect(&configs, &capabilities, CancellationToken::new());
        let manager = tokio::time::timeout(Duration::from_secs(10), connect)
            .await
            .expect("partial startup is bounded");
        assert_eq!(manager.catalog().len(), 4);
        assert!(
            manager
                .catalog()
                .iter()
                .all(|entry| entry.server == "fixture")
        );
        let warnings = manager.warnings();
        assert!(
            warnings.iter().any(|warning| warning.contains("broken")),
            "{warnings:?}"
        );
        let servers = manager.servers();
        assert!(matches!(servers["broken"], McpServerStatus::Failed(_)));
        assert_eq!(servers["fixture"], McpServerStatus::Connected { tools: 4 });
        shutdown(&manager).await;
    }

    #[test]
    fn sdk_transport_error_diagnostics_are_redacted() {
        use rmcp::transport::{DynamicTransportError, async_rw::AsyncRwTransport};
        type FixtureTransport = AsyncRwTransport<RoleClient, tokio::io::Empty, tokio::io::Sink>;
        let secret = "https://user:password@example.invalid/mcp?token=private-secret";
        let error = std::io::Error::other(secret);
        let error = DynamicTransportError::new::<FixtureTransport, RoleClient>(error);
        let rendered = request_error(ServiceError::TransportSend(error)).to_string();
        for leaked in ["private-secret", "password", "example.invalid"] {
            assert!(!rendered.contains(leaked));
        }
    }
}
