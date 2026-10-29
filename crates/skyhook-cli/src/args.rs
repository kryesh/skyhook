//! Syntax-only CLI admission. Paths and inputs remain unread until their host starts.
use super::{InitialInput, NoArgs, NoCommand, Product};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use skyhook::{
    identity::SessionId,
    provider::profile::{ModelRef, ProviderName},
    tool::policy::{Capability, ModeName},
};
use std::path::PathBuf;

/// Session options belong to session execution only: a subcommand takes just its own.
#[derive(Parser)]
#[group(skip)]
#[command(
    version,
    args_conflicts_with_subcommands = true,
    disable_help_subcommand = true
)]
struct Args<A: clap::Args = NoArgs, C: Subcommand = NoCommand> {
    #[command(subcommand)]
    command: Option<Command<A, C>>,
    #[command(flatten)]
    session: SessionArgs<A>,
    /// Start the new session prompt in this mode instead of the default.
    #[arg(long, value_name = "NAME")]
    mode: Option<ModeName>,
}

/// What a session runs with, in the terminal or as a batch job.
#[derive(clap::Args)]
#[group(skip)]
struct SessionArgs<A: clap::Args> {
    #[command(flatten)]
    product: A,
    #[command(flatten)]
    config: ConfigRequest,
    /// Resume an existing session id.
    #[arg(long)]
    resume: Option<SessionId>,
    /// Select and remember the root model for a new session; a resumed session
    /// switches its root to it before anything runs, a prompt or script included.
    #[arg(short = 'm', long = "model", value_name = "PROVIDER/MODEL")]
    model: Option<ModelRef>,
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
struct Capabilities(Vec<Capability>);

fn parse_capabilities(value: &str) -> Result<Capabilities, String> {
    if value.is_empty() {
        return Ok(Capabilities(Vec::new()));
    }
    value
        .split(',')
        .map(|name| {
            let capability = name
                .trim()
                .parse::<Capability>()
                .map_err(|error| error.to_string())?;
            if capability == Capability::Interactive {
                return Err("interactive is never available to a batch job".into());
            }
            Ok(capability)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Capabilities)
}

#[derive(Subcommand)]
enum Command<A: clap::Args, C: Subcommand> {
    #[command(flatten)]
    Product(C),
    /// Run one prompt or script without terminal interaction, printing the session id.
    Batch {
        #[command(flatten)]
        session: SessionArgs<A>,
        /// Run in this mode instead of the default.
        #[arg(long, value_name = "NAME", conflicts_with = "capabilities")]
        mode: Option<ModeName>,
        /// Exact comma-separated policy capabilities, instead of a mode.
        #[arg(long, value_name = "LIST", value_parser = parse_capabilities)]
        capabilities: Option<Capabilities>,
    },
    /// Manage provider credentials.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Inspect effective config (default) or loaded skills without starting a session.
    /// The config dump takes the product's session options and shows what a session
    /// started with them would run with.
    Dump {
        #[arg(value_enum, default_value = "config")]
        what: DumpKind,
        #[command(flatten)]
        product: A,
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
/// A configured provider whose credentials Skyhook manages.
#[derive(clap::Args)]
#[group(skip)]
pub(super) struct AuthTarget {
    /// The provider's name in the configuration.
    pub(super) provider: ProviderName,
    #[command(flatten)]
    pub(super) source: ConfigSource,
}

#[derive(Subcommand)]
pub(super) enum AuthCommand {
    /// Sign in using a browser, or the public device flow on a headless machine.
    Login {
        #[command(flatten)]
        target: AuthTarget,
        #[arg(long)]
        headless: bool,
    },
    /// Inspect managed credentials without displaying tokens.
    Status {
        #[command(flatten)]
        target: AuthTarget,
        /// Also present them to the service, in a request that spends no quota.
        #[arg(long)]
        check: bool,
    },
}

impl AuthCommand {
    pub(super) fn target(&self) -> &AuthTarget {
        match self {
            Self::Login { target, .. } | Self::Status { target, .. } => target,
        }
    }
}
/// Configuration discovery, shared by execution, config inspection and login.
#[derive(clap::Args)]
#[group(skip)]
pub(super) struct ConfigSource {
    /// Workspace whose `.skyhook/config.yaml` overlays the user config; sessions' coding tools work in it.
    #[arg(short, long, default_value = ".")]
    pub(super) workspace: PathBuf,
    /// Use only this YAML config, regardless of extension; disable user/workspace discovery and merging.
    #[arg(short, long)]
    pub(super) config: Option<PathBuf>,
}

/// Configuration discovery and CLI policy overrides, shared by execution and
/// config inspection. No model selection, input, or session state belongs here.
#[derive(clap::Args)]
#[group(skip)]
pub(super) struct ConfigRequest {
    #[command(flatten)]
    pub(super) source: ConfigSource,
    /// Approve every tool invocation without prompting.
    #[arg(short, long)]
    pub(super) approve_all: bool,
}

pub(super) enum Inspection<A = NoArgs> {
    Config(ConfigRequest, A),
    /// Skill discovery only needs the workspace.
    Skills(PathBuf),
}

/// What `stats` reports on.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum StatsTarget {
    /// The workspace's sessions with their totals.
    List,
    Session(SessionId, Option<StatsFormat>),
}

pub(super) struct StatsRequest {
    pub(super) target: StatsTarget,
    pub(super) workspace: PathBuf,
}

/// A configured mode (the default when unnamed), or a batch job's exact capabilities.
pub(super) enum PermissionArgs {
    Mode(Option<ModeName>),
    Exact(Vec<Capability>),
}

pub(super) struct ExecutionRequest {
    pub(super) config: ConfigRequest,
    pub(super) resume: Option<SessionId>,
    pub(super) model: Option<ModelRef>,
}

pub(super) struct BatchRequest {
    pub(super) execution: ExecutionRequest,
    pub(super) permissions: PermissionArgs,
}

/// The terminal always runs in a configured mode; an explicit name may be one only
/// the resumed session still knows.
pub(super) struct InteractiveRequest {
    pub(super) execution: ExecutionRequest,
    pub(super) mode: Option<ModeName>,
}

pub(super) enum Invocation<P: Product> {
    Product(P::Command),
    Auth(AuthCommand),
    Inspect(Inspection<P::Args>),
    Stats(StatsRequest),
    Headless(BatchRequest, InitialInput, P::Args),
    Interactive(InteractiveRequest, Option<InitialInput>, P::Args),
}

pub(super) fn parse_product<P, I, T>(product: &P, args: I) -> Result<Invocation<P>, clap::Error>
where
    P: Product,
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let identity = product.identity();
    let mut command = Args::<P::Args, P::Command>::command()
        .name(identity.name().to_owned())
        .about(identity.about);
    let matches = command.try_get_matches_from_mut(args)?;
    if let Some((_, sub)) = matches.subcommand() {
        admit_command(Command::from_arg_matches(&matches)?, given::<P::Args>(sub))
    } else {
        let (execution, input, extra) = SessionArgs::from_arg_matches(&matches)?.into_request();
        let request = InteractiveRequest {
            execution,
            mode: matches.get_one::<ModeName>("mode").cloned(),
        };
        Ok(Invocation::Interactive(request, input, extra))
    }
}

fn conflict(message: &str) -> clap::Error {
    clap::Error::raw(clap::error::ErrorKind::ArgumentConflict, message)
}

/// Whether `matches` hold a product option the command line gave, which clap
/// cannot tie to a subcommand's own value for a generic product.
fn given<A: clap::Args>(matches: &clap::ArgMatches) -> bool {
    let product = A::augment_args(clap::Command::new("product"));
    product.get_arguments().any(|argument| {
        matches
            .try_get_raw(argument.get_id().as_str())
            .is_ok_and(|_| {
                matches.value_source(argument.get_id().as_str())
                    == Some(clap::parser::ValueSource::CommandLine)
            })
    })
}

/// `product_given`: the command line gave product session options, which only
/// the commands that start or inspect a session take.
fn admit_command<P: Product>(
    command: Command<P::Args, P::Command>,
    product_given: bool,
) -> Result<Invocation<P>, clap::Error> {
    use Invocation as Inv;
    // Clap enforces the external grammar; only the supplemental conflicts
    // that it cannot express are checked here.
    match command {
        Command::Product(command) => Ok(Inv::Product(command)),
        Command::Auth { command } => Ok(Inv::Auth(command)),
        Command::Dump {
            what: DumpKind::Skills,
            config,
            product: _,
        } => {
            if config.source.config.is_some() || config.approve_all || product_given {
                return Err(conflict(
                    "dump skills uses skill discovery, not --config, --approve-all or session options",
                ));
            }
            Ok(Inv::Inspect(Inspection::Skills(config.source.workspace)))
        }
        Command::Dump {
            what: DumpKind::Config,
            config,
            product,
        } => Ok(Inv::Inspect(Inspection::Config(config, product))),
        Command::Stats {
            session,
            list: _,
            workspace,
            format,
        } => {
            let target = match session {
                Some(session) => StatsTarget::Session(session, format),
                None => StatsTarget::List,
            };
            Ok(Inv::Stats(StatsRequest { target, workspace }))
        }
        Command::Batch {
            session,
            mode,
            capabilities,
        } => {
            let (execution, input, extra) = session.into_request();
            if execution.resume.is_some()
                && mode.is_some()
                && matches!(input, Some(InitialInput::Script(_)))
            {
                return Err(conflict(
                    "--mode changes a resumed session with its next prompt; a script has none",
                ));
            }
            let permissions = match capabilities {
                Some(Capabilities(capabilities)) => PermissionArgs::Exact(capabilities),
                None => PermissionArgs::Mode(mode),
            };
            let input = input.ok_or_else(|| {
                clap::Error::raw(
                    clap::error::ErrorKind::MissingRequiredArgument,
                    "batch requires --prompt or --script\n",
                )
            })?;
            let request = BatchRequest {
                execution,
                permissions,
            };
            Ok(Inv::Headless(request, input, extra))
        }
    }
}

impl<A: clap::Args> SessionArgs<A> {
    fn into_request(self) -> (ExecutionRequest, Option<InitialInput>, A) {
        // Clap rejects both together: `--prompt` is `conflicts_with = "script"`.
        let input = match self.prompt {
            Some(text) => Some(InitialInput::Prompt {
                text,
                images: self.images,
            }),
            None => self.script.map(InitialInput::Script),
        };
        let request = ExecutionRequest {
            config: self.config,
            resume: self.resume,
            model: self.model,
        };
        (request, input, self.product)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::path::Path;

    const ID: &str = "00000000000000000000000000000001";

    pub(crate) fn parse_from<I, T>(args: I) -> Result<Invocation<crate::Skyhook>, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        parse_product(&crate::Skyhook, args)
    }

    fn parse(args: &[&str]) -> Option<Args> {
        Args::try_parse_from(std::iter::once(&"skyhook").chain(args)).ok()
    }

    #[test]
    fn auth_batch_and_permission_arguments_parse_or_are_rejected() {
        let command = |args: &[&str]| parse(args).unwrap().command;
        assert!(matches!(
            command(&["auth", "login", "work", "--headless"]),
            Some(Command::Auth {
                command: AuthCommand::Login { headless: true, .. }
            })
        ));
        let Some(Command::Auth { command: status }) =
            command(&["auth", "status", "work", "-w", "w"])
        else {
            panic!("auth invocation");
        };
        assert!(matches!(status, AuthCommand::Status { .. }));
        let target = status.target();
        assert_eq!(target.provider.as_str(), "work");
        assert_eq!(target.source.workspace, Path::new("w"));
        assert!(command(&["--prompt", "hello"]).is_none());
        // A batch job takes exactly one input; allowlists must name real capabilities.
        for (args, valid) in [
            (&["batch"][..], false),
            (&["batch", "-p", "hello"], true),
            (&["batch", "-s", "run.js"], true),
            (&["batch", "-p", "hello", "-s", "run.js"], false),
            (&["batch", "-p", "x", "--mode", "readonly"], true),
            (
                &["batch", "-p", "x", "--mode", "m", "--capabilities", "read"],
                false,
            ),
            (&["batch", "-p", "x", "--capabilities", "read,typo"], false),
            (&["batch", "-p", "x", "--capabilities", "read,"], false),
            (
                &["batch", "-p", "x", "--capabilities", "interactive"],
                false,
            ),
            (&["--non-interactive", "-p", "hello"], false),
            (&["--capabilities", "read"], false),
            (&["--mode", "readonly", "-p", "hello"], true),
            (&["--mode", "readonly", "batch", "-p", "hello"], false),
            (&["batch", "--resume", ID, "--mode", "m", "-s", "x"], false),
            (&["batch", "--resume", ID, "--mode", "m", "-p", "x"], true),
            (
                &["batch", "--resume", ID, "--model", "p/m", "-s", "x"],
                true,
            ),
        ] {
            let parsed = parse_from(std::iter::once(&"skyhook").chain(args));
            assert_eq!(parsed.is_ok(), valid, "{args:?}");
        }
        let permissions = |args: &[&str]| match parse_from([&["skyhook"], args].concat()) {
            Ok(Invocation::Headless(request, _, _)) => request.permissions,
            _ => panic!("batch invocation"),
        };
        assert!(matches!(
            permissions(&["batch", "-p", "x", "--capabilities="]),
            PermissionArgs::Exact(capabilities) if capabilities.is_empty()
        ));
        assert!(matches!(
            permissions(&["batch", "-p", "x", "--capabilities", "read,exec,targets"]),
            PermissionArgs::Exact(capabilities)
                if capabilities == [Capability::Read, Capability::Exec, Capability::Targets]
        ));
        assert!(matches!(
            permissions(&["batch", "-p", "x", "--mode", "m"]),
            PermissionArgs::Mode(Some(mode)) if mode.as_str() == "m"
        ));
        assert!(matches!(
            permissions(&["batch", "-s", "x"]),
            PermissionArgs::Mode(None)
        ));
        assert!(matches!(
            parse_from(["skyhook", "--mode", "m"]),
            Ok(Invocation::Interactive(request, _, _)) if request.mode.as_ref().is_some_and(|mode| mode.as_str() == "m")
        ));
        // A model is named where it is given: an unqualified name says how.
        assert!(matches!(
            parse_from(["skyhook", "-m", "local/first"]),
            Ok(Invocation::Interactive(request, _, _))
                if request.execution.model.as_ref().is_some_and(|model| model.to_string() == "local/first")
        ));
        let Err(unqualified) = parse_from(["skyhook", "batch", "-p", "x", "-m", "first"]) else {
            panic!("an unqualified model is rejected");
        };
        let unqualified = unqualified.to_string();
        assert!(
            unqualified.contains("a model is named provider/model"),
            "{unqualified}"
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
            &["--model", "local/first"],
            &["--resume", ID],
            &["auth", "status", "codex"],
        ] {
            assert!(parse(&[&["dump", "config"], extra].concat()).is_none());
            assert!(parse(&[extra, &["dump", "config"]].concat()).is_none());
        }
        assert!(parse(&["--workspace", "w", "dump"]).is_none());
        assert!(parse(&["batch", "-p", "x", "dump"]).is_none());
        assert!(parse(&["dump", "--capabilities", "read"]).is_none());
        assert!(parse(&["dump", "unknown"]).is_none());
        assert!(parse(&["help"]).is_none());
        assert!(
            parse(&["-c", "c.yaml", "-a", "-p", "x"])
                .unwrap()
                .session
                .config
                .approve_all
        );
        let skills = parse(&["dump", "skills", "--config", "other.yaml"]).unwrap();
        assert!(admit_command::<crate::Skyhook>(skills.command.unwrap(), false).is_err());
        let stats = |args: &[&str]| match parse_from([&["skyhook", "stats"], args].concat()) {
            Ok(Invocation::Stats(request)) => request,
            _ => panic!("stats invocation"),
        };
        let session = |format| StatsTarget::Session(ID.parse().unwrap(), format);
        let request = stats(&[ID]);
        assert_eq!(request.target, session(None));
        assert_eq!(request.workspace, Path::new("."));
        let request = stats(&[ID, "-w", "w", "-f", "json"]);
        assert_eq!(request.workspace, Path::new("w"));
        assert_eq!(request.target, session(Some(StatsFormat::Json)));
        assert_eq!(
            stats(&["--format", "tree", ID]).target,
            session(Some(StatsFormat::Tree))
        );
        assert_eq!(
            stats(&[ID, "--format", "markdown"]).target,
            session(Some(StatsFormat::Markdown))
        );
        assert_eq!(stats(&["-l"]).target, StatsTarget::List);
        for args in [
            &["stats"][..],
            &["stats", ID, "--list"],
            &["stats", "--list", "--format", "json"],
            &["stats", "not-an-id"],
            &["stats", ID, "--format", "csv"],
            &["stats", ID, "--config", "c.yaml"],
        ] {
            assert!(parse(args).is_none(), "{args:?}");
        }
    }

    #[test]
    fn invocation_conversion_matrix_preserves_input_presence_and_modes() {
        assert!(matches!(
            parse_from(["skyhook", "auth", "status", "codex"]).unwrap(),
            Invocation::Auth(AuthCommand::Status { .. })
        ));
        assert!(parse_from(["skyhook", "--approve-all", "auth", "status", "codex"]).is_err());
        assert!(matches!(
            parse_from(["skyhook", "dump"]).unwrap(),
            Invocation::Inspect(Inspection::Config(_, _))
        ));
        assert!(matches!(
            parse_from(["skyhook", "dump", "skills"]).unwrap(),
            Invocation::Inspect(Inspection::Skills(_))
        ));
        assert!(matches!(
            parse_from(["skyhook"]).unwrap(),
            Invocation::Interactive(_, None, _)
        ));
        for headless in [false, true] {
            for script in [false, true] {
                let mode = if headless { &["batch"][..] } else { &[] };
                let input = if script {
                    &["--script", "relative/workflow.js"][..]
                } else {
                    &["--prompt", "", "--image", "relative/image.png"]
                };
                let args = [&["skyhook"], mode, input].concat();
                let input = match parse_from(args).unwrap() {
                    Invocation::Headless(_, input, _) if headless => input,
                    Invocation::Interactive(_, Some(input), _) if !headless => input,
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

    #[derive(clap::Args)]
    struct ResearchArgs {
        #[arg(long)]
        run: Option<String>,
    }
    #[derive(Subcommand)]
    enum ResearchCommand {
        Runs,
    }
    struct Research;
    impl Product for Research {
        type Args = ResearchArgs;
        type Command = ResearchCommand;
        fn identity(&self) -> super::super::Identity {
            super::super::Identity {
                host: "research".parse().unwrap(),
                ..crate::Skyhook.identity()
            }
        }
        async fn command(&self, _: ResearchCommand) -> Result<(), super::super::Error> {
            Ok(())
        }
    }

    /// Product options join the session grammar at the root, under `batch` and for the
    /// config dump; shared commands and the product's own take only their own arguments.
    #[test]
    fn products_compose_session_args_and_commands_without_changing_shared_grammar() {
        let product = ["--run", "first", "-m", "provider/model", "--prompt", "go"];
        match parse_product(&Research, [&["research"], &product[..]].concat()).unwrap() {
            Invocation::Interactive(request, Some(InitialInput::Prompt { text, .. }), extra) => {
                assert_eq!(
                    request.execution.model.unwrap().to_string(),
                    "provider/model"
                );
                assert_eq!(extra.run.as_deref(), Some("first"));
                assert_eq!(text, "go");
            }
            _ => panic!("interactive invocation"),
        }
        match parse_product(&Research, [&["research", "batch"], &product[..]].concat()).unwrap() {
            Invocation::Headless(_, _, extra) => assert_eq!(extra.run.as_deref(), Some("first")),
            _ => panic!("batch invocation"),
        }
        assert!(matches!(
            parse_product(&Research, ["research", "dump", "--run", "first"]).unwrap(),
            Invocation::Inspect(Inspection::Config(_, args)) if args.run.as_deref() == Some("first")
        ));
        assert!(matches!(
            parse_product(&Research, ["research", "runs"]).unwrap(),
            Invocation::Product(ResearchCommand::Runs)
        ));
        assert!(parse_product(&Research, ["research", "--run", "first", "runs"]).is_err());
        assert!(
            parse_product(&Research, ["research", "stats", "--list", "--run", "first"]).is_err()
        );
    }
}
