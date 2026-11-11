use crate::provider::http::credentials::ConfiguredHeaders;
use crate::tool::policy::{Capability, CapabilitySet};
use serde::{Serialize, Serializer};
use skyhook_base::{
    bounded::TimeoutSecs,
    decode::{Context, Decode, Node, Segment},
    string_newtype,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

/// A configured server, admitted as written: without startup or secret lookup.
/// It serializes in the flat shape and URL spelling it was written in.
#[derive(Clone, Debug, Decode)]
pub struct McpServerConfig {
    #[decode(flatten)]
    connection: McpConnection,
    #[decode(flatten)]
    settings: Settings,
}

/// What every transport takes.
#[derive(Clone, Debug, Serialize, Decode)]
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
#[derive(Clone, Debug, Serialize, Decode)]
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

#[derive(Clone, Debug, Serialize, Decode)]
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

/// Strings are used exactly as written; only executable shape is checked. A relative
/// `cwd` resolves against the directory of the file that writes it.
#[derive(Clone, Debug, Serialize, Decode)]
#[decode(check = "CommandSpec::resolved")]
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

    fn resolved(&mut self, node: &Node, cx: &mut Context) {
        let (Some(cwd), Some(base)) = (&mut self.cwd, node.at.directory()) else {
            return;
        };
        if cwd.is_relative() {
            let joined = base.join(&*cwd);
            match joined.to_str() {
                Some(_) => *cwd = joined,
                None => cx.error_within(
                    node,
                    vec![Segment::Key("cwd".into())],
                    "cwd cannot be represented as UTF-8",
                ),
            }
        }
    }
}

/// A program and its arguments.
#[derive(Clone, Debug, Serialize)]
#[serde(into = "Vec<String>")]
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

skyhook_base::decode_via!(Argv => Vec<String>);

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

impl Serialize for McpServerConfig {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fields = crate::yaml::object(&self.connection);
        fields.extend(crate::yaml::object(&self.settings));
        fields.serialize(serializer)
    }
}

impl McpServerConfig {
    pub(super) fn connection(&self) -> &McpConnection {
        &self.connection
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
            let config: McpServerConfig = crate::yaml::parse(text).unwrap();
            let written = crate::yaml::from_str(text).unwrap();
            assert_eq!(serde_json::to_value(&config).unwrap(), written);
        }
        let config: McpServerConfig = crate::yaml::parse(
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
                "`mcp.s`: missing required field `start_command`",
            ),
            (
                "transport: 'stdio'\nstart_command: []".into(),
                "start_command`: must contain a nonempty executable",
            ),
            (
                "transport: 'stdio'\nstart_command: ['   ']".into(),
                "start_command`: must contain a nonempty executable",
            ),
            (
                format!("{STDIO}\nurl: 'https://example.com/mcp'"),
                "url`: unknown field `url`",
            ),
            (
                format!("{STDIO}\nheaders: {{ Authorization: 'TOKEN' }}"),
                "headers`: unknown field `headers`",
            ),
            (
                "transport: 'streamable_http'".into(),
                "`mcp.s`: missing required field `url`",
            ),
            (
                "transport: 'streamable_http'\ncwd: 'work'".into(),
                "`mcp.s`: missing required fields `url`, `start_command`",
            ),
            (
                "transport: 'streamable_http'\nurl: '/mcp'".into(),
                "url`: MCP HTTP url must be an absolute http or https URL",
            ),
            (
                "transport: 'streamable_http'\nurl: 'ftp://example.com/mcp'".into(),
                "url`: MCP HTTP url must be an absolute http or https URL",
            ),
            (
                format!("{http}\nheaders: {{ 'invalid name': 'TOKEN' }}"),
                "`mcp.s.headers.invalid name`: `invalid name` is not a valid header name",
            ),
            (
                format!("{http}\ncwd: 'work'"),
                "`mcp.s`: missing required field `start_command`",
            ),
            (
                format!("{http}\nenv: {{ MODE: 'test' }}"),
                "`mcp.s`: missing required field `start_command`",
            ),
            (
                format!("{http}\nstart_command: []"),
                "start_command`: must contain a nonempty executable",
            ),
            (
                "start_command: ['server']".into(),
                "`mcp.s`: missing required field `transport`",
            ),
            (
                "transport: 'sse'\nstart_command: ['server']".into(),
                "unknown variant `sse`",
            ),
            (
                format!("{STDIO}\ncommand: ['other']"),
                "`mcp.s.command`: unknown field `command`, expected one of `transport`, \
                 `start_command`,",
            ),
            (
                format!("{http}\nheader: {{}}"),
                "`mcp.s.header`: unknown field `header`, expected one of `transport`, `url`, \
                 `headers`, `start_command`,",
            ),
            (
                format!("{STDIO}\ncapabilities: ['Read']"),
                "capabilities[0]`: unknown capability `Read`, expected one of",
            ),
            (
                format!("{STDIO}\nstartup_timeout_secs: 0"),
                "startup_timeout_secs`: must be 1 through 86400",
            ),
            (
                format!("{STDIO}\ncall_timeout_secs: '120'"),
                "call_timeout_secs`: expected a nonnegative integer, found a string",
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
