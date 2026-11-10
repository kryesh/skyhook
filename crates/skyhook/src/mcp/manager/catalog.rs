//! Bounded startup-only tool discovery, including pagination and capability negotiation.
use super::super::transport::Client;
use super::StartupError;
use rmcp::model::{PaginatedRequestParams, Tool};
use std::collections::HashSet;

pub(super) const MAX_TOOLS: usize = 1024;
pub(super) const MAX_TOOL_BYTES: usize = 256 * 1024;
const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;
/// A broken server must not exhaust memory by generating endless cursors.
const MAX_PAGES: usize = 1000;
const MAX_CURSOR_BYTES: usize = 4096;

/// A discovery bound a server's catalog broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DiscoveryLimit {
    #[error("tool schema/metadata exceeds size limit")]
    ToolSize,
    #[error("tool catalog exceeds size limit")]
    CatalogSize,
    #[error("duplicate tool name in discovery")]
    DuplicateName,
    #[error("tools/list cursor exceeds size limit")]
    CursorSize,
    #[error("repeated tools/list cursor")]
    RepeatedCursor,
    #[error("tools/list pagination limit exceeded")]
    Pages,
}

// Count encoded bytes without allocating another copy of a potentially large
// schema/result. These post-parse bounds complement the raw transport bound.
pub(super) fn bounded_json_size(value: &impl serde::Serialize, limit: usize) -> Option<usize> {
    let mut counter = skyhook_base::limits::ByteCount { bytes: 0, limit };
    serde_json::to_writer(&mut counter, value).ok()?;
    Some(counter.bytes)
}

pub(super) async fn discover(client: &Client) -> Result<Vec<Tool>, StartupError> {
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
    for _ in 0..MAX_PAGES {
        let params = cursor
            .take()
            .map(|cursor| PaginatedRequestParams::default().with_cursor(Some(cursor)));
        let page = client
            .list_tools(params)
            .await
            .map_err(|error| StartupError::Discovery(super::request_error(error)))?;
        for tool in page.tools {
            bytes += bounded_json_size(&tool, MAX_TOOL_BYTES).ok_or(DiscoveryLimit::ToolSize)?;
            if tools.len() >= MAX_TOOLS || bytes > MAX_CATALOG_BYTES {
                return Err(DiscoveryLimit::CatalogSize.into());
            }
            if !names.insert(tool.name.to_string()) {
                return Err(DiscoveryLimit::DuplicateName.into());
            }
            tools.push(tool);
        }
        let Some(next) = page.next_cursor else {
            return Ok(tools);
        };
        if next.len() > MAX_CURSOR_BYTES {
            return Err(DiscoveryLimit::CursorSize.into());
        }
        if !cursors.insert(next.clone()) {
            return Err(DiscoveryLimit::RepeatedCursor.into());
        }
        cursor = Some(next);
    }
    Err(DiscoveryLimit::Pages.into())
}

#[cfg(test)]
mod tests {
    use super::super::tests::assert_process_reaped;
    use super::super::tests::{Fixture, failure, shutdown};
    use super::super::{McpError, McpManager};
    use super::*;
    use crate::tests::bounded;
    use rmcp::ServiceExt;
    use serde_json::{Value, json};
    use std::assert_matches;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    /// Discovery from an in-process server that advertises `tools` and answers every
    /// tools/list with `listed`; returns what discovery found and the requests it saw.
    async fn discovered(
        tools: bool,
        listed: Value,
    ) -> (Result<Vec<Tool>, StartupError>, Vec<Value>) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(server);
            let (mut lines, mut requests) = (tokio::io::BufReader::new(read).lines(), Vec::new());
            while let Some(line) = lines.next_line().await.unwrap() {
                let request: Value = serde_json::from_str(&line).unwrap();
                requests.push(request.clone());
                let Some(id) = request.get("id") else {
                    continue;
                };
                let mut reply = match request["method"].as_str() {
                    Some("initialize") => json!({"result": {
                        "protocolVersion": request["params"]["protocolVersion"],
                        "capabilities": if tools { json!({"tools": {}}) } else { json!({"resources": {}}) },
                        "serverInfo": {"name": "fixture", "version": "1"},
                    }}),
                    _ => listed.clone(),
                };
                (reply["jsonrpc"], reply["id"]) = ("2.0".into(), id.clone());
                let reply = format!("{reply}\n");
                // Discovery may stop reading once it fails.
                if write.write_all(reply.as_bytes()).await.is_err() {
                    break;
                }
            }
            requests
        });
        let (read, write) = tokio::io::split(client);
        let transport = rmcp::transport::async_rw::AsyncRwTransport::new_client(read, write);
        let client = bounded(().serve(transport)).await.unwrap();
        let result = bounded(discover(&client)).await;
        drop(client);
        (result, bounded(server).await.unwrap())
    }

    #[tokio::test]
    async fn discovery_limits_skip_and_reap_unusable_servers() {
        let tool = |name: String| json!({"name": name, "inputSchema": {"type": "object"}});
        let huge = json!({"name": "huge", "description": "x".repeat(MAX_TOOL_BYTES),
            "inputSchema": {"type": "object"}});
        let many: Vec<_> = (0..=MAX_TOOLS)
            .map(|index| tool(format!("tool_{index}")))
            .collect();
        for (listed, limit) in [
            (json!({"tools": [huge]}), DiscoveryLimit::ToolSize),
            (json!({"tools": many}), DiscoveryLimit::CatalogSize),
            (
                json!({"tools": [], "nextCursor": "repeat"}),
                DiscoveryLimit::RepeatedCursor,
            ),
        ] {
            let (result, _) = discovered(true, json!({"result": listed})).await;
            assert_matches!(result, Err(StartupError::Limit(found)) if found == limit);
        }
        // A failed tools/list also rejects the server, retaining discovery context.
        let error = json!({"error": {"code": -32602, "message": "fixture discovery error"}});
        let (result, _) = discovered(true, error).await;
        assert_matches!(
            result,
            Err(StartupError::Discovery(McpError::JsonRpc(-32602))),
            "{result:?}"
        );
        // A server rejected at discovery is skipped and its process reaped.
        let fixture = Fixture::new();
        let mut config = fixture.config();
        let repeated = "repeated_cursor";
        config["env"]["MCP_TEST_DISCOVERY"] = repeated.into();
        let configs = [(
            repeated.to_owned(),
            crate::mcp::manager::tests::server(config),
        )]
        .into();
        let manager = McpManager::connect(
            &configs,
            &Default::default(),
            &std::env::temp_dir(),
            Default::default(),
        )
        .await;
        assert!(manager.catalog().is_empty());
        let limit = DiscoveryLimit::RepeatedCursor;
        assert_matches!(*failure(&manager, repeated), StartupError::Limit(l) if l == limit);
        assert_process_reaped(fixture.pid()).await;
        shutdown(&manager).await;
    }

    #[test]
    fn bounded_serialization_counts_without_unbounded_output_copy() {
        assert_eq!(bounded_json_size(&json!({"a": "b"}), 9), Some(9));
        assert_eq!(bounded_json_size(&json!({"a": "b"}), 8), None);
    }

    #[tokio::test]
    async fn resources_only_server_gets_no_tools_requests() {
        let (result, requests) = discovered(false, Value::Null).await;
        assert!(result.unwrap().is_empty());
        let methods: Vec<_> = (requests.iter())
            .filter_map(|request| request["method"].as_str())
            .collect();
        assert!(!methods.contains(&"tools/list"), "{methods:?}");
    }
}
