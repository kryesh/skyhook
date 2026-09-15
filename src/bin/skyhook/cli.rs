//! Syntax-only CLI admission. Paths and inputs remain unread until their host starts.
use clap::{Parser, Subcommand, ValueEnum};
use skyhook::{identity::SessionId, tool::policy::Capability};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "skyhook", version, about = "Programmable coding-agent harness")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// Inspect effective config (default) or loaded skills without starting a session.
    #[arg(
        long,
        value_enum,
        num_args = 0..=1,
        default_missing_value = "config",
        value_name = "WHAT",
        conflicts_with_all = ["input", "resume", "images", "non_interactive", "model"]
    )]
    dump: Option<DumpKind>,
    /// Use only this TOML config; disable user/workspace config discovery and merging.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum DumpKind {
    Config,
    Skills,
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
pub(super) struct ConfigRequest {
    pub(super) workspace: PathBuf,
    pub(super) config: Option<PathBuf>,
    pub(super) capabilities: Option<Capabilities>,
    pub(super) approve_all: bool,
}

pub(super) enum Inspection {
    Config(ConfigRequest),
    /// Skill discovery only needs the workspace.
    Skills(PathBuf),
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
        // Clap enforces the external grammar; only the supplemental dump and
        // auth diagnostics that it cannot express are checked here.
        if args.dump.is_some() && args.command.is_some() {
            return Err(conflict("--dump cannot be combined with an auth command"));
        }
        if args.dump == Some(DumpKind::Skills)
            && (args.config.is_some() || args.capabilities.is_some() || args.approve_all)
        {
            return Err(conflict(
                "--dump skills uses skill discovery, not --config, --capabilities, or --approve-all",
            ));
        }
        if args.non_interactive && args.command.is_some() {
            return Err(conflict(
                "--non-interactive cannot be combined with an auth command",
            ));
        }
        // Clap rejects both together: `--prompt` is `conflicts_with = "script"`.
        let input = match args.prompt {
            Some(text) => Some(InitialInput::Prompt {
                text,
                images: args.images,
            }),
            None => args.script.map(InitialInput::Script),
        };
        // Authentication historically accepts otherwise unrelated execution flags
        // (except --dump/--non-interactive). Preserve that grammar and ignore them.
        if let Some(Command::Auth { command }) = args.command {
            return Ok(Self::Auth(command));
        }
        if args.dump == Some(DumpKind::Skills) {
            return Ok(Self::Inspect(Inspection::Skills(args.workspace)));
        }
        let config = ConfigRequest {
            workspace: args.workspace,
            config: args.config,
            capabilities: args.capabilities,
            approve_all: args.approve_all,
        };
        if args.dump == Some(DumpKind::Config) {
            return Ok(Self::Inspect(Inspection::Config(config)));
        }
        let request = ExecutionRequest {
            config,
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
        let capabilities = |args: &[&str]| parse(args).unwrap().capabilities.unwrap().0;
        assert!(capabilities(&["--capabilities="]).is_empty());
        assert_eq!(
            capabilities(&["--capabilities", "read,exec,targets"]),
            [Capability::Read, Capability::Exec, Capability::Targets]
        );
    }

    #[test]
    fn dump_selectors_and_execution_conflicts() {
        for (args, expected) in [
            (&["--dump"][..], DumpKind::Config),
            (&["--dump", "config"], DumpKind::Config),
            (&["--dump", "skills"], DumpKind::Skills),
        ] {
            assert_eq!(parse(args).unwrap().dump, Some(expected));
        }
        for extra in [
            &["--prompt", "hello"][..],
            &["--script", "run.js"],
            &["--model", "first"],
            &["--non-interactive"],
            &["--resume", "00000000000000000000000000000001"],
        ] {
            assert!(parse(&[&["--dump=config"], extra].concat()).is_none());
        }
        assert!(parse(&["--dump", "unknown"]).is_none());
        for args in [
            &["--dump=config", "auth", "status"][..],
            &["--dump", "skills", "--config", "other.toml"],
        ] {
            assert!(Invocation::try_from(parse(args).unwrap()).is_err());
        }
    }

    #[test]
    fn invocation_conversion_matrix_preserves_input_presence_and_modes() {
        // Auth accepts but ignores otherwise unrelated execution options.
        assert!(matches!(
            parse_from([
                "skyhook",
                "--workspace",
                "missing",
                "--config",
                "missing.toml",
                "--model",
                "unknown",
                "--capabilities=",
                "--approve-all",
                "--prompt",
                "ignored",
                "--image",
                "missing.png",
                "auth",
                "status",
            ])
            .unwrap(),
            Invocation::Auth(AuthCommand::Status { .. })
        ));
        assert!(matches!(
            parse_from(["skyhook", "--dump"]).unwrap(),
            Invocation::Inspect(Inspection::Config(_))
        ));
        assert!(matches!(
            parse_from(["skyhook", "--dump=skills"]).unwrap(),
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
