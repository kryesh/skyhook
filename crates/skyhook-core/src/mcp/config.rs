//! Trusted, user-configured MCP server connections.

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use serde::Deserialize;

use crate::tool::policy::Capability;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    Stdio,
    StreamableHttp,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "RawMcpServerConfig")]
pub struct McpServerConfig {
    pub transport: McpTransport,
    pub start_command: Option<Vec<String>>,
    pub url: Option<String>,
    /// Every listed capability is required to expose this server's tools.
    pub capabilities: Vec<Capability>,
    pub startup_timeout_secs: u64,
    pub call_timeout_secs: u64,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    /// HTTP header name to environment-variable name (never a literal secret).
    pub headers_env: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMcpServerConfig {
    transport: McpTransport,
    start_command: Option<Vec<String>>,
    url: Option<String>,
    #[serde(default)]
    capabilities: Vec<Capability>,
    #[serde(default = "default_startup_timeout_secs")]
    startup_timeout_secs: u64,
    #[serde(default = "default_call_timeout_secs")]
    call_timeout_secs: u64,
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    headers_env: BTreeMap<String, String>,
}

const fn default_startup_timeout_secs() -> u64 {
    30
}

const fn default_call_timeout_secs() -> u64 {
    120
}

impl TryFrom<RawMcpServerConfig> for McpServerConfig {
    type Error = String;

    fn try_from(raw: RawMcpServerConfig) -> Result<Self, Self::Error> {
        let config = Self {
            transport: raw.transport,
            start_command: raw.start_command,
            url: raw.url,
            capabilities: raw.capabilities,
            startup_timeout_secs: raw.startup_timeout_secs,
            call_timeout_secs: raw.call_timeout_secs,
            cwd: raw.cwd,
            env: raw.env,
            headers_env: raw.headers_env,
        };
        config.validate()?;
        Ok(config)
    }
}

impl McpServerConfig {
    /// Validate without starting processes, connecting, or reading secrets.
    pub fn validate(&self) -> Result<(), String> {
        for (name, seconds) in [
            ("startup_timeout_secs", self.startup_timeout_secs),
            ("call_timeout_secs", self.call_timeout_secs),
        ] {
            if seconds == 0
                || std::time::Instant::now()
                    .checked_add(Duration::from_secs(seconds))
                    .is_none()
            {
                return Err(format!("{name} must be positive and fit a timer deadline"));
            }
        }
        if let Some(command) = &self.start_command
            && command
                .first()
                .is_none_or(|program| program.trim().is_empty())
        {
            return Err("start_command must contain a nonempty executable".to_owned());
        }
        match self.transport {
            McpTransport::Stdio => {
                if self.start_command.is_none() {
                    return Err("stdio transport requires start_command".to_owned());
                }
                if self.url.is_some() || !self.headers_env.is_empty() {
                    return Err("stdio transport does not accept url or headers_env".to_owned());
                }
            }
            McpTransport::StreamableHttp => {
                let url = self
                    .url
                    .as_deref()
                    .ok_or_else(|| "streamable_http transport requires url".to_owned())?;
                let parsed = reqwest::Url::parse(url)
                    .map_err(|error| format!("invalid MCP HTTP url: {error}"))?;
                if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
                    return Err("MCP HTTP url must be an absolute http or https URL".to_owned());
                }
                if self.start_command.is_none() && (self.cwd.is_some() || !self.env.is_empty()) {
                    return Err("cwd and env require start_command".to_owned());
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_connections_are_rejected() {
        for text in [
            "transport = 'stdio'",
            "transport = 'stdio'\nstart_command = []",
            "transport = 'stdio'\nstart_command = ['']",
            "transport = 'stdio'\nstart_command = ['   ']",
            "transport = 'stdio'\nstart_command = ['server']\nurl = 'https://example.com/mcp'",
            "transport = 'stdio'\nstart_command = ['server']\nheaders_env = { Authorization = 'TOKEN' }",
            "transport = 'streamable_http'",
            "transport = 'streamable_http'\nurl = ''",
            "transport = 'streamable_http'\nurl = '/mcp'",
            "transport = 'streamable_http'\nurl = 'ftp://example.com/mcp'",
            "transport = 'streamable_http'\nurl = 'https://example.com/mcp'\nstart_command = []",
            "transport = 'streamable_http'\nurl = 'https://example.com/mcp'\ncwd = 'work'",
            "transport = 'streamable_http'\nurl = 'https://example.com/mcp'\nenv = { MODE = 'test' }",
            "transport = 'sse'\nurl = 'https://example.com/mcp'",
        ] {
            assert!(toml::from_str::<McpServerConfig>(text).is_err(), "{text}");
        }
    }

    #[test]
    fn unknown_fields_capabilities_and_invalid_timeouts_are_rejected() {
        for extra in [
            "capabilities = ['unknown_capability']",
            "capabilities = ['Read']",
            "command = ['other']",
            "startup_timeout_secs = 0",
            "call_timeout_secs = 0",
            "startup_timeout_secs = -1",
            "call_timeout_secs = -1",
            "startup_timeout_secs = 1.5",
            "call_timeout_secs = '120'",
        ] {
            let text = format!("transport = 'stdio'\nstart_command = ['server']\n{extra}");
            assert!(toml::from_str::<McpServerConfig>(&text).is_err(), "{extra}");
        }
        let mut config: McpServerConfig =
            toml::from_str("transport = 'stdio'\nstart_command = ['server']").unwrap();
        config.call_timeout_secs = u64::MAX;
        assert!(config.validate().is_err());
    }
}
