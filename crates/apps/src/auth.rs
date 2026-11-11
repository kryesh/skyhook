//! The `auth` commands: sign a managed provider in and inspect its credentials.

use std::time::Duration;

use crate::args::AuthCommand;
use skyhook::product::Identity;
use skyhook::provider::dialect::{
    AccountUsage, AuthStatus, LoginFlow, LoginPresenter, LoginRequired,
};

/// Why an `auth` command failed.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error(transparent)]
    Config(#[from] crate::config::LayerResolutionError),
    #[error(transparent)]
    Login(#[from] skyhook::provider::dialect::LoginError),
    #[error(transparent)]
    Provider(#[from] skyhook::provider::ProviderError),
    #[error(transparent)]
    Check(#[from] skyhook::provider::dialect::BuildError),
    #[error(transparent)]
    LoginRequired(LoginRequired),
}

/// Print `text` with its terminal controls escaped: provider names, paths and
/// what the service reports all reach the terminal.
fn say(text: impl std::fmt::Display) {
    println!("{}", skyhook_tools::diagnostic::escape_controls(text));
}

pub async fn run(command: AuthCommand, identity: &Identity) -> Result<(), AuthError> {
    let (AuthCommand::Login { target, .. } | AuthCommand::Status { target, .. }) = &command;
    let (provider, source) = (&target.provider, &target.source);
    // Host layers may carry host settings such as `approve_all`, which the library refuses.
    // No product runs here, so none's capability declarations bear on what the file grants.
    let config = crate::config::resolve_layers::<skyhook::launch::config::HostConfig>(
        &source.workspace,
        source.config.as_deref(),
        None,
    )
    .await?
    .config
    .config;
    let login = config.login(provider, &identity.host)?;
    match &command {
        AuthCommand::Login { headless, .. } => {
            let flow = if *headless {
                LoginFlow::Device
            } else {
                LoginFlow::Browser
            };
            login.login(flow, &Terminal { identity }).await?;
            say(format_args!(
                "Signed in `{provider}` for {}.",
                identity.display_name
            ));
        }
        AuthCommand::Status { check, .. } => match login.status().await? {
            AuthStatus::LoginRequired(required) => {
                if *check {
                    return Err(AuthError::LoginRequired(required));
                }
                say(format_args!("{required}."));
            }
            AuthStatus::LoggedIn { expires_at } => {
                say(format_args!(
                    "`{provider}`: {} credentials present (access token expires at Unix time {expires_at}; refreshed automatically when needed).",
                    identity.display_name
                ));
                if *check {
                    let usage = login.check().await?;
                    say(format_args!("`{provider}`: {}.", accepted(&usage)));
                }
            }
        },
    }
    Ok(())
}

/// Login instructions on stderr, which leaves stdout to the command's result.
struct Terminal<'a> {
    identity: &'a Identity,
}

impl LoginPresenter for Terminal<'_> {
    fn authorize(&self, url: &str) {
        eprintln!(
            "Open this URL to sign in for {}:\n{url}",
            self.identity.display_name
        );
        // Best effort: the URL is printed for manual opening.
        tokio::spawn(launch_browser(url.to_owned()));
    }

    fn device_code(&self, url: &str, code: &str) {
        eprintln!(
            "Open {url} and enter this one-time code: {code}\nOnly continue if you initiated this {} login. The code expires in 15 minutes.",
            self.identity.display_name
        );
    }
}

const BROWSER_TIMEOUT: Duration = Duration::from_secs(3);

async fn launch_browser(url: String) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    // No shell interpolation, no inherited output, no un-reaped background process.
    let mut command = tokio::process::Command::new(program);
    command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let _ = tokio::time::timeout(BROWSER_TIMEOUT, command.status()).await;
}

/// What a check found: the service accepted the credentials, with the plan
/// and window use it reported.
fn accepted(usage: &AccountUsage) -> String {
    let mut text = String::from("the service accepted the credentials");
    if let Some(plan) = &usage.plan {
        text += &format!("; plan {plan}");
    }
    for window in &usage.windows {
        let limit = match window.length.map(|length| length.as_secs()) {
            Some(seconds) if seconds % 86_400 == 0 => format!("the {}d limit", seconds / 86_400),
            Some(seconds) if seconds % 3_600 == 0 => format!("the {}h limit", seconds / 3_600),
            Some(seconds) => format!("the {}m limit", seconds / 60),
            None => "a limit".to_owned(),
        };
        text += &format!("; {}% of {limit} used", window.used_percent);
    }
    text
}
