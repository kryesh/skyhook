//! Bounded startup-only tool discovery, including pagination and capability negotiation.
use super::super::transport::Client;
use super::McpError;
use crate::tool::{
    ToolError,
    diagnostic::{Operation, PartialContext, Subject},
};
use rmcp::model::{PaginatedRequestParams, Tool};
use std::collections::HashSet;

const MAX_TOOLS: usize = 1024;
const MAX_TOOL_BYTES: usize = 256 * 1024;
const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;

// Count encoded bytes without allocating another copy of a potentially large
// schema/result. These post-parse bounds complement the raw transport bound.
pub(super) fn bounded_json_size(value: &impl serde::Serialize, limit: usize) -> Option<usize> {
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

pub(super) async fn discover(client: &Client) -> Result<Vec<Tool>, ToolError> {
    // Keep the known discovery operation separate from the sanitized SDK cause.
    let failure = |error: McpError| {
        ToolError::from(error).context(PartialContext::new(
            Operation::Read,
            Subject::Label("MCP tools/list discovery".into()),
        ))
    };
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
            .map_err(|error| failure(super::request_error(error)))?;
        for tool in page.tools {
            let size = bounded_json_size(&tool, MAX_TOOL_BYTES).ok_or_else(|| {
                failure(McpError::Startup(
                    "tool schema/metadata exceeds size limit".into(),
                ))
            })?;
            bytes += size;
            if tools.len() >= MAX_TOOLS || bytes > MAX_CATALOG_BYTES {
                return Err(failure(McpError::Startup(
                    "tool catalog exceeds size limit".into(),
                )));
            }
            if !names.insert(tool.name.to_string()) {
                return Err(failure(McpError::Startup(
                    "duplicate tool name in discovery".into(),
                )));
            }
            tools.push(tool);
        }
        let Some(next) = page.next_cursor else {
            return Ok(tools);
        };
        if next.len() > 4096 {
            return Err(failure(McpError::Startup(
                "tools/list cursor exceeds size limit".into(),
            )));
        }
        if !cursors.insert(next.clone()) {
            return Err(failure(McpError::Startup(
                "repeated tools/list cursor".into(),
            )));
        }
        cursor = Some(next);
    }
    Err(failure(McpError::Startup(
        "tools/list pagination limit exceeded".into(),
    )))
}

#[cfg(test)]
mod tests {
    use super::super::McpManager;
    #[cfg(unix)]
    use super::super::tests::assert_process_reaped;
    use super::super::tests::{Fixture, connect, shutdown};
    use super::*;
    use crate::tool::diagnostic::Cause;
    use serde_json::{Value, json};
    use std::time::Duration;

    #[tokio::test]
    async fn discovery_limits_skip_and_reap_unusable_servers() {
        let modes = ["oversized_schema", "many_tools", "repeated_cursor"];
        let Some(fixtures) = modes.map(|_| Fixture::new()).into_iter().collect() else {
            return;
        };
        let fixtures: Vec<Fixture> = fixtures;
        let configs = std::iter::zip(modes, &fixtures)
            .map(|(mode, fixture)| {
                let mut config = fixture.config();
                config.env.insert("MCP_TEST_DISCOVERY".into(), mode.into());
                (mode.to_owned(), config.try_into().unwrap())
            })
            .collect();
        let manager = McpManager::connect(&configs, &Default::default(), Default::default()).await;
        assert!(manager.catalog().is_empty());
        assert_eq!(manager.warnings().len(), 3, "{:?}", manager.warnings());
        #[cfg(unix)]
        for fixture in &fixtures {
            assert_process_reaped(fixture.pid()).await;
        }
        shutdown(&manager).await;

        // A failed tools/list also rejects the server, retaining discovery context
        // and the safe protocol code rather than the server's message or payload.
        let fixture = &fixtures[0];
        let mut config = fixture.config();
        config
            .env
            .insert("MCP_TEST_DISCOVERY".into(), "rpc_error".into());
        let connected = tokio::time::timeout(
            Duration::from_secs(10),
            super::super::connection::connect_one(
                "fixture".into(),
                config.try_into().unwrap(),
                Default::default(),
            ),
        )
        .await
        .expect("discovery failure and cleanup are bounded")
        .expect("server startup was attempted");
        let Err(error) = connected.1 else {
            panic!("failed tools/list must reject the server");
        };
        let diagnostic = error.diagnostic();
        assert_eq!(
            diagnostic.context,
            PartialContext::new(
                Operation::Read,
                Subject::Label("MCP tools/list discovery".into()),
            )
            .resolve(),
        );
        assert_eq!(
            diagnostic.cause,
            Cause::Message("server JSON-RPC error -32602".into()),
        );
        let rendered = error.to_string();
        assert!(rendered.contains("tools/list discovery"), "{rendered}");
        assert!(rendered.contains("-32602"), "{rendered}");
        for secret in ["private-discovery-message", "private-discovery-payload"] {
            assert!(!rendered.contains(secret), "{rendered}");
        }
        #[cfg(unix)]
        assert_process_reaped(fixture.pid()).await;
    }

    #[test]
    fn bounded_serialization_counts_without_unbounded_output_copy() {
        assert_eq!(bounded_json_size(&json!({"a": "b"}), 9), Some(9));
        assert_eq!(bounded_json_size(&json!({"a": "b"}), 8), None);
    }

    #[tokio::test]
    async fn resources_only_server_gets_no_tools_requests_or_extra_client_capabilities() {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let mut config = fixture.config();
        config.env.insert("MCP_TEST_NO_TOOLS".into(), "1".into());
        let manager = connect(config).await;
        assert!(manager.catalog().is_empty());
        assert!(manager.warnings().is_empty(), "{:?}", manager.warnings());
        assert!(!fixture.directory.path().join("list").exists());
        let initialize: Value =
            serde_json::from_str(&std::fs::read_to_string(fixture.path("initialize")).unwrap())
                .unwrap();
        let capabilities = initialize["capabilities"].as_object().unwrap();
        assert!(!capabilities.contains_key("sampling"));
        assert!(!capabilities.contains_key("elicitation"));
        assert!(!capabilities.contains_key("roots"));
        shutdown(&manager).await;
    }
}
