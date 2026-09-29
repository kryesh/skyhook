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
    #[cfg(unix)]
    use super::super::tests::assert_process_reaped;
    use super::super::tests::{Fixture, connect, failure, shutdown};
    use super::super::{McpError, McpManager};
    use super::*;
    use crate::tests::bounded;
    use serde_json::{Value, json};

    #[tokio::test]
    async fn discovery_limits_skip_and_reap_unusable_servers() {
        const LIMITS: [(&str, DiscoveryLimit); 3] = [
            ("oversized_schema", DiscoveryLimit::ToolSize),
            ("many_tools", DiscoveryLimit::CatalogSize),
            ("repeated_cursor", DiscoveryLimit::RepeatedCursor),
        ];
        let modes = LIMITS.map(|(mode, _)| mode);
        let Some(fixtures) = Fixture::batch::<{ LIMITS.len() }>() else {
            return;
        };
        let configs = std::iter::zip(modes, &fixtures)
            .map(|(mode, fixture)| {
                let mut config = fixture.config();
                config.env.insert("MCP_TEST_DISCOVERY".into(), mode.into());
                (mode.to_owned(), config.try_into().unwrap())
            })
            .collect();
        let manager = McpManager::connect(&configs, &Default::default(), Default::default()).await;
        assert!(manager.catalog().is_empty());
        for (mode, limit) in LIMITS {
            assert!(matches!(*failure(&manager, mode), StartupError::Limit(l) if l == limit));
        }
        #[cfg(unix)]
        for fixture in &fixtures {
            assert_process_reaped(fixture.pid()).await;
        }
        shutdown(&manager).await;

        // A failed tools/list also rejects the server, retaining discovery context.
        let fixture = &fixtures[0];
        let mut config = fixture.config();
        config
            .env
            .insert("MCP_TEST_DISCOVERY".into(), "rpc_error".into());
        let connect = super::super::connection::connect_one(
            config.try_into().unwrap(),
            Default::default(),
            std::future::pending(),
        );
        let Some(Err(error)) = bounded(connect).await else {
            panic!("failed tools/list must reject the server");
        };
        assert!(
            matches!(error, StartupError::Discovery(McpError::JsonRpc(-32602))),
            "{error}"
        );
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
