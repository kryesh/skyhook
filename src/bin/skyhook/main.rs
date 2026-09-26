mod cli;
mod dotenv;
mod dump;
mod embedded_shims;
mod headless;
mod interaction;
mod launch;
mod stats;
mod tui;
use cli::{AuthCommand, AuthProvider, Invocation};
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Why an `auth` command failed.
#[derive(Debug, thiserror::Error)]
enum AuthError {
    #[error(transparent)]
    Config(#[from] skyhook::config::ConfigError),
    #[error(transparent)]
    Codex(#[from] skyhook::provider::ProviderError),
}

/// The issuer the admitted codex entries share; without a config, OpenAI's.
fn codex_issuer(
    config: Result<skyhook::config::Config, skyhook::config::ConfigError>,
) -> Result<skyhook::provider::dialect::codex::auth::Issuer, AuthError> {
    use skyhook::config::ConfigError;
    match config {
        Ok(config) => Ok(config.codex_issuer()?),
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
            auth::AuthStatus::LoggedIn { expires_at, .. } => println!(
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

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--askpass") {
        let Some(socket) = std::env::var_os("SKYHOOK_ASKPASS_SOCKET") else {
            std::process::exit(1)
        };
        let prompt = std::env::args().nth(2).unwrap_or_default();
        if let Err(error) =
            skyhook::remote::run_askpass_helper(std::path::Path::new(&socket), prompt)
        {
            eprintln!("skyhook askpass: {error}");
            std::process::exit(1);
        }
        return;
    }
    let invocation = cli::parse_from(std::env::args_os()).unwrap_or_else(|error| error.exit());
    // SAFETY: startup is still single-threaded: no Tokio runtime, terminal,
    // tracing subscriber, provider, or background worker has been started.
    // Parse Clap first so help/version do not depend on a valid .env file.
    if let Err(error) = unsafe { dotenv::load_invocation_env() } {
        eprintln!("skyhook: {error}");
        std::process::exit(1);
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            eprintln!("skyhook: could not start async runtime");
            std::process::exit(1);
        }
    };
    runtime.block_on(run(invocation));
}

async fn run(invocation: Invocation) {
    match invocation {
        Invocation::Inspect(request) => {
            if let Err(error) = dump::run(request).await {
                eprintln!(
                    "skyhook dump: {}",
                    skyhook::tool::diagnostic::escape_controls(error)
                );
                std::process::exit(1);
            }
        }
        Invocation::Stats(request) => {
            if let Err(error) = stats::run(request).await {
                eprintln!(
                    "skyhook stats: {}",
                    skyhook::tool::diagnostic::escape_controls(error)
                );
                std::process::exit(1);
            }
        }
        Invocation::Auth(command) => {
            if let Err(error) = run_auth(command).await {
                eprintln!("skyhook auth: {error}");
                std::process::exit(1);
            }
        }
        Invocation::Headless(request, input) => {
            // Do not install any terminal, renderer, or tracing subscriber here.
            if let Err(error) = headless::run(request, input).await {
                eprintln!("skyhook: {error}");
                std::process::exit(1);
            }
        }
        Invocation::Interactive(request, input) => {
            if let Err(error) = tui::run(request, input).await {
                eprintln!("skyhook: {error}");
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skyhook::config::ConfigError;

    #[test]
    fn login_issuer_defaults_without_config_and_propagates_config_errors() {
        let issuer = |config| codex_issuer(config).map(|issuer| issuer.to_string());
        let missing = ConfigError::Missing(Default::default());
        assert_eq!(issuer(Err(missing)).unwrap(), "https://auth.openai.com/");
        let other = ConfigError::Structure("broken".into());
        assert!(matches!(issuer(Err(other)), Err(AuthError::Config(_))));
    }
}
