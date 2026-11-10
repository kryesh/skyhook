use crate::tool::policy::{Capability, CapabilitySet};
use crate::{provider::http::credentials::ConfiguredHeaders, yaml};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Map, Value};
use skyhook_base::{bounded::TimeoutSecs, string_newtype};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

/// A configured server, admitted as written: without startup or secret lookup.
/// It serializes in the flat shape and URL spelling it was written in.
#[derive(Clone, Debug)]
pub struct McpServerConfig {
    connection: McpConnection,
    settings: Settings,
}

/// What every transport takes.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Settings {
    #[serde(default)]
    capabilities: Vec<Capability>,
    #[serde(default = "default_startup_timeout_secs")]
    startup_timeout_secs: TimeoutSecs,
    #[serde(default = "default_call_timeout_secs")]
    call_timeout_secs: TimeoutSecs,
}

const fn default_startup_timeout_secs() -> TimeoutSecs {
    TimeoutSecs::new::<30>()
}
const fn default_call_timeout_secs() -> TimeoutSecs {
    TimeoutSecs::new::<120>()
}

/// How a server is reached, tagged by `transport`.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub(super) enum McpConnection {
    Stdio(CommandSpec),
    StreamableHttp {
        #[serde(flatten)]
        endpoint: Endpoint,
        /// Run only while the endpoint is unreachable.
        #[serde(flatten)]
        start_if_unreachable: Option<CommandSpec>,
    },
}

/// The `transport` names, which select the fields beside them.
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Transport {
    Stdio,
    StreamableHttp,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Endpoint {
    pub(super) url: Url,
    /// Header names mapped to their values, read only when the server is contacted.
    #[serde(default)]
    pub(super) headers: ConfiguredHeaders,
}

string_newtype! {
    /// An MCP endpoint, kept as written.
    pub struct Url(&'static str) = |url| {
        let absolute = reqwest::Url::parse(url)
            .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some());
        absolute.then_some(()).ok_or("MCP HTTP url must be an absolute http or https URL")
    };
}

/// Strings are used exactly as written; only executable shape is checked.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CommandSpec {
    pub(super) start_command: Argv,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) cwd: Option<PathBuf>,
    #[serde(default)]
    pub(super) env: BTreeMap<String, String>,
}

impl CommandSpec {
    /// The configured directory, else the session workspace.
    pub(super) fn directory<'a>(&'a self, workspace: &'a Path) -> &'a Path {
        self.cwd.as_deref().unwrap_or(workspace)
    }
}

/// A program and its arguments.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub(super) struct Argv(Vec<String>);

impl TryFrom<Vec<String>> for Argv {
    type Error = &'static str;

    fn try_from(argv: Vec<String>) -> Result<Self, &'static str> {
        match argv.first() {
            Some(program) if !program.trim().is_empty() => Ok(Self(argv)),
            _ => Err("must contain a nonempty executable"),
        }
    }
}

impl From<Argv> for Vec<String> {
    fn from(argv: Argv) -> Self {
        argv.0
    }
}

impl Argv {
    pub(super) fn program(&self) -> &str {
        &self.0[0]
    }

    pub(super) fn args(&self) -> &[String] {
        &self.0[1..]
    }
}

impl<'de> Deserialize<'de> for McpServerConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut fields = Map::new();
        let settings = yaml::split(deserializer, &mut fields)?;
        let transport = fields
            .remove("transport")
            .ok_or_else(|| de::Error::missing_field("transport"))?;
        let connection = match Transport::deserialize(transport).map_err(de::Error::custom)? {
            Transport::Stdio => McpConnection::Stdio(yaml::located(fields)?),
            Transport::StreamableHttp => {
                let mut launch = Map::new();
                let endpoint =
                    yaml::split(Value::Object(fields), &mut launch).map_err(de::Error::custom)?;
                let start_if_unreachable = (!launch.is_empty())
                    .then(|| yaml::located(launch))
                    .transpose()?;
                McpConnection::StreamableHttp {
                    endpoint,
                    start_if_unreachable,
                }
            }
        };
        Ok(Self {
            connection,
            settings,
        })
    }
}

impl Serialize for McpServerConfig {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fields = yaml::object(&self.connection);
        fields.extend(yaml::object(&self.settings));
        fields.serialize(serializer)
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum McpConfigError {
    #[error("cwd cannot be represented as UTF-8")]
    NonUtf8Cwd,
}

impl McpServerConfig {
    pub(super) fn connection(&self) -> &McpConnection {
        &self.connection
    }
    /// Resolve a relative launch `cwd` against `base`.
    pub fn resolve_cwd(&mut self, base: &Path) -> Result<(), McpConfigError> {
        let command = match &mut self.connection {
            McpConnection::Stdio(command) => command,
            McpConnection::StreamableHttp {
                start_if_unreachable,
                ..
            } => match start_if_unreachable {
                Some(command) => command,
                None => return Ok(()),
            },
        };
        if let Some(cwd) = &mut command.cwd
            && cwd.is_relative()
        {
            let joined = base.join(&*cwd);
            if joined.to_str().is_none() {
                return Err(McpConfigError::NonUtf8Cwd);
            }
            *cwd = joined;
        }
        Ok(())
    }
    pub fn capabilities(&self) -> &[Capability] {
        &self.settings.capabilities
    }
    /// Whether an agent holding `capabilities` may use this server: MCP itself
    /// and every capability the server lists.
    pub fn permitted_by(&self, capabilities: &CapabilitySet) -> bool {
        capabilities.contains(&Capability::Mcp)
            && (self.settings.capabilities.iter()).all(|cap| capabilities.contains(cap))
    }
    pub fn startup_timeout(&self) -> Duration {
        self.settings.startup_timeout_secs.into()
    }
    pub fn call_timeout(&self) -> Duration {
        self.settings.call_timeout_secs.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skyhook_worker::secret::Credential;

    const STDIO: &str = "transport: 'stdio'\nstart_command: ['server']";

    /// Each transport takes its own fields, and a server dumps as written with its
    /// defaults in effect; admission is pure, leaving existence, access and
    /// environment values to startup, and header names are normalized.
    #[test]
    fn transports_take_their_own_fields_and_dump_as_written() {
        for text in [
            "transport: 'stdio'\nstart_command: [' server name ', 'a b', '$UNEXPANDED']\nenv: {}\ncapabilities: []\nstartup_timeout_secs: 30\ncall_timeout_secs: 120",
            "transport: 'streamable_http'\nurl: 'HTTPS://Example.COM:443/a%2Fb?q=x#fragment'\nheaders: {}\ncapabilities: []\nstartup_timeout_secs: 30\ncall_timeout_secs: 120",
            "transport: 'streamable_http'\nurl: 'http://[::1]:1234/mcp'\nheaders: { authorization: { env: 'MISSING_MCP_TEST_TOKEN' }, x-command: { command: 'printf v' } }\nstart_command: ['/nonexistent/mcp-command', '--flag']\ncwd: 'relative/path'\nenv: { KEY: 'value' }\ncapabilities: ['read', 'exec']\nstartup_timeout_secs: 2\ncall_timeout_secs: 9",
        ] {
            let config: McpServerConfig = yaml::parse(text).unwrap();
            let written = yaml::from_str(text).unwrap();
            assert_eq!(serde_json::to_value(&config).unwrap(), written);
        }
        let config: McpServerConfig = yaml::parse(
            "transport: streamable_http\nurl: http://h/mcp\nheaders: {Authorization: x}",
        )
        .unwrap();
        let dumped = serde_json::to_value(config).unwrap();
        let literal = serde_json::to_value(Credential::Literal("x".into())).unwrap();
        assert_eq!(
            dumped["headers"],
            serde_json::json!({"authorization": literal})
        );
    }

    #[test]
    fn misplaced_or_missing_fields_are_refused_by_name() {
        let http = "transport: 'streamable_http'\nurl: 'https://example.com/mcp'";
        for (text, expected) in [
            (
                "transport: 'stdio'".to_owned(),
                "missing field `start_command`",
            ),
            (
                "transport: 'stdio'\nstart_command: []".into(),
                "start_command: must contain a nonempty executable",
            ),
            (
                "transport: 'stdio'\nstart_command: ['   ']".into(),
                "start_command: must contain a nonempty executable",
            ),
            (
                format!("{STDIO}\nurl: 'https://example.com/mcp'"),
                "url: unknown field `url`",
            ),
            (
                format!("{STDIO}\nheaders: {{ Authorization: 'TOKEN' }}"),
                "headers: unknown field `headers`",
            ),
            ("transport: 'streamable_http'".into(), "missing field `url`"),
            (
                "transport: 'streamable_http'\nurl: '/mcp'".into(),
                "MCP HTTP url must be an absolute http or https URL",
            ),
            (
                "transport: 'streamable_http'\nurl: 'ftp://example.com/mcp'".into(),
                "MCP HTTP url must be an absolute http or https URL",
            ),
            (
                format!("{http}\nheaders: {{ 'invalid name': 'TOKEN' }}"),
                "headers: `invalid name` is not a valid header name",
            ),
            (
                format!("{http}\ncwd: 'work'"),
                "missing field `start_command`",
            ),
            (
                format!("{http}\nenv: {{ MODE: 'test' }}"),
                "missing field `start_command`",
            ),
            (
                format!("{http}\nstart_command: []"),
                "start_command: must contain a nonempty executable",
            ),
            (
                "start_command: ['server']".into(),
                "missing field `transport`",
            ),
            (
                "transport: 'sse'\nstart_command: ['server']".into(),
                "unknown variant `sse`",
            ),
            (
                format!("{STDIO}\ncommand: ['other']"),
                "unknown field `command`",
            ),
            (
                format!("{STDIO}\ncapabilities: ['Read']"),
                "capabilities[0]`: host capability must be",
            ),
            (
                format!("{STDIO}\nstartup_timeout_secs: 0"),
                "startup_timeout_secs`: must be 1 through 86400",
            ),
            (
                format!("{STDIO}\ncall_timeout_secs: '120'"),
                "call_timeout_secs`: invalid type",
            ),
        ] {
            let mcp = format!("mcp:\n  s:\n    {}", text.replace('\n', "\n    "));
            let error = crate::config::Config::from_yaml(&mcp)
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{text}: {error}");
        }
    }
}
