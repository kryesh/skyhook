mod cli;
mod dotenv;
mod dump;
mod embedded_shims;
mod headless;
mod interaction;
mod launch;
mod state;
mod stats;
mod text;
mod tui;
use cli::{AuthCommand, AuthProvider, Invocation};
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Why an `auth` command failed.
#[derive(Debug, thiserror::Error)]
enum AuthError {
    #[error(transparent)]
    Config(#[from] skyhook::config::ConfigError),
    #[error("invalid configuration: {0}")]
    CodexIssuers(#[from] skyhook::provider::dialect::codex::IssuerConflict),
    #[error(transparent)]
    Codex(#[from] skyhook::provider::ProviderError),
}

/// The issuer the admitted codex entries share; without a config, OpenAI's.
fn codex_issuer(
    config: Result<skyhook::config::Config, skyhook::config::ConfigError>,
) -> Result<skyhook::provider::dialect::codex::auth::Issuer, AuthError> {
    use skyhook::config::ConfigError;
    match config {
        Ok(config) => Ok(skyhook::provider::dialect::codex::issuer(
            &config.providers,
        )?),
        Err(ConfigError::Missing(_)) => Ok(Default::default()),
        Err(error) => Err(error.into()),
    }
}

async fn run_auth(command: AuthCommand) -> Result<(), AuthError> {
    use skyhook::{config::Config, provider::dialect::codex::auth};
    let issuer =
        async || codex_issuer(Config::load_for_workspace(std::path::Path::new("."), None).await);
    match command {
        AuthCommand::Login {
            provider: AuthProvider::Codex,
            headless,
        } => {
            auth::login(issuer().await?, headless).await?;
            println!("Signed in to Codex for Skyhook.");
        }
        AuthCommand::Status {
            provider: AuthProvider::Codex,
        } => match auth::status(issuer().await?).await? {
            auth::AuthStatus::LoginRequired(required) => println!("{required}."),
            auth::AuthStatus::LoggedIn { expires_at } => println!(
                "Codex: Skyhook credentials present (access token expires at Unix time {expires_at}; refreshed automatically when needed)."
            ),
        },
        AuthCommand::Logout {
            provider: AuthProvider::Codex,
        } => {
            auth::logout().await?;
            println!(
                "Removed Skyhook's Codex credentials. Other applications' credentials were not changed."
            );
        }
    }
    Ok(())
}

/// Exit when `result` failed, reporting the error under `label` with its
/// terminal controls escaped.
fn exit_on<T>(label: &str, result: Result<T, impl std::fmt::Display>) -> T {
    result.unwrap_or_else(|error| {
        let error = skyhook::tool::diagnostic::escape_controls(error);
        eprintln!("{label}: {error}");
        std::process::exit(1)
    })
}

fn main() {
    skyhook::remote::askpass_main();
    let invocation = cli::parse_from(std::env::args_os()).unwrap_or_else(|error| error.exit());
    // SAFETY: startup is still single-threaded: no Tokio runtime, terminal,
    // tracing subscriber, provider, or background worker has been started.
    // Parse Clap first so help/version do not depend on a valid .env file.
    exit_on("skyhook", unsafe { dotenv::load_invocation_env() });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|_| "could not start async runtime");
    exit_on("skyhook", runtime).block_on(run(invocation));
}

async fn run(invocation: Invocation) {
    match invocation {
        Invocation::Inspect(request) => exit_on("skyhook dump", dump::run(request).await),
        Invocation::Stats(request) => exit_on("skyhook stats", stats::run(request).await),
        Invocation::Auth(command) => exit_on("skyhook auth", run_auth(command).await),
        // Do not install any terminal, renderer, or tracing subscriber here.
        Invocation::Headless(request, input) => {
            exit_on("skyhook", headless::run(request, input).await)
        }
        Invocation::Interactive(request, input) => {
            exit_on("skyhook", tui::run(request, input).await)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skyhook::config::ConfigError;

    /// Await `future`, failing the test at the caller if it stalls.
    #[track_caller]
    pub(crate) fn bounded<T>(future: impl Future<Output = T>) -> impl Future<Output = T> {
        let caller = std::panic::Location::caller();
        async move {
            tokio::time::timeout(std::time::Duration::from_secs(20), future)
                .await
                .unwrap_or_else(|_| panic!("test synchronization timed out at {caller}"))
        }
    }

    #[test]
    fn login_issuer_defaults_without_config_and_propagates_config_errors() {
        let issuer = |config| codex_issuer(config).map(|issuer| issuer.to_string());
        let missing = ConfigError::Missing(Default::default());
        assert_eq!(issuer(Err(missing)).unwrap(), "https://auth.openai.com/");
        let other = ConfigError::Empty;
        assert!(matches!(issuer(Err(other)), Err(AuthError::Config(_))));
    }
}
