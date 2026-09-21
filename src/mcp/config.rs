use crate::tool::policy::Capability;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    Stdio,
    StreamableHttp,
}

/// An admitted connection. Mutable input is represented by [`RawMcpServerConfig`];
/// deserialize or use `TryFrom` to validate it without startup or secret lookup.
/// Serialization retains the original flat configuration shape and URL spelling.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "RawMcpServerConfig", into = "RawMcpServerConfig")]
pub struct McpServerConfig {
    connection: McpConnection,
    capabilities: Vec<Capability>,
    startup_timeout: Duration,
    call_timeout: Duration,
}

#[derive(Clone, Debug)]
pub(super) enum McpConnection {
    Stdio(CommandSpec),
    Http {
        endpoint: String,
        headers: BTreeMap<String, String>,
        start_if_unreachable: Option<CommandSpec>,
    },
}

/// Strings remain exactly those supplied by YAML: executable/arguments are never
/// split, trimmed, shell-expanded, or normalized. Only executable shape is checked.
#[derive(Clone, Debug)]
pub(super) struct CommandSpec {
    pub(super) argv: Vec<String>,
    pub(super) cwd: Option<PathBuf>,
    pub(super) env: BTreeMap<String, String>,
}

/// Editable external DTO. Conversion into [`McpServerConfig`] is mandatory before
/// connecting; this type cannot be passed to transport or manager startup APIs.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawMcpServerConfig {
    pub transport: McpTransport,
    pub start_command: Option<Vec<String>>,
    pub url: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    #[serde(default = "default_startup_timeout_secs")]
    pub startup_timeout_secs: u64,
    #[serde(default = "default_call_timeout_secs")]
    pub call_timeout_secs: u64,
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Header names mapped to environment-variable names, never resolved here.
    #[serde(default)]
    pub headers_env: BTreeMap<String, String>,
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
        for (name, seconds) in [
            ("startup_timeout_secs", raw.startup_timeout_secs),
            ("call_timeout_secs", raw.call_timeout_secs),
        ] {
            if seconds == 0
                || std::time::Instant::now()
                    .checked_add(Duration::from_secs(seconds))
                    .is_none()
            {
                return Err(format!("{name} must be positive and fit a timer deadline"));
            }
        }
        if let Some(command) = &raw.start_command
            && command
                .first()
                .is_none_or(|program| program.trim().is_empty())
        {
            return Err("start_command must contain a nonempty executable".to_owned());
        }
        let connection = match raw.transport {
            McpTransport::Stdio => {
                let argv = raw
                    .start_command
                    .ok_or("stdio transport requires start_command")?;
                if raw.url.is_some() || !raw.headers_env.is_empty() {
                    return Err("stdio transport does not accept url or headers_env".to_owned());
                }
                McpConnection::Stdio(CommandSpec {
                    argv,
                    cwd: raw.cwd,
                    env: raw.env,
                })
            }
            McpTransport::StreamableHttp => {
                let original = raw.url.ok_or("streamable_http transport requires url")?;
                let parsed = reqwest::Url::parse(&original)
                    .map_err(|error| format!("invalid MCP HTTP url: {error}"))?;
                if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
                    return Err("MCP HTTP url must be an absolute http or https URL".to_owned());
                }
                if raw.start_command.is_none() && (raw.cwd.is_some() || !raw.env.is_empty()) {
                    return Err("cwd and env require start_command".to_owned());
                }
                McpConnection::Http {
                    endpoint: original,
                    headers: raw.headers_env,
                    start_if_unreachable: raw.start_command.map(|argv| CommandSpec {
                        argv,
                        cwd: raw.cwd,
                        env: raw.env,
                    }),
                }
            }
        };
        Ok(Self {
            connection,
            capabilities: raw.capabilities,
            startup_timeout: Duration::from_secs(raw.startup_timeout_secs),
            call_timeout: Duration::from_secs(raw.call_timeout_secs),
        })
    }
}

impl McpServerConfig {
    pub(super) fn connection(&self) -> &McpConnection {
        &self.connection
    }
    pub fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }
    pub fn startup_timeout(&self) -> Duration {
        self.startup_timeout
    }
    pub fn call_timeout(&self) -> Duration {
        self.call_timeout
    }
    #[cfg(test)]
    pub(crate) fn with_call_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = timeout;
        self
    }
    pub fn transport(&self) -> McpTransport {
        match self.connection {
            McpConnection::Stdio(_) => McpTransport::Stdio,
            McpConnection::Http { .. } => McpTransport::StreamableHttp,
        }
    }
}

impl From<McpServerConfig> for RawMcpServerConfig {
    fn from(config: McpServerConfig) -> Self {
        let transport = config.transport();
        let (command, url, headers_env) = match config.connection {
            McpConnection::Stdio(command) => (Some(command), None, BTreeMap::new()),
            McpConnection::Http {
                endpoint,
                headers,
                start_if_unreachable,
            } => (start_if_unreachable, Some(endpoint), headers),
        };
        let (start_command, cwd, env) = match command {
            Some(command) => (Some(command.argv), command.cwd, command.env),
            None => (None, None, BTreeMap::new()),
        };
        Self {
            transport,
            start_command,
            url,
            capabilities: config.capabilities,
            startup_timeout_secs: config.startup_timeout.as_secs(),
            call_timeout_secs: config.call_timeout.as_secs(),
            cwd,
            env,
            headers_env,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const STDIO: &str = "transport: 'stdio'\nstart_command: ['server']";

    #[test]
    fn flat_config_roundtrips_without_changing_defaults_or_endpoint_semantics() {
        for text in [
            "transport: 'stdio'\nstart_command: [' server name ', 'a b', '$UNEXPANDED']",
            "transport: 'streamable_http'\nurl: 'HTTPS://Example.COM:443/a%2Fb?q=x#fragment'",
            "transport: 'streamable_http'\nurl: 'http://user:password@localhost:1234/mcp'",
            "transport: 'streamable_http'\nurl: 'http://[::1]:1234/mcp'\nstart_command: ['server', '--flag']\ncwd: 'relative/path'\nenv: { KEY: 'value' }\nheaders_env: { Authorization: 'MISSING_MCP_TEST_TOKEN' }\ncapabilities: ['read', 'exec']\nstartup_timeout_secs: 2\ncall_timeout_secs: 9",
        ] {
            let raw: RawMcpServerConfig = crate::yaml::parse(text).unwrap();
            let expected = serde_json::to_value(&raw).unwrap();
            let config: McpServerConfig = crate::yaml::parse(text).unwrap();
            assert_eq!(serde_json::to_value(&config).unwrap(), expected);
            let encoded = serde_saphyr::to_string(&config).unwrap();
            let decoded: McpServerConfig = crate::yaml::parse(&encoded).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
            assert_eq!(config.transport(), raw.transport);
        }
        let config: McpServerConfig = crate::yaml::parse(STDIO).unwrap();
        assert_eq!(config.startup_timeout(), Duration::from_secs(30));
        assert_eq!(config.call_timeout(), Duration::from_secs(120));
        assert!(config.capabilities().is_empty());
    }

    #[test]
    fn admission_is_pure_and_retains_runtime_environment_failures() {
        // Existence/access and secret/header-value validity remain startup concerns.
        let config: McpServerConfig = crate::yaml::parse("transport: 'streamable_http'\nurl: 'http://localhost:1234/mcp'\nstart_command: ['/nonexistent/mcp-command']\ncwd: '/nonexistent/mcp-directory'\nheaders_env: { 'invalid header name': 'MISSING_MCP_TEST_TOKEN' }").unwrap();
        let mut raw: RawMcpServerConfig = config.into();
        assert_eq!(
            raw.headers_env["invalid header name"],
            "MISSING_MCP_TEST_TOKEN"
        );
        let cwd = Some(Path::new("/nonexistent/mcp-directory"));
        assert_eq!(raw.cwd.as_deref(), cwd);
        raw.start_command = None;
        assert!(McpServerConfig::try_from(raw).is_err());
    }

    #[test]
    fn programmatic_timeouts_and_commands_must_pass_the_same_admission() {
        let raw: RawMcpServerConfig = crate::yaml::parse(STDIO).unwrap();
        for seconds in [0, u64::MAX] {
            let mut startup = raw.clone();
            startup.startup_timeout_secs = seconds;
            assert!(McpServerConfig::try_from(startup).is_err());
            let mut call = raw.clone();
            call.call_timeout_secs = seconds;
            assert!(McpServerConfig::try_from(call).is_err());
        }
        for command in [None, Some(vec![]), Some(vec![" ".into()])] {
            let mut invalid = raw.clone();
            invalid.start_command = command;
            assert!(McpServerConfig::try_from(invalid).is_err());
        }
    }

    #[test]
    fn invalid_connections_unknown_fields_capabilities_and_timeouts_are_rejected() {
        let mut texts: Vec<String> = [
            "transport: 'stdio'",
            "transport: 'stdio'\nstart_command: []",
            "transport: 'stdio'\nstart_command: ['']",
            "transport: 'stdio'\nstart_command: ['   ']",
            "transport: 'stdio'\nstart_command: ['server']\nurl: 'https://example.com/mcp'",
            "transport: 'stdio'\nstart_command: ['server']\nheaders_env: { Authorization: 'TOKEN' }",
            "transport: 'streamable_http'",
            "transport: 'streamable_http'\nurl: ''",
            "transport: 'streamable_http'\nurl: '/mcp'",
            "transport: 'streamable_http'\nurl: 'ftp://example.com/mcp'",
            "transport: 'streamable_http'\nurl: 'https://example.com/mcp'\nstart_command: []",
            "transport: 'streamable_http'\nurl: 'https://example.com/mcp'\ncwd: 'work'",
            "transport: 'streamable_http'\nurl: 'https://example.com/mcp'\nenv: { MODE: 'test' }",
            "transport: 'sse'\nurl: 'https://example.com/mcp'",
        ]
        .map(String::from)
        .into();
        for extra in [
            "capabilities: ['unknown_capability']",
            "capabilities: ['Read']",
            "command: ['other']",
            "startup_timeout_secs: 0",
            "call_timeout_secs: 0",
            "startup_timeout_secs: -1",
            "call_timeout_secs: -1",
            "startup_timeout_secs: 1.5",
            "call_timeout_secs: '120'",
        ] {
            texts.push(format!("{STDIO}\n{extra}"));
        }
        for text in texts {
            assert!(
                crate::yaml::parse::<McpServerConfig>(&text).is_err(),
                "{text}"
            );
        }
    }
}
