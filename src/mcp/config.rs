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

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum McpConfigError {
    #[error("startup_timeout_secs must be positive and fit a timer deadline")]
    StartupTimeout,
    #[error("call_timeout_secs must be positive and fit a timer deadline")]
    CallTimeout,
    #[error("start_command must contain a nonempty executable")]
    EmptyCommand,
    #[error("stdio transport requires start_command")]
    MissingCommand,
    #[error("stdio transport does not accept url or headers_env")]
    StdioEndpoint,
    #[error("streamable_http transport requires url")]
    MissingUrl,
    #[error("MCP HTTP url must be an absolute http or https URL")]
    Url,
    #[error("cwd and env require start_command")]
    LaunchWithoutCommand,
}

impl TryFrom<RawMcpServerConfig> for McpServerConfig {
    type Error = McpConfigError;
    fn try_from(raw: RawMcpServerConfig) -> Result<Self, Self::Error> {
        let timeout = |seconds, error| {
            let timeout = Duration::from_secs(seconds);
            let fits = std::time::Instant::now().checked_add(timeout).is_some();
            (seconds > 0 && fits).then_some(timeout).ok_or(error)
        };
        let startup_timeout = timeout(raw.startup_timeout_secs, McpConfigError::StartupTimeout)?;
        let call_timeout = timeout(raw.call_timeout_secs, McpConfigError::CallTimeout)?;
        if let Some(command) = &raw.start_command
            && command
                .first()
                .is_none_or(|program| program.trim().is_empty())
        {
            return Err(McpConfigError::EmptyCommand);
        }
        let connection = match raw.transport {
            McpTransport::Stdio => {
                let argv = raw.start_command.ok_or(McpConfigError::MissingCommand)?;
                if raw.url.is_some() || !raw.headers_env.is_empty() {
                    return Err(McpConfigError::StdioEndpoint);
                }
                McpConnection::Stdio(CommandSpec {
                    argv,
                    cwd: raw.cwd,
                    env: raw.env,
                })
            }
            McpTransport::StreamableHttp => {
                let original = raw.url.ok_or(McpConfigError::MissingUrl)?;
                let absolute = reqwest::Url::parse(&original).is_ok_and(|url| {
                    matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
                });
                if !absolute {
                    return Err(McpConfigError::Url);
                }
                if raw.start_command.is_none() && (raw.cwd.is_some() || !raw.env.is_empty()) {
                    return Err(McpConfigError::LaunchWithoutCommand);
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
            startup_timeout,
            call_timeout,
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
        let error = McpServerConfig::try_from(raw).unwrap_err();
        assert_eq!(error, McpConfigError::LaunchWithoutCommand);
    }

    #[test]
    fn admission_rejects_invalid_connections_commands_and_timeouts() {
        use McpConfigError::*;
        let http = "transport: 'streamable_http'\nurl: 'https://example.com/mcp'";
        let cases = [
            ("transport: 'stdio'".into(), MissingCommand),
            ("transport: 'stdio'\nstart_command: []".into(), EmptyCommand),
            (
                "transport: 'stdio'\nstart_command: ['']".into(),
                EmptyCommand,
            ),
            (
                "transport: 'stdio'\nstart_command: ['   ']".into(),
                EmptyCommand,
            ),
            (
                format!("{STDIO}\nurl: 'https://example.com/mcp'"),
                StdioEndpoint,
            ),
            (
                format!("{STDIO}\nheaders_env: {{ Authorization: 'TOKEN' }}"),
                StdioEndpoint,
            ),
            ("transport: 'streamable_http'".into(), MissingUrl),
            ("transport: 'streamable_http'\nurl: ''".into(), Url),
            ("transport: 'streamable_http'\nurl: '/mcp'".into(), Url),
            (
                "transport: 'streamable_http'\nurl: 'ftp://example.com/mcp'".into(),
                Url,
            ),
            (format!("{http}\nstart_command: []"), EmptyCommand),
            (format!("{http}\ncwd: 'work'"), LaunchWithoutCommand),
            (
                format!("{http}\nenv: {{ MODE: 'test' }}"),
                LaunchWithoutCommand,
            ),
            (format!("{STDIO}\nstartup_timeout_secs: 0"), StartupTimeout),
            (format!("{STDIO}\ncall_timeout_secs: 0"), CallTimeout),
            (
                format!("{STDIO}\nstartup_timeout_secs: {}", u64::MAX),
                StartupTimeout,
            ),
            (
                format!("{STDIO}\ncall_timeout_secs: {}", u64::MAX),
                CallTimeout,
            ),
        ];
        for (text, expected) in cases {
            let raw: RawMcpServerConfig = crate::yaml::parse(&text).unwrap();
            let error = McpServerConfig::try_from(raw).unwrap_err();
            assert_eq!(error, expected, "{text}");
        }
        let extras = [
            "capabilities: ['unknown_capability']",
            "capabilities: ['Read']",
            "command: ['other']",
            "startup_timeout_secs: -1",
            "call_timeout_secs: -1",
            "startup_timeout_secs: 1.5",
            "call_timeout_secs: '120'",
        ]
        .map(|extra| format!("{STDIO}\n{extra}"));
        let sse = "transport: 'sse'\nstart_command: ['server']";
        for text in extras.iter().map(String::as_str).chain([sse]) {
            let parsed = crate::yaml::parse::<RawMcpServerConfig>(text);
            assert!(
                matches!(parsed, Err(crate::yaml::YamlError::Value(_))),
                "{text}"
            );
        }
    }
}
