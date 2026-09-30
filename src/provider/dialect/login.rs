//! Credentials `skyhook auth` manages for a provider entry, in place of an `api_key`.

use std::{fmt, time::Duration};

use super::{AdmissionError, BuildError, ConfigHome, Dialect, codex, config::Pending};
use crate::provider::{
    ProviderError, ProviderErrorKind::Authentication, http::Headers, profile::ProviderName,
};

/// Why a configured provider has no login to manage.
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error("no provider `{0}` is configured")]
    Unknown(ProviderName),
    #[error("provider `{provider}` uses the {dialect} dialect, which authenticates with api_key")]
    Unmanaged {
        provider: ProviderName,
        dialect: Dialect,
    },
    #[error("invalid configuration: {0}")]
    Admission(#[from] AdmissionError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
}

/// A configured provider's Skyhook-managed login.
#[derive(Clone)]
pub enum Login {
    Codex(codex::Account),
}

impl Login {
    /// Sign in using a browser, or the device flow when `headless`.
    pub async fn login(&self, headless: bool) -> Result<(), ProviderError> {
        match self {
            Self::Codex(account) => account.login(headless).await,
        }
    }

    /// Local state only; the server is neither asked nor refreshed.
    pub async fn status(&self) -> Result<AuthStatus, ProviderError> {
        match self {
            Self::Codex(account) => account.status().await,
        }
    }

    /// Present the credentials to the service in a request that spends no
    /// quota, refreshing them first if a request would.
    pub async fn check(&self) -> Result<Usage, BuildError> {
        match self {
            Self::Codex(account) => account.check().await,
        }
    }

    pub(crate) fn headers(self) -> Headers<Pending> {
        match self {
            Self::Codex(account) => account.into_headers(),
        }
    }
}

/// What the service reports for a signed-in account; what it omits is absent.
#[derive(Clone, Debug, PartialEq)]
pub struct Usage {
    /// The service's name for the account's plan, shown as given.
    pub plan: Option<String>,
    pub windows: Vec<UsageWindow>,
}

/// One rate-limit window's use.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageWindow {
    pub used_percent: f64,
    pub length: Option<Duration>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthStatus {
    LoginRequired(LoginRequired),
    LoggedIn { expires_at: u64 },
}

/// The `skyhook auth login` invocation that signs a provider in. Naming the
/// defining file with `-c` selects the same entry wherever the command runs;
/// options precede the provider, after `--` when its name looks like one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginCommand {
    pub provider: ProviderName,
    pub home: ConfigHome,
}

impl fmt::Display for LoginCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("skyhook auth login ")?;
        if let ConfigHome::File(config) = &self.home {
            write!(f, "-c {} ", shell_word(&config.to_string_lossy()))?;
        }
        let provider = self.provider.as_str();
        if provider.starts_with('-') {
            f.write_str("-- ")?;
        }
        f.write_str(&shell_word(provider))
    }
}

/// `text` as one shell word, single-quoted only when a shell would split or
/// expand it.
fn shell_word(text: &str) -> std::borrow::Cow<'_, str> {
    let plain = |c: char| c.is_ascii_alphanumeric() || "/._-+:@%,=".contains(c);
    if !text.is_empty() && text.chars().all(plain) {
        return text.into();
    }
    format!("'{}'", text.replace('\'', r"'\''")).into()
}

/// Stored credentials that cannot be used until the user logs in again.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{}` {reason}; run `{command}`", command.provider)]
pub struct LoginRequired {
    pub command: LoginCommand,
    pub reason: LoginReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LoginReason {
    #[error("is not logged in")]
    LoggedOut,
    #[error("holds credentials another auth_url issued")]
    OtherIssuer,
    #[error("holds credentials the service rejected")]
    Rejected,
}

impl From<LoginRequired> for ProviderError {
    fn from(required: LoginRequired) -> Self {
        Authentication.error(required.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hint pastes into a shell as one command, and clap reads its
    /// provider as the provider, whatever the path and name hold.
    #[test]
    fn login_command_pastes_as_one_command_for_any_path_or_name() {
        let command = |provider: &str, config: &str| {
            let command = LoginCommand {
                provider: provider.parse().unwrap(),
                home: ConfigHome::File(config.into()),
            };
            command.to_string()
        };
        assert_eq!(
            command("work", "/home/me/.config/skyhook/config.yaml"),
            "skyhook auth login -c /home/me/.config/skyhook/config.yaml work"
        );
        assert_eq!(
            command("-work", "/home/me/My Projects/it's/config.yaml"),
            r"skyhook auth login -c '/home/me/My Projects/it'\''s/config.yaml' -- -work"
        );
    }
}
