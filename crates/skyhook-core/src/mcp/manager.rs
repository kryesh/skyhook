//! Session-scoped MCP clients and an immutable, startup-discovered tool catalog.
//!
//! Only the root runtime constructs a manager. Children share its `Arc`; they
//! neither initialize servers nor acquire ownership of server processes.

use std::{
    collections::{BTreeMap, HashSet},
    time::Duration,
};

use futures_util::{StreamExt, stream};

use rmcp::{
    Peer, RoleClient,
    model::{
        CallToolRequest, CallToolRequestParams, CallToolResult, ClientRequest,
        PaginatedRequestParams, ServerResult, Tool,
    },
    service::{PeerRequestOptions, RequestHandle, ServiceError},
};
use serde_json::{Map, Value};
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;

use super::{
    config::McpServerConfig,
    transport::{self, Client, OwnedProcess},
};

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
    pub server: String,
    pub tool: Tool,
}

struct Resources {
    client: Client,
    process: Option<OwnedProcess>,
}

impl Resources {
    async fn shutdown(mut self) {
        // Kill/reap owned commands even if remote protocol cleanup stalls.
        if let Some(process) = &mut self.process {
            process.shutdown().await;
        }
        let _ = self.client.close_with_timeout(Duration::from_secs(5)).await;
    }
}

enum ResourceState {
    Running(Box<Resources>),
    Closing(tokio::task::JoinHandle<()>),
    Closed,
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

struct Server {
    peer: Peer<RoleClient>,
    timeout: Duration,
    resources: Mutex<ResourceState>,
    calls: Semaphore,
}

pub struct McpManager {
    catalog: Vec<DiscoveredTool>,
    warnings: Vec<String>,
    servers: BTreeMap<String, Server>,
    closed: CancellationToken,
}

impl McpManager {
    /// Connect ONLY capability-eligible configurations. The root runtime must
    /// filter the map before this call: even resolving headers or spawning a
    /// command for an ineligible server would cross the capability boundary.
    /// An individual failed server is excluded with a warning, not fatal.
    pub async fn connect(
        configs: &BTreeMap<String, McpServerConfig>,
        cancel: CancellationToken,
    ) -> Self {
        let mut manager = Self {
            catalog: Vec::new(),
            warnings: Vec::new(),
            servers: BTreeMap::new(),
            closed: CancellationToken::new(),
        };
        // Own entries before creating futures: no borrowed iterator/closure
        // lifetime leaks into the public Send future (including cross-crate use).
        let entries: Vec<_> = configs
            .iter()
            .map(|(name, config)| (name.clone(), config.clone()))
            .collect();
        let futures: Vec<_> = entries
            .into_iter()
            .map(|(name, config)| connect_one(name, config, cancel.clone()))
            .collect();
        let mut pending = stream::iter(futures).buffer_unordered(4);
        while let Some(result) = pending.next().await {
            let Some((name, result)) = result else {
                continue;
            };
            match result {
                Ok((tools, server)) => {
                    manager
                        .catalog
                        .extend(tools.into_iter().map(|tool| DiscoveredTool {
                            server: name.clone(),
                            tool,
                        }));
                    manager.servers.insert(name, server);
                }
                Err(error) => manager
                    .warnings
                    .push(format!("MCP server {name:?} unavailable: {error}")),
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
        let _permit = tokio::select! {
            biased;
            _ = self.closed.cancelled() => return Err(McpError::Closed),
            _ = cancel.cancelled() => return Err(McpError::Cancelled),
            _ = tokio::time::sleep_until(deadline) => return Err(McpError::Timeout),
            permit = server.calls.acquire() => permit.map_err(|_| McpError::Closed)?,
        };
        if bounded_json_size(&arguments, MAX_RESULT_BYTES).is_none() {
            return Err(McpError::Request("tool arguments exceed size limit".into()));
        }
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(
            CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments),
        ));
        let handle = tokio::select! {
            biased;
            _ = self.closed.cancelled() => return Err(McpError::Closed),
            _ = cancel.cancelled() => return Err(McpError::Cancelled),
            _ = tokio::time::sleep_until(deadline) => return Err(McpError::Timeout),
            result = server.peer.send_cancellable_request(request, PeerRequestOptions::default()) => result.map_err(request_error)?,
        };
        let mut handle = CancelOnDrop(Some(handle));
        let result = tokio::select! {
            biased;
            _ = self.closed.cancelled() => Err(McpError::Closed),
            _ = cancel.cancelled() => Err(McpError::Cancelled),
            _ = tokio::time::sleep_until(deadline) => Err(McpError::Timeout),
            response = &mut handle.0.as_mut().expect("active request").rx => match response {
                Ok(Ok(ServerResult::CallToolResult(result))) => {
                    if bounded_json_size(&result, MAX_RESULT_BYTES).is_none() {
                        Err(McpError::Request("tool result exceeds size limit".into()))
                    } else { Ok(result) }
                },
                Ok(Ok(_)) => Err(McpError::Request("unexpected response to tools/call".into())),
                Ok(Err(error)) => Err(request_error(error)),
                Err(_) => Err(McpError::Request("transport closed".into())),
            },
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
            // Store the teardown task before awaiting it. A cancelled shutdown
            // waiter releases the lock but does not lose kill/reap ownership;
            // another caller can still join the same cleanup task.
            let mut state = server.resources.lock().await;
            if matches!(*state, ResourceState::Running(_)) {
                let ResourceState::Running(resources) =
                    std::mem::replace(&mut *state, ResourceState::Closed)
                else {
                    unreachable!()
                };
                *state = ResourceState::Closing(tokio::spawn(resources.shutdown()));
            }
            if let ResourceState::Closing(task) = &mut *state {
                let _ = task.await;
                *state = ResourceState::Closed;
            }
        }
    }
}

impl Drop for McpManager {
    fn drop(&mut self) {
        self.closed.cancel();
    }
}

async fn connect_one(
    name: String,
    config: McpServerConfig,
    cancel: CancellationToken,
) -> Option<(String, Result<(Vec<Tool>, Server), McpError>)> {
    if cancel.is_cancelled() {
        return None;
    }
    if let Err(error) = config.validate() {
        return Some((name.clone(), Err(McpError::Configuration(error))));
    }
    let mut process = None;
    let mut client = None;
    let startup = async {
        client = Some(transport::connect(&config, &mut process).await?);
        discover(client.as_ref().expect("connected client")).await
    };
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(McpError::Cancelled),
        result = tokio::time::timeout(Duration::from_secs(config.startup_timeout_secs), startup) => {
            result.unwrap_or(Err(McpError::Timeout))
        }
    };
    match result {
        Ok(tools) => {
            let client = client.take().expect("discovery needs client");
            let peer = client.peer().clone();
            Some((
                name.clone(),
                Ok((
                    tools,
                    Server {
                        peer,
                        timeout: Duration::from_secs(config.call_timeout_secs),
                        resources: Mutex::new(ResourceState::Running(Box::new(Resources {
                            client,
                            process,
                        }))),
                        calls: Semaphore::new(16),
                    },
                )),
            ))
        }
        Err(error) => {
            if let Some(mut client) = client {
                let _ = client.close_with_timeout(Duration::from_secs(5)).await;
            }
            if let Some(mut process) = process {
                process.shutdown().await;
            }
            Some((name.clone(), Err(error)))
        }
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

const MAX_TOOLS: usize = 1024;
const MAX_TOOL_BYTES: usize = 256 * 1024;
const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESULT_BYTES: usize = 4 * 1024 * 1024;

// Count encoded bytes without allocating another copy of a potentially large
// schema/result. These post-parse bounds complement the raw transport bound.
fn bounded_json_size(value: &impl serde::Serialize, limit: usize) -> Option<usize> {
    struct Counter {
        bytes: usize,
        limit: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            if data.len() > self.limit.saturating_sub(self.bytes) {
                return Err(std::io::Error::other("MCP JSON size limit exceeded"));
            }
            self.bytes += data.len();
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, value).ok()?;
    Some(counter.bytes)
}

async fn discover(client: &Client) -> Result<Vec<Tool>, McpError> {
    // A resources/prompts-only server must not receive unsupported tools/list.
    if client
        .peer_info()
        .is_none_or(|info| info.capabilities.tools.is_none())
    {
        return Ok(Vec::new());
    }
    let mut tools = Vec::new();
    let mut bytes = 0;
    let mut names = HashSet::new();
    let mut cursors = HashSet::new();
    let mut cursor = None;
    // A broken server must not exhaust memory by generating endless cursors.
    for _ in 0..1000 {
        let params = cursor
            .take()
            .map(|cursor| PaginatedRequestParams::default().with_cursor(Some(cursor)));
        let page = client
            .list_tools(params)
            .await
            .map_err(|_| McpError::Startup("tools/list failed".into()))?;
        for tool in page.tools {
            let size = bounded_json_size(&tool, MAX_TOOL_BYTES).ok_or_else(|| {
                McpError::Startup("tool schema/metadata exceeds size limit".into())
            })?;
            bytes += size;
            if tools.len() >= MAX_TOOLS || bytes > MAX_CATALOG_BYTES {
                return Err(McpError::Startup("tool catalog exceeds size limit".into()));
            }
            if !names.insert(tool.name.to_string()) {
                return Err(McpError::Startup("duplicate tool name in discovery".into()));
            }
            tools.push(tool);
        }
        let Some(next) = page.next_cursor else {
            return Ok(tools);
        };
        if next.len() > 4096 {
            return Err(McpError::Startup(
                "tools/list cursor exceeds size limit".into(),
            ));
        }
        if !cursors.insert(next.clone()) {
            return Err(McpError::Startup("repeated tools/list cursor".into()));
        }
        cursor = Some(next);
    }
    Err(McpError::Startup(
        "tools/list pagination limit exceeded".into(),
    ))
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
