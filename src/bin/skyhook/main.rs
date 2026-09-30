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
use cli::{AuthCommand, AuthTarget, Invocation};
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Why an `auth` command failed.
#[derive(Debug, thiserror::Error)]
enum AuthError {
    #[error(transparent)]
    Config(#[from] skyhook::config::ConfigError),
    #[error(transparent)]
    Login(#[from] skyhook::provider::dialect::LoginError),
    #[error(transparent)]
    Provider(#[from] skyhook::provider::ProviderError),
    #[error(transparent)]
    Check(#[from] skyhook::provider::dialect::BuildError),
    #[error(transparent)]
    LoginRequired(#[from] skyhook::provider::dialect::LoginRequired),
}

/// Print `text` with its terminal controls escaped: provider names, paths and
/// what the service reports all reach the terminal.
fn say(text: impl std::fmt::Display) {
    println!("{}", skyhook::tool::diagnostic::escape_controls(text));
}

async fn run_auth(command: AuthCommand) -> Result<(), AuthError> {
    use skyhook::provider::dialect::AuthStatus;
    let AuthTarget { provider, source } = command.target();
    let config = skyhook::config::Config::resolve(&source.workspace, source.config.as_deref())
        .await?
        .config;
    let login = config.login(provider)?;
    match command {
        AuthCommand::Login { headless, .. } => {
            login.login(headless).await?;
            say(format_args!("Signed in `{provider}` for Skyhook."));
        }
        AuthCommand::Status { check, .. } => match login.status().await? {
            AuthStatus::LoginRequired(required) if check => return Err(required.into()),
            AuthStatus::LoginRequired(required) => say(format_args!("{required}.")),
            AuthStatus::LoggedIn { expires_at } => {
                say(format_args!(
                    "`{provider}`: Skyhook credentials present (access token expires at Unix time {expires_at}; refreshed automatically when needed)."
                ));
                if check {
                    let usage = login.check().await?;
                    say(format_args!("`{provider}`: {}.", accepted(&usage)));
                }
            }
        },
    }
    Ok(())
}

/// What a check found: the service accepted the credentials, with the plan
/// and window use it reported.
fn accepted(usage: &skyhook::provider::dialect::Usage) -> String {
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
}
