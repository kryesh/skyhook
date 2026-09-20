//! Syntax-only CLI admission. Paths and inputs remain unread until their host starts.
use clap::{Parser, Subcommand, ValueEnum};
use skyhook::{identity::SessionId, tool::policy::Capability};
use std::path::PathBuf;

/// Session options belong to session execution only: a subcommand takes just its own.
#[derive(Parser)]
#[command(
    name = "skyhook",
    version,
    about = "Programmable coding-agent harness",
    args_conflicts_with_subcommands = true,
    disable_help_subcommand = true
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    config: ConfigRequest,
    /// Resume an existing session id.
    #[arg(long)]
    resume: Option<SessionId>,
    /// Select and remember the root model for a new session.
    #[arg(short = 'm', long = "model")]
    model: Option<String>,
    /// Run without terminal interaction; requires --prompt or --script.
    #[arg(long, requires = "input")]
    non_interactive: bool,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum DumpKind {
    Config,
    Skills,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(super) enum StatsFormat {
    /// The agent, model, and tool tables as a Markdown document.
    Markdown,
    /// The agent tree with each agent's figures on its line.
    Tree,
    /// Every figure, including per-agent tool breakdowns.
    Json,
}

#[derive(Clone, Debug)]
pub(super) struct Capabilities(pub(super) Vec<Capability>);

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
    /// Inspect effective config (default) or loaded skills without starting a session.
    Dump {
        #[arg(value_enum, default_value = "config")]
        what: DumpKind,
        #[command(flatten)]
        config: ConfigRequest,
    },
    /// Report a saved session's token usage, model requests, delegation, and tool calls.
    Stats {
        /// The session id, as printed by a headless run or listed by --list.
        #[arg(required_unless_present = "list", conflicts_with = "list")]
        session: Option<SessionId>,
        /// List the workspace's sessions with their totals instead.
        #[arg(short, long, conflicts_with = "format")]
        list: bool,
        /// Workspace whose session history holds the session.
        #[arg(short, long, default_value = ".")]
        workspace: PathBuf,
        /// Machine-readable output; omitted, the tables are rendered for reading.
        #[arg(short, long, value_enum)]
        format: Option<StatsFormat>,
    },
}
#[derive(Clone, Copy, ValueEnum)]
pub(super) enum AuthProvider {
    Codex,
}
#[derive(Subcommand)]
pub(super) enum AuthCommand {
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
/// Configuration discovery and CLI policy overrides, shared by execution and
/// config inspection. No model selection, input, or session state belongs here.
#[derive(clap::Args)]
pub(super) struct ConfigRequest {
    /// Workspace visible to coding tools.
    #[arg(short, long, default_value = ".")]
    pub(super) workspace: PathBuf,
    /// Use only this TOML config; disable user/workspace config discovery and merging.
    #[arg(short, long)]
    pub(super) config: Option<PathBuf>,
    /// Exact comma-separated policy capabilities; interaction follows the runtime mode.
    #[arg(long, value_name = "LIST", value_parser = parse_capabilities)]
    pub(super) capabilities: Option<Capabilities>,
    /// Approve every tool invocation without prompting.
    #[arg(short, long)]
    pub(super) approve_all: bool,
}

pub(super) enum Inspection {
    Config(ConfigRequest),
    /// Skill discovery only needs the workspace.
    Skills(PathBuf),
}

pub(super) struct StatsRequest {
    /// None lists the workspace's sessions.
    pub(super) session: Option<SessionId>,
    pub(super) workspace: PathBuf,
    pub(super) format: Option<StatsFormat>,
}

pub(super) struct ExecutionRequest {
    pub(super) config: ConfigRequest,
    pub(super) resume: Option<SessionId>,
    pub(super) model: Option<String>,
}

/// Only paths are admitted here: in particular headless input I/O must remain
/// after opening the session and flushing its ID to stdout.
pub(super) enum InitialInput {
    Prompt { text: String, images: Vec<PathBuf> },
    Script(PathBuf),
}

pub(super) enum Invocation {
    Auth(AuthCommand),
    Inspect(Inspection),
    Stats(StatsRequest),
    Headless(ExecutionRequest, InitialInput),
    Interactive(ExecutionRequest, Option<InitialInput>),
}

pub(super) fn parse_from<I, T>(args: I) -> Result<Invocation, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    Args::try_parse_from(args)?.try_into()
}

fn conflict(message: &str) -> clap::Error {
    clap::Error::raw(clap::error::ErrorKind::ArgumentConflict, message)
}

impl TryFrom<Args> for Invocation {
    type Error = clap::Error;

    fn try_from(args: Args) -> Result<Self, Self::Error> {
        // Clap enforces the external grammar; only the supplemental conflict
        // that it cannot express is checked here.
        match args.command {
            Some(Command::Auth { command }) => return Ok(Self::Auth(command)),
            Some(Command::Dump {
                what: DumpKind::Skills,
                config,
            }) => {
                if config.config.is_some() || config.capabilities.is_some() || config.approve_all {
                    return Err(conflict(
                        "dump skills uses skill discovery, not --config, --capabilities, or --approve-all",
                    ));
                }
                return Ok(Self::Inspect(Inspection::Skills(config.workspace)));
            }
            Some(Command::Dump {
                what: DumpKind::Config,
                config,
            }) => return Ok(Self::Inspect(Inspection::Config(config))),
            Some(Command::Stats {
                session,
                list: _,
                workspace,
                format,
            }) => {
                return Ok(Self::Stats(StatsRequest {
                    session,
                    workspace,
                    format,
                }));
            }
            None => {}
        }
        // Clap rejects both together: `--prompt` is `conflicts_with = "script"`.
        let input = match args.prompt {
            Some(text) => Some(InitialInput::Prompt {
                text,
                images: args.images,
            }),
            None => args.script.map(InitialInput::Script),
        };
        let request = ExecutionRequest {
            config: args.config,
            resume: args.resume,
            model: args.model,
        };
        if args.non_interactive {
            // Clap enforces `--non-interactive` `requires = "input"`.
            let input = input.expect("clap: --non-interactive requires --prompt or --script");
            Ok(Self::Headless(request, input))
        } else {
            Ok(Self::Interactive(request, input))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn parse(args: &[&str]) -> Option<Args> {
        Args::try_parse_from(std::iter::once(&"skyhook").chain(args)).ok()
    }

    #[test]
    fn auth_headless_and_capability_arguments_parse_or_are_rejected() {
        let command = |args: &[&str]| parse(args).unwrap().command;
        assert!(matches!(
            command(&["auth", "login", "--headless"]),
            Some(Command::Auth {
                command: AuthCommand::Login { headless: true, .. }
            })
        ));
        assert!(matches!(
            command(&["auth", "status", "codex"]),
            Some(Command::Auth {
                command: AuthCommand::Status { .. }
            })
        ));
        assert!(matches!(
            command(&["auth", "logout"]),
            Some(Command::Auth {
                command: AuthCommand::Logout { .. }
            })
        ));
        assert!(command(&["--prompt", "hello"]).is_none());
        // Headless mode requires exactly one input; allowlists must name real capabilities.
        assert!(
            parse(&["--non-interactive", "-p", "hello"])
                .unwrap()
                .non_interactive
        );
        for (args, valid) in [
            (&["--non-interactive"][..], false),
            (&["--non-interactive", "-s", "run.js"], true),
            (&["--non-interactive", "-p", "hello", "-s", "run.js"], false),
            (&["--capabilities", "read,typo"], false),
            (&["--capabilities", "read,"], false),
        ] {
            assert_eq!(parse(args).is_some(), valid, "{args:?}");
        }
        let capabilities = |args: &[&str]| parse(args).unwrap().config.capabilities.unwrap().0;
        assert!(capabilities(&["--capabilities="]).is_empty());
        assert_eq!(
            capabilities(&["--capabilities", "read,exec,targets"]),
            [Capability::Read, Capability::Exec, Capability::Targets]
        );
    }

    #[test]
    fn dump_and_stats_subcommands_take_only_their_own_options() {
        for (args, expected) in [
            (&["dump"][..], DumpKind::Config),
            (&["dump", "config"], DumpKind::Config),
            (&["dump", "skills", "-w", "w"], DumpKind::Skills),
        ] {
            assert!(matches!(
                parse(args).unwrap().command,
                Some(Command::Dump { what, .. }) if what == expected
            ));
        }
        // Session options are rejected around a subcommand, not silently ignored.
        for extra in [
            &["--prompt", "hello"][..],
            &["--script", "run.js"],
            &["--model", "first"],
            &["--resume", "00000000000000000000000000000001"],
            &["auth", "status"],
        ] {
            assert!(parse(&[&["dump", "config"], extra].concat()).is_none());
            assert!(parse(&[extra, &["dump", "config"]].concat()).is_none());
        }
        assert!(parse(&["--workspace", "w", "dump"]).is_none());
        assert!(parse(&["--non-interactive", "-p", "x", "dump"]).is_none());
        assert!(parse(&["dump", "unknown"]).is_none());
        assert!(parse(&["help"]).is_none());
        assert!(
            parse(&["-c", "c.toml", "-a", "-p", "x"])
                .unwrap()
                .config
                .approve_all
        );
        let skills = parse(&["dump", "skills", "--config", "other.toml"]).unwrap();
        assert!(Invocation::try_from(skills).is_err());
        let id = "00000000000000000000000000000001";
        let stats = |args: &[&str]| match parse_from([&["skyhook", "stats"], args].concat()) {
            Ok(Invocation::Stats(request)) => request,
            _ => panic!("stats invocation"),
        };
        let request = stats(&[id]);
        assert_eq!(request.session.unwrap().to_string(), id);
        assert_eq!(request.workspace, Path::new("."));
        assert_eq!(request.format, None);
        let request = stats(&[id, "-w", "w", "-f", "json"]);
        assert_eq!(request.workspace, Path::new("w"));
        assert_eq!(request.format, Some(StatsFormat::Json));
        assert_eq!(
            stats(&["--format", "tree", id]).format,
            Some(StatsFormat::Tree)
        );
        assert_eq!(
            stats(&[id, "--format", "markdown"]).format,
            Some(StatsFormat::Markdown)
        );
        assert_eq!(stats(&["-l"]).session, None);
        for args in [
            &["stats"][..],
            &["stats", id, "--list"],
            &["stats", "--list", "--format", "json"],
            &["stats", "not-an-id"],
            &["stats", id, "--format", "csv"],
            &["stats", id, "--config", "c.toml"],
        ] {
            assert!(parse(args).is_none(), "{args:?}");
        }
    }

    #[test]
    fn invocation_conversion_matrix_preserves_input_presence_and_modes() {
        assert!(matches!(
            parse_from(["skyhook", "auth", "status"]).unwrap(),
            Invocation::Auth(AuthCommand::Status { .. })
        ));
        assert!(parse_from(["skyhook", "--approve-all", "auth", "status"]).is_err());
        assert!(matches!(
            parse_from(["skyhook", "dump"]).unwrap(),
            Invocation::Inspect(Inspection::Config(_))
        ));
        assert!(matches!(
            parse_from(["skyhook", "dump", "skills"]).unwrap(),
            Invocation::Inspect(Inspection::Skills(_))
        ));
        assert!(matches!(
            parse_from(["skyhook"]).unwrap(),
            Invocation::Interactive(_, None)
        ));
        for headless in [false, true] {
            for script in [false, true] {
                let mode = if headless {
                    &["--non-interactive"][..]
                } else {
                    &[]
                };
                let input = if script {
                    &["--script", "relative/workflow.js"][..]
                } else {
                    &["--prompt", "", "--image", "relative/image.png"]
                };
                let args = [&["skyhook"], mode, input].concat();
                let input = match parse_from(args).unwrap() {
                    Invocation::Headless(_, input) if headless => input,
                    Invocation::Interactive(_, Some(input)) if !headless => input,
                    _ => panic!("wrong invocation mode"),
                };
                match input {
                    InitialInput::Prompt { text, images } if !script => {
                        assert!(text.is_empty());
                        assert_eq!(images, [PathBuf::from("relative/image.png")]);
                    }
                    InitialInput::Script(path) if script => {
                        assert_eq!(path, Path::new("relative/workflow.js"))
                    }
                    _ => panic!("wrong input kind"),
                }
            }
        }
    }
}
