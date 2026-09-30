//! The configuration every provider entry shares, the value type that may carry
//! a secret, and the checks an entry passes on admission.

use std::{env, fmt, time::Duration};

use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};

use super::{DialectError, PlacementError};
use crate::provider::{
    codec::EffortLevels,
    http::{
        Headers, Timeouts,
        headers::{CommandValue, Value, ValueField, prefixed},
    },
    profile::{LimitsError, ModelName},
};

/// A configured value: written literally, read from the environment when the
/// provider is built, or produced by `/bin/sh -c` on first use and cached on
/// success. Commands run on the host with Skyhook's environment.
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged, deny_unknown_fields)]
pub enum Sourced {
    Literal(String),
    Env { env: String },
    Command { command: String },
}

/// A literal may be a secret and a command may name one's location; only the
/// kind of source, or the variable's name, is shown.
impl fmt::Debug for Sourced {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(_) => f.write_str("Literal(..)"),
            Self::Env { env } => f.debug_struct("Env").field("name", env).finish(),
            Self::Command { .. } => f.write_str("Command(..)"),
        }
    }
}

/// A configured value that cannot be used: refused when the entry is admitted,
/// or, for an environment value, when the provider is built.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{field}: {problem}")]
pub struct ValueError {
    pub field: ValueField,
    pub problem: ValueProblem,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ValueProblem {
    #[error("must not be blank")]
    Blank,
    #[error("env must name a nonempty environment variable")]
    InvalidEnvName,
    #[error("command must not be blank")]
    BlankCommand,
    #[error("must be a valid header value, without line breaks or control characters")]
    InvalidHeaderValue,
    #[error("environment variable `{0}` is required and must not be empty")]
    MissingEnvironment(String),
    #[error(
        "environment variable `{0}` must hold a valid header value, without line breaks or control characters"
    )]
    InvalidEnvironment(String),
}

/// A configured value, admitted: a literal is already a header value.
#[derive(Clone)]
pub(crate) enum Source {
    Literal(HeaderValue),
    Env(String),
    Command(String),
}

/// Shown as `Sourced` is.
impl fmt::Debug for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(_) => f.write_str("Literal(..)"),
            Self::Env(name) => f.debug_struct("Env").field("name", name).finish(),
            Self::Command(_) => f.write_str("Command(..)"),
        }
    }
}

impl Sourced {
    /// The value `field` is configured with, checked without reading the
    /// environment or running a command.
    pub(crate) fn admit(&self, field: ValueField) -> Result<Source, ValueError> {
        let problem = match self {
            Self::Literal(value) if value.trim().is_empty() => ValueProblem::Blank,
            Self::Literal(value) => match HeaderValue::from_str(value) {
                Ok(value) => return Ok(Source::Literal(value)),
                Err(_) => ValueProblem::InvalidHeaderValue,
            },
            Self::Env { env } if env.trim().is_empty() || env.contains(['=', '\0']) => {
                ValueProblem::InvalidEnvName
            }
            Self::Env { env } => return Ok(Source::Env(env.clone())),
            Self::Command { command } if command.trim().is_empty() => ValueProblem::BlankCommand,
            Self::Command { command } => return Ok(Source::Command(command.clone())),
        };
        Err(ValueError { field, problem })
    }
}

impl Source {
    /// The value for `field`, behind `prefix` as `Bearer ` precedes a bearer
    /// token, once composition shows it is sent.
    pub(crate) fn header(&self, field: ValueField, prefix: Option<&'static str>) -> Pending {
        Pending::Source {
            source: self.clone(),
            field,
            prefix,
        }
    }

    /// Credentials are never logged, whatever their source; other values only
    /// when written literally in the file. Environment values are read here;
    /// commands stay lazy.
    fn read(self, field: &ValueField) -> Result<Raw, ValueError> {
        let sensitive = |mut value: HeaderValue, sensitive| {
            value.set_sensitive(sensitive);
            value
        };
        Ok(match self {
            Self::Literal(value) => Raw::Fixed(sensitive(value, *field == ValueField::ApiKey)),
            Self::Env(name) => {
                let problem = match env::var(&name) {
                    Ok(value) if !value.trim().is_empty() => match HeaderValue::from_str(&value) {
                        Ok(value) => return Ok(Raw::Fixed(sensitive(value, true))),
                        Err(_) => ValueProblem::InvalidEnvironment(name),
                    },
                    _ => ValueProblem::MissingEnvironment(name),
                };
                let field = field.clone();
                return Err(ValueError { field, problem });
            }
            Self::Command(command) => Raw::Command(CommandValue::new(command, field.clone())),
        })
    }
}

/// A header value while a provider is built: ready, or a source read once
/// composition shows the value is sent.
#[derive(Clone)]
pub(crate) enum Pending {
    Ready(Value),
    Source {
        source: Source,
        field: ValueField,
        prefix: Option<&'static str>,
    },
}

/// The sources one provider build has read, shared by all its models so that
/// each reads the environment once and shares one command generation, whatever
/// prefix its credential scheme puts in front.
#[derive(Default)]
pub(crate) struct Sources(Vec<(ValueField, Raw)>);

/// A source's value before any prefix.
#[derive(Clone)]
enum Raw {
    Fixed(HeaderValue),
    Command(CommandValue),
}

impl Sources {
    pub(crate) fn read(&mut self, pending: Pending) -> Result<Value, ValueError> {
        let (source, field, prefix) = match pending {
            Pending::Ready(value) => return Ok(value),
            Pending::Source {
                source,
                field,
                prefix,
            } => (source, field, prefix),
        };
        let raw = match self.0.iter().find(|(read, _)| *read == field) {
            Some((_, raw)) => raw.clone(),
            None => {
                let raw = source.read(&field)?;
                self.0.push((field, raw.clone()));
                raw
            }
        };
        Ok(match raw {
            Raw::Fixed(value) => Value::Fixed(prefixed(prefix, &value)),
            Raw::Command(value) => Value::Command(value.with_prefix(prefix)),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ModelError {
    #[error(transparent)]
    Settings(#[from] crate::provider::settings::MissingSetting),
    #[error(transparent)]
    Dialect(#[from] DialectError),
    #[error(transparent)]
    Limits(#[from] LimitsError),
    #[error(transparent)]
    Placement(#[from] PlacementError),
    #[error("reasoning `{effort}` is not one of: {}", .levels.0.join(", "))]
    Reasoning {
        effort: String,
        levels: EffortLevels,
    },
}

/// Why a provider entry was refused.
#[derive(Debug, thiserror::Error)]
pub enum AdmissionError {
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
    #[error(transparent)]
    Timeouts(#[from] crate::provider::http::TimeoutError),
    #[error(transparent)]
    Value(#[from] ValueError),
    #[error("headers: `{0}` is not a valid header name")]
    HeaderName(String),
    #[error("headers: `{0}` is named twice; header names ignore case")]
    DuplicateHeader(HeaderName),
    #[error(transparent)]
    Dialect(#[from] DialectError),
    #[error("models.{model}: {error}")]
    Model { model: ModelName, error: ModelError },
}

/// Fields every dialect accepts, as written. A dialect's own fields sit beside them.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Common {
    /// The API root; Skyhook appends the path of the service it calls. Absent
    /// only where the dialect has its own service (Codex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<Sourced>,
    /// Fixed request headers, for proxies and attribution.
    #[serde(default, skip_serializing_if = "indexmap::IndexMap::is_empty")]
    pub headers: indexmap::IndexMap<String, Sourced>,
    /// Time to receive HTTP response headers per attempt; absent takes the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_secs: Option<u64>,
    /// Maximum interval between HTTP response body reads; absent takes the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_idle_timeout_secs: Option<u64>,
}

/// The common fields, admitted: what a provider is built from.
pub(crate) struct Connection {
    pub(super) root: reqwest::Url,
    pub(super) api_key: Option<Source>,
    pub(super) headers: Vec<(HeaderName, Source)>,
    pub(super) timeouts: Timeouts,
}

impl Connection {
    pub(crate) fn credentials(
        &self,
        scheme: super::Scheme,
    ) -> crate::provider::http::Headers<Pending> {
        self.api_key
            .as_ref()
            .map_or_else(Default::default, |key| scheme.credential(key))
    }
}

impl Common {
    fn timeouts(&self) -> Timeouts {
        let defaults = Timeouts::default();
        Timeouts {
            startup: self
                .startup_timeout_secs
                .map_or(defaults.startup, Duration::from_secs),
            read_idle: self
                .read_idle_timeout_secs
                .map_or(defaults.read_idle, Duration::from_secs),
        }
    }

    /// Admit the shared API root and sources without reading credentials.
    pub(crate) fn admit(&self, base_url: super::BaseUrl) -> Result<Connection, AdmissionError> {
        let base = match (self.base_url.as_deref(), base_url) {
            (Some(base), _) => base,
            (None, super::BaseUrl::Default(base)) => base,
            (None, super::BaseUrl::Required) => return Err(EndpointError::Missing.into()),
        };
        let root = service_url(base)?;
        let timeouts = self.timeouts();
        timeouts.validate()?;
        let api_key = self
            .api_key
            .as_ref()
            .map(|key| key.admit(ValueField::ApiKey))
            .transpose()?;
        let mut headers: Vec<(HeaderName, Source)> = Vec::new();
        for (name, value) in &self.headers {
            let header = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| AdmissionError::HeaderName(name.clone()))?;
            if headers.iter().any(|(admitted, _)| *admitted == header) {
                return Err(AdmissionError::DuplicateHeader(header));
            }
            let value = value.admit(ValueField::Header(header.clone()))?;
            headers.push((header, value));
        }
        Ok(Connection {
            root,
            api_key,
            headers,
            timeouts,
        })
    }

    /// Settled defaults, as `dump config` writes them.
    pub(crate) fn with_resolved_defaults(mut self) -> Self {
        let timeouts = self.timeouts();
        self.startup_timeout_secs = Some(timeouts.startup.as_secs());
        self.read_idle_timeout_secs = Some(timeouts.read_idle.as_secs());
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EndpointError {
    #[error("base_url is required")]
    Missing,
    #[error("base_url must be an absolute HTTP(S) API-root URL")]
    NotAbsolute,
    #[error("base_url must be HTTP(S), without credentials, query, or fragment")]
    Unclean,
}

/// An absolute HTTP(S) URL without credentials, query or fragment.
pub(crate) fn service_url(text: &str) -> Result<reqwest::Url, EndpointError> {
    let url = reqwest::Url::parse(text).map_err(|_| EndpointError::NotAbsolute)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(EndpointError::Unclean);
    }
    Ok(url)
}

impl Connection {
    /// The entry's `headers`, read once composition shows they are sent.
    pub(crate) fn configured_headers(&self) -> Headers<Pending> {
        let mut headers = Headers::default();
        for (header, value) in &self.headers {
            let field = ValueField::Header(header.clone());
            headers.insert(header.clone(), value.header(field, None));
        }
        headers
    }

    /// `path` beneath the API root.
    pub(crate) fn url(&self, path: &str) -> reqwest::Url {
        let mut url = self.root.clone();
        url.set_path(&format!("{}/{path}", url.path().trim_end_matches('/')));
        url
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_append_paths_and_refuse_secrets_in_urls() {
        let common = Common {
            base_url: Some("https://example.com/custom/v1/".into()),
            ..Common::default()
        };
        let connection = common.admit(super::super::BaseUrl::Required).unwrap();
        let url = connection.url("codex/responses");
        assert_eq!(
            url.as_str(),
            "https://example.com/custom/v1/codex/responses"
        );
        for invalid in [
            "/v1",
            "ftp://example.com/v1",
            "https://user:secret@example.com/v1",
            "https://example.com/v1?q=secret",
            "https://example.com/v1#fragment",
        ] {
            let error = service_url(invalid).unwrap_err();
            assert!(!error.to_string().contains("secret"), "{invalid}");
        }
    }

    /// Debug output may reach logs, so it names only the kind of each source;
    /// `dump config` still writes what the file says.
    #[test]
    fn debug_shows_only_the_kind_of_source() {
        let text = "providers:\n  a:\n    dialect: anthropic\n    codec: messages\n    base_url: https://h/v1\n    api_key: literal-secret\n    headers:\n      x-env: {env: SKYHOOK_NAME}\n      x-command: {command: cat private-path}\n";
        let config = crate::config::Config::from_yaml(text).unwrap();
        let debug = format!("{config:?}");
        for hidden in ["literal-secret", "private-path"] {
            assert!(!debug.contains(hidden), "{debug}");
        }
        assert!(debug.contains("Env { name: \"SKYHOOK_NAME\" }"), "{debug}");
        let dumped = config.to_yaml().unwrap();
        for shown in ["literal-secret", "private-path"] {
            assert!(dumped.contains(shown), "{dumped}");
        }
    }

    #[test]
    fn sourced_values_parse_as_literal_env_or_command() {
        let parse = |text: &str| crate::yaml::parse::<Sourced>(text).unwrap();
        assert_eq!(parse("plain"), Sourced::Literal("plain".into()));
        assert_eq!(parse("{env: KEY}"), Sourced::Env { env: "KEY".into() });
        assert_eq!(
            parse("{command: 'echo k'}"),
            Sourced::Command {
                command: "echo k".into()
            }
        );
        assert!(crate::yaml::parse::<Sourced>("{env: KEY, command: x}").is_err());
        let problem = |sourced: Sourced| sourced.admit(ValueField::ApiKey).err().map(|e| e.problem);
        assert_eq!(
            problem(Sourced::Env { env: "A=B".into() }),
            Some(ValueProblem::InvalidEnvName)
        );
        assert_eq!(
            problem(Sourced::Command {
                command: "  ".into()
            }),
            Some(ValueProblem::BlankCommand)
        );
        // A block scalar keeps its trailing newline, which no header carries.
        assert_eq!(
            problem(parse("|\n  sk-key\n")),
            Some(ValueProblem::InvalidHeaderValue)
        );
        let fixed = |text: &str, field: ValueField, prefix| {
            let source = parse(text).admit(field.clone()).unwrap();
            match Sources::default()
                .read(source.header(field, prefix))
                .unwrap()
            {
                Value::Fixed(value) => value,
                _ => panic!("{text} is read when the provider is built"),
            }
        };
        let header = || ValueField::Header(HeaderName::from_static("x-header"));
        let value = fixed("k", ValueField::ApiKey, Some("Bearer "));
        assert_eq!(value.to_str().unwrap(), "Bearer k");
        assert!(value.is_sensitive());
        assert!(!fixed("v", header(), None).is_sensitive());
        // Environment values stay sensitive.
        assert!(fixed("{env: PATH}", header(), None).is_sensitive());
    }
}
