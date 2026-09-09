mod embedded_shims;
mod interaction;
mod tui;
use clap::{Parser, Subcommand, ValueEnum};
use skyhook::identity::SessionId;
use std::path::PathBuf;
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(name = "skyhook", version, about = "Programmable coding-agent harness")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// Explicit TOML config used instead of the user config.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Workspace visible to coding tools.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Resume an existing session id.
    #[arg(long)]
    resume: Option<SessionId>,
    /// Select and remember the root model for a new session.
    #[arg(short = 'm', long = "model")]
    model: Option<String>,
    /// Override the configured root instruction profile.
    #[arg(long)]
    agent_profile: Option<String>,
    /// Approve every tool invocation without prompting.
    #[arg(long)]
    approve_all: bool,
    /// Attach images to the first prompt.
    #[arg(long = "image", requires = "prompt")]
    images: Vec<PathBuf>,
    /// Open the interface and submit this initial prompt.
    #[arg(short, long, conflicts_with = "script")]
    prompt: Option<String>,
    /// Open the interface and run this JavaScript workflow.
    #[arg(short, long, value_name = "PATH", conflicts_with = "prompt")]
    script: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Manage Skyhook-owned provider credentials.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
}
#[derive(Clone, Copy, ValueEnum)]
enum AuthProvider {
    Codex,
}
#[derive(Subcommand)]
enum AuthCommand {
    /// Sign in using a browser, or the public device flow on a headless machine.
    Login {
        #[arg(value_enum, default_value = "codex")]
        provider: AuthProvider,
        #[arg(long)]
        headless: bool,
    },
    /// Inspect Skyhook's credentials without displaying tokens.
    Status {
        #[arg(value_enum, default_value = "codex")]
        provider: AuthProvider,
    },
    /// Delete only Skyhook's locally stored credentials.
    Logout {
        #[arg(value_enum, default_value = "codex")]
        provider: AuthProvider,
    },
}
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

#[tokio::main]
async fn main() {
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
    let mut args = Args::parse();
    if let Some(Command::Auth { command }) = args.command.take() {
        if let Err(error) = run_auth(command).await {
            eprintln!("skyhook auth: {error}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(error) = tui::run(args).await {
        eprintln!("skyhook: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod auth_cli_tests {
    use super::*;
    #[test]
    fn auth_commands_parse_without_starting_tui() {
        assert!(matches!(
            Args::try_parse_from(["skyhook", "auth", "login", "--headless"])
                .unwrap()
                .command,
            Some(Command::Auth {
                command: AuthCommand::Login { headless: true, .. }
            })
        ));
        assert!(matches!(
            Args::try_parse_from(["skyhook", "auth", "status", "codex"])
                .unwrap()
                .command,
            Some(Command::Auth {
                command: AuthCommand::Status { .. }
            })
        ));
        assert!(matches!(
            Args::try_parse_from(["skyhook", "auth", "logout"])
                .unwrap()
                .command,
            Some(Command::Auth {
                command: AuthCommand::Logout { .. }
            })
        ));
        assert!(
            Args::try_parse_from(["skyhook", "--prompt", "hello"])
                .unwrap()
                .command
                .is_none()
        );
    }
}
