mod cli;
mod dotenv;
mod dump;
mod embedded_shims;
mod headless;
mod interaction;
mod launch;
mod tui;
use cli::{AuthCommand, AuthProvider, Invocation};
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

async fn run_auth(
    command: AuthCommand,
) -> Result<(), skyhook::provider::backends::codex::auth::AuthError> {
    use skyhook::provider::backends::codex::auth;
    match command {
        AuthCommand::Login {
            provider: AuthProvider::Codex,
            headless,
        } => {
            auth::login(headless).await?;
            println!("Signed in to Codex for Skyhook.");
        }
        AuthCommand::Status {
            provider: AuthProvider::Codex,
        } => match auth::status().await? {
            auth::AuthStatus::LoggedOut => {
                println!("Codex: not signed in to Skyhook. Run skyhook auth login.")
            }
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
    // Clap normally prints parse errors. Machine-mode failures are silent even
    // before a session exists; explicit help/version retain their normal output.
    let cli: Vec<_> = std::env::args_os().collect();
    let dumping = cli
        .iter()
        .skip(1)
        .take_while(|arg| arg.as_os_str() != "--")
        .any(|arg| arg == "--dump" || arg.to_str().is_some_and(|arg| arg.starts_with("--dump=")));
    let silent = !dumping
        && cli
            .iter()
            .skip(1)
            .take_while(|arg| arg.as_os_str() != "--")
            .any(|arg| {
                arg == "--non-interactive"
                    || arg
                        .to_str()
                        .is_some_and(|arg| arg.starts_with("--non-interactive="))
            });
    let invocation = match cli::parse_from(cli) {
        Ok(invocation) => invocation,
        Err(error) => {
            if !silent
                || matches!(
                    error.kind(),
                    clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
                )
            {
                error.exit();
            }
            std::process::exit(2);
        }
    };
    // SAFETY: startup is still single-threaded: no Tokio runtime, terminal,
    // tracing subscriber, provider, or background worker has been started.
    // Parse Clap first so help/version do not depend on a valid .env file.
    if let Err(error) = unsafe { dotenv::load_invocation_env() } {
        if !silent {
            eprintln!("skyhook: {error}");
        }
        std::process::exit(1);
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            if !silent {
                eprintln!("skyhook: could not start async runtime");
            }
            std::process::exit(1);
        }
    };
    runtime.block_on(run(invocation));
}

async fn run(invocation: Invocation) {
    match invocation {
        Invocation::Inspect(request) => {
            if let Err(error) = dump::run(request).await {
                eprintln!("skyhook dump: {}", dump::diagnostic_text(error));
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
            if headless::run(request, input).await.is_err() {
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
