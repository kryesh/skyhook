mod dotenv;
mod embedded_shims;
mod headless;
mod interaction;
mod launch;
mod tui;
use clap::{Parser, Subcommand, ValueEnum};
use skyhook::{identity::SessionId, tool::policy::Capability};
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
    /// Approve every tool invocation without prompting.
    #[arg(long)]
    approve_all: bool,
    /// Run without terminal interaction; requires --prompt or --script.
    #[arg(long, requires = "input")]
    non_interactive: bool,
    /// Exact comma-separated policy capabilities; interaction follows the runtime mode.
    #[arg(long, value_name = "LIST", value_parser = parse_capabilities)]
    capabilities: Option<Capabilities>,
    /// Attach images to the first prompt.
    #[arg(long = "image", requires = "prompt", conflicts_with = "script")]
    images: Vec<PathBuf>,
    /// Submit this initial prompt.
    #[arg(short, long, conflicts_with = "script", group = "input")]
    prompt: Option<String>,
    /// Run this JavaScript workflow.
    #[arg(
        short,
        long,
        value_name = "PATH",
        conflicts_with = "prompt",
        group = "input"
    )]
    script: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct Capabilities(Vec<Capability>);

fn parse_capabilities(value: &str) -> Result<Capabilities, String> {
    if value.is_empty() {
        return Ok(Capabilities(Vec::new()));
    }
    value
        .split(',')
        .map(|name| {
            let capability = name.trim().parse::<Capability>().map_err(|error| error.to_string())?;
            if capability == Capability::Interactive {
                return Err("interactive is controlled by the runtime mode; use --non-interactive to disable it".into());
            }
            Ok(capability)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Capabilities)
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
    let silent = cli
        .iter()
        .skip(1)
        .take_while(|arg| arg.as_os_str() != "--")
        .any(|arg| {
            arg == "--non-interactive"
                || arg
                    .to_str()
                    .is_some_and(|arg| arg.starts_with("--non-interactive="))
        });
    let args = match Args::try_parse_from(cli) {
        Ok(args) => args,
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
    if args.non_interactive && args.command.is_some() {
        std::process::exit(2);
    }
    // SAFETY: startup is still single-threaded: no Tokio runtime, terminal,
    // tracing subscriber, provider, or background worker has been started.
    // Parse Clap first so help/version do not depend on a valid .env file.
    if let Err(error) = unsafe { dotenv::load_invocation_env() } {
        if !args.non_interactive {
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
            if !args.non_interactive {
                eprintln!("skyhook: could not start async runtime");
            }
            std::process::exit(1);
        }
    };
    runtime.block_on(run(args));
}

async fn run(mut args: Args) {
    if let Some(Command::Auth { command }) = args.command.take() {
        if let Err(error) = run_auth(command).await {
            eprintln!("skyhook auth: {error}");
            std::process::exit(1);
        }
        return;
    }
    if args.non_interactive {
        // Do not install any terminal, renderer, or tracing subscriber here.
        if headless::run(args).await.is_err() {
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

#[cfg(test)]
mod headless_cli_tests {
    use super::*;

    #[test]
    fn headless_requires_exactly_one_input() {
        assert!(Args::try_parse_from(["skyhook", "--non-interactive"]).is_err());
        assert!(
            Args::try_parse_from(["skyhook", "--non-interactive", "-p", "hello"])
                .unwrap()
                .non_interactive
        );
        assert!(Args::try_parse_from(["skyhook", "--non-interactive", "-s", "run.js"]).is_ok());
        assert!(
            Args::try_parse_from([
                "skyhook",
                "--non-interactive",
                "-p",
                "hello",
                "-s",
                "run.js"
            ])
            .is_err()
        );
    }

    #[test]
    fn capability_allowlist_accepts_empty_and_rejects_unknown_names() {
        let empty = Args::try_parse_from(["skyhook", "--capabilities="]).unwrap();
        assert!(empty.capabilities.unwrap().0.is_empty());
        let selected =
            Args::try_parse_from(["skyhook", "--capabilities", "read,exec,targets"]).unwrap();
        assert_eq!(
            selected.capabilities.unwrap().0,
            vec![Capability::Read, Capability::Exec, Capability::Targets]
        );
        assert!(Args::try_parse_from(["skyhook", "--capabilities", "read,typo"]).is_err());
        assert!(Args::try_parse_from(["skyhook", "--capabilities", "read,"]).is_err());
    }
}
