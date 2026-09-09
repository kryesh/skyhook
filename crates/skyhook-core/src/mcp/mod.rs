//! Root-owned Model Context Protocol tools.
//!
//! Servers are configured by the host and discovered once per session. Their
//! tools use the same registry, capability gates and script API as builtins.
//! Remote workers never construct MCP clients or launch MCP processes.

pub(crate) mod adapter;
pub mod config;
pub(crate) mod manager;
pub(crate) mod transport;

pub use config::{McpServerConfig, McpTransport};
