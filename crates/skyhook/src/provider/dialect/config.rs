//! The configuration every provider entry shares and the checks an entry passes
//! on admission.

use serde::{Deserialize, Serialize};

use super::{DialectError, PlacementError, ResponseSetting};
use crate::provider::{
    codec::EffortLevels,
    http::{
        DEFAULT_TIMEOUT, Headers, Timeouts,
        credentials::{
            ConfiguredHeaders, HeaderError, Pending, Scheme, ValueError, ValueField, admit,
        },
    },
    model::{LimitsError, ModelName},
};
use skyhook_base::bounded::TimeoutSecs;
use skyhook_worker::secret::Credential;

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
    #[error("{0} applies only to the responses codec")]
    ResponsesOnly(ResponseSetting),
    #[error("explicit_caching applies only where the service takes request cache breakpoints")]
    ExplicitCaching,
    #[error("reasoning `{effort}` is not one of: {levels}")]
    Reasoning {
        effort: crate::provider::model::Effort,
        levels: EffortLevels,
    },
}

/// Why a provider entry was refused.
#[derive(Debug, thiserror::Error)]
pub enum AdmissionError {
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
    #[error(transparent)]
    Value(#[from] ValueError),
    #[error(transparent)]
    Header(#[from] HeaderError),
    #[error(transparent)]
    Dialect(#[from] DialectError),
    #[error("models.{model}: {error}")]
    Model { model: ModelName, error: ModelError },
}

/// Fields every dialect accepts, as written. A dialect's own fields sit beside them.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Common {
    /// The API root; Skyhook appends the path of the service it calls. Absent
    /// only where the dialect has its own service (Codex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<Credential>,
    /// Fixed request headers, for proxies and attribution.
    #[serde(default)]
    pub headers: indexmap::IndexMap<String, Credential>,
    /// Time to receive HTTP response headers per attempt.
    #[serde(default = "default_timeout")]
    pub startup_timeout_secs: TimeoutSecs,
    /// Maximum interval between HTTP response body reads.
    #[serde(default = "default_timeout")]
    pub read_idle_timeout_secs: TimeoutSecs,
}

const fn default_timeout() -> TimeoutSecs {
    DEFAULT_TIMEOUT
}

impl Default for Common {
    fn default() -> Self {
        Self {
            base_url: None,
            api_key: None,
            headers: Default::default(),
            startup_timeout_secs: DEFAULT_TIMEOUT,
            read_idle_timeout_secs: DEFAULT_TIMEOUT,
        }
    }
}

/// The common fields, admitted: what a provider is built from.
pub(crate) struct Connection {
    pub(super) root: reqwest::Url,
    pub(super) api_key: Option<Credential>,
    pub(super) headers: ConfiguredHeaders,
    pub(super) timeouts: Timeouts,
}

impl Connection {
    pub(crate) fn credentials(&self, scheme: Scheme) -> Headers<Pending> {
        self.api_key
            .as_ref()
            .map_or_else(Default::default, |key| scheme.credential(key))
    }
}

impl Common {
    /// Admit the shared API root and sources without reading credentials.
    pub(crate) fn admit(&self, base_url: super::BaseUrl) -> Result<Connection, AdmissionError> {
        let base = match (self.base_url.as_deref(), base_url) {
            (Some(base), _) => base,
            (None, super::BaseUrl::Default(base)) => base,
            (None, super::BaseUrl::Required) => return Err(EndpointError::Missing.into()),
        };
        let root = service_url(base)?;
        if let Some(key) = &self.api_key {
            admit(key, ValueField::ApiKey)?;
        }
        Ok(Connection {
            root,
            api_key: self.api_key.clone(),
            headers: ConfiguredHeaders::admit(&self.headers)?,
            timeouts: Timeouts {
                startup: self.startup_timeout_secs.into(),
                read_idle: self.read_idle_timeout_secs.into(),
            },
        })
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
            assert!(service_url(invalid).is_err(), "{invalid}");
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
}
