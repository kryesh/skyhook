//! Syntax-only CLI admission, shared by the binaries. Paths and inputs remain unread until
//! their session starts.
use crate::batch::BatchRequest;
use crate::config::{LayerResolutionError, ResolvedConfig};
use crate::dump::DumpRequest;
use crate::stats::{StatsRequest, StatsTarget};
use crate::tui::InteractiveRequest;
use clap::builder::{PossibleValuesParser, TypedValueParser};
use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use skyhook::identity::SessionId;
use skyhook::launch::request::{ExecutionRequest, InitialInput, PermissionRequest};
use skyhook::product::{Product, RootMode};
use skyhook::tool::policy::Capability;
use skyhook::{
    config::mode::ModeName,
    provider::model::{ModelRef, ProviderName},
};
use skyhook_base::enums::NamedEnum;
use std::path::{Path, PathBuf};

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
}

// The mode a switchable root starts in at the terminal. A doc comment here or on
// `Permissions` would replace the description of the command it augments.
#[derive(clap::Args)]
#[group(skip)]
struct Mode {
    /// Start the new session prompt in this mode instead of the default.
    #[arg(long, value_name = "NAME")]
    mode: Option<ModeName>,
}

// What a switchable root may do in a batch job.
#[derive(clap::Args)]
#[group(skip)]
struct Permissions {
    /// Run in this mode instead of the default.
    #[arg(long, value_name = "NAME", conflicts_with = "capabilities")]
    mode: Option<ModeName>,
    /// Exact comma-separated policy capabilities, instead of a mode.
    #[arg(long, value_name = "LIST", value_parser = parse_capabilities)]
    capabilities: Option<Capabilities>,
}

/// What a session runs with, in the terminal or as a batch job.
#[derive(clap::Args)]
#[group(skip)]
#[command(group = clap::ArgGroup::new("new").multiple(true).conflicts_with("resume"))]
struct SessionArgs<A: clap::Args> {
    #[command(flatten)]
    product: A,
    #[command(flatten)]
    config: ConfigArgs,
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

/// How a session's report is formatted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum StatsFormat {
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
            name.trim()
                .parse::<Capability>()
                .map_err(|error| error.to_string())
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
    },
    /// Manage provider credentials.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Inspect effective config (default) or loaded skills without starting a session.
    /// The config dump takes the product's session options and shows what a session
    /// started with them would run with.
    #[command(group = clap::ArgGroup::new("new").multiple(true))]
    Dump {
        #[arg(value_enum, default_value = "config")]
        what: DumpKind,
        #[command(flatten)]
        product: A,
        #[command(flatten)]
        config: ConfigArgs,
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
pub struct AuthTarget {
    /// The provider's name in the configuration.
    pub(crate) provider: ProviderName,
    #[command(flatten)]
    pub(crate) source: SourceArgs,
}

#[derive(Subcommand)]
pub enum AuthCommand {
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

/// Configuration discovery, shared by execution, config inspection and login.
#[derive(clap::Args)]
#[group(skip)]
pub struct SourceArgs {
    /// Workspace whose `.skyhook/config.yaml` overlays the user config; sessions' coding tools work in it.
    #[arg(short, long, default_value = ".")]
    pub(crate) workspace: PathBuf,
    /// Use only this YAML config, regardless of extension; disable user/workspace discovery and merging.
    #[arg(short, long)]
    pub(crate) config: Option<PathBuf>,
}

/// Configuration discovery and CLI policy overrides, shared by execution and
/// config inspection.
#[derive(clap::Args)]
#[group(skip)]
pub struct ConfigArgs {
    #[command(flatten)]
    pub(crate) source: SourceArgs,
    /// Approve every tool invocation without prompting.
    #[arg(short, long)]
    pub(crate) approve_all: bool,
}

/// A product's session options as its command line gives them: `O` once the configuration
/// file of its own that they name is read. One in the `new` group applies to a new session
/// only, so it conflicts with `--resume`.
pub trait ProductArgs<O> {
    /// The product's options for sessions in `workspace`, with what its file's loader
    /// read and refused, reported after the main configuration's.
    fn load(
        self,
        workspace: &Path,
    ) -> impl Future<Output = Result<ResolvedConfig<O>, LayerResolutionError>>;
}

/// Product options/subcommands for a binary that adds none.
#[derive(clap::Args)]
pub struct NoArgs {}
impl ProductArgs<()> for NoArgs {
    async fn load(self, _: &Path) -> Result<ResolvedConfig<()>, LayerResolutionError> {
        Ok(ResolvedConfig {
            config: (),
            report: Default::default(),
        })
    }
}
#[derive(clap::Subcommand)]
pub enum NoCommand {}

/// Parse a product option into a [`NamedEnum`], offering its spellings as the possible values.
pub fn named<T: NamedEnum + Send + Sync>() -> impl TypedValueParser<Value = T> {
    PossibleValuesParser::new(T::NAMES).map(|name| T::parse(&name).expect("a possible value"))
}

/// What the command line asked for: a product command `C`, a shared command, or a session
/// with the product's options as it gave them, `A`.
pub enum Invocation<C, A> {
    Product(C),
    Auth(AuthCommand),
    Dump(DumpRequest<A>),
    Stats(StatsRequest),
    Batch(BatchRequest<A>),
    Interactive(InteractiveRequest<A>),
}

/// The process's command line for `product`, whose session options are `A` and whose own
/// commands are `C`, described by `about`; help, version and syntax errors exit here.
///
/// `Product::identity` and clap's value parsers must not write stdout or start threads: the
/// invocation's `.env` is loaded after this.
pub fn parse<A, C, P>(product: &P, about: &'static str) -> Invocation<C, A>
where
    A: clap::Args,
    C: Subcommand,
    P: Product,
{
    parse_from::<A, C, P>(product, about, std::env::args_os()).unwrap_or_else(|error| error.exit())
}

fn parse_from<A, C, P>(
    product: &P,
    about: &'static str,
    args: impl IntoIterator<Item = impl Into<std::ffi::OsString> + Clone>,
) -> Result<Invocation<C, A>, clap::Error>
where
    A: clap::Args,
    C: Subcommand,
    P: Product,
{
    let identity = product.identity();
    let mut command = Args::<A, C>::command()
        .name(identity.name().to_owned())
        .about(about);
    if identity.root_mode == RootMode::Switchable {
        command = <Mode as clap::Args>::augment_args(command)
            .mut_subcommand("batch", <Permissions as clap::Args>::augment_args);
    }
    let matches = command.try_get_matches_from_mut(args)?;
    if let Some((_, sub)) = matches.subcommand() {
        admit_command(Command::from_arg_matches(&matches)?, sub)
    } else {
        let session = SessionArgs::<A>::from_arg_matches(&matches)?;
        let (config, execution, input, options) = session.into_request();
        Ok(Invocation::Interactive(InteractiveRequest {
            config,
            execution,
            mode: selected(&matches, "mode"),
            input,
            options,
        }))
    }
}

/// What the command line gave for `id`, an option of [`Mode`] or [`Permissions`].
fn selected<T: Clone + Send + Sync + 'static>(matches: &ArgMatches, id: &str) -> Option<T> {
    matches.try_get_one(id).ok().flatten().cloned()
}

fn conflict(message: &str) -> clap::Error {
    clap::Error::raw(clap::error::ErrorKind::ArgumentConflict, message)
}

/// Whether `matches` hold a product option the command line gave, which clap
/// cannot tie to a subcommand's own value for a generic product.
fn given<A: clap::Args>(matches: &ArgMatches) -> bool {
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

/// `matches` are the command's own. Only the commands that start or inspect a session
/// take product session options.
fn admit_command<A: clap::Args, C: Subcommand>(
    command: Command<A, C>,
    matches: &ArgMatches,
) -> Result<Invocation<C, A>, clap::Error> {
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
            if config.source.config.is_some() || config.approve_all || given::<A>(matches) {
                return Err(conflict(
                    "dump skills uses skill discovery, not --config, --approve-all or session options",
                ));
            }
            Ok(Inv::Dump(DumpRequest::Skills(config.source.workspace)))
        }
        Command::Dump {
            what: DumpKind::Config,
            config,
            product,
        } => Ok(Inv::Dump(DumpRequest::Config(config, product))),
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
        Command::Batch { session } => {
            let (config, execution, input, options) = session.into_request();
            let mode = selected::<ModeName>(matches, "mode");
            if execution.resume.is_some()
                && mode.is_some()
                && matches!(input, Some(InitialInput::Script(_)))
            {
                return Err(conflict(
                    "--mode changes a resumed session with its next prompt; a script has none",
                ));
            }
            let permissions = match selected(matches, "capabilities") {
                Some(Capabilities(capabilities)) => PermissionRequest::Exact(capabilities),
                None => PermissionRequest::Mode(mode),
            };
            let input = input.ok_or_else(|| {
                clap::Error::raw(
                    clap::error::ErrorKind::MissingRequiredArgument,
                    "batch requires --prompt or --script\n",
                )
            })?;
            Ok(Inv::Batch(BatchRequest {
                config,
                execution,
                permissions,
                input,
                options,
            }))
        }
    }
}

impl<A: clap::Args> SessionArgs<A> {
    fn into_request(self) -> (ConfigArgs, ExecutionRequest, Option<InitialInput>, A) {
        // Clap rejects both together: `--prompt` is `conflicts_with = "script"`.
        let input = match self.prompt {
            Some(text) => Some(InitialInput::Prompt {
                text,
                images: self.images,
            }),
            None => self.script.map(InitialInput::Script),
        };
        let request = ExecutionRequest {
            resume: self.resume,
            model: self.model,
        };
        (self.config, request, input, self.product)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const ID: &str = "00000000000000000000000000000001";

    /// Skyhook's invocation for `args`.
    pub(crate) fn invocation<T: Into<std::ffi::OsString> + Clone>(
        args: impl IntoIterator<Item = T>,
    ) -> Result<Invocation<NoCommand, NoArgs>, clap::Error> {
        parse_from::<NoArgs, NoCommand, _>(&skyhook::product::Skyhook, "", args)
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
        let AuthCommand::Status { target, .. } = status else {
            panic!("status invocation");
        };
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
            (&["--mode", "readonly", "-p", "hello"], true),
            (&["--mode", "readonly", "batch", "-p", "hello"], false),
            (&["batch", "--resume", ID, "--mode", "m", "-s", "x"], false),
            (&["batch", "--resume", ID, "--mode", "m", "-p", "x"], true),
            (
                &["batch", "--resume", ID, "--model", "p/m", "-s", "x"],
                true,
            ),
        ] {
            let parsed = invocation(std::iter::once(&"skyhook").chain(args));
            assert_eq!(parsed.is_ok(), valid, "{args:?}");
        }
        let permissions = |args: &[&str]| match invocation([&["skyhook"], args].concat()) {
            Ok(Invocation::Batch(request)) => request.permissions,
            _ => panic!("batch invocation"),
        };
        assert!(matches!(
            permissions(&["batch", "-p", "x", "--capabilities="]),
            PermissionRequest::Exact(capabilities) if capabilities.is_empty()
        ));
        assert!(matches!(
            permissions(&["batch", "-p", "x", "--capabilities", "read,exec,targets"]),
            PermissionRequest::Exact(capabilities)
                if capabilities == [Capability::Read, Capability::Exec, Capability::Targets]
        ));
        assert!(matches!(
            permissions(&["batch", "-p", "x", "--mode", "m"]),
            PermissionRequest::Mode(Some(mode)) if mode.as_str() == "m"
        ));
        assert!(matches!(
            permissions(&["batch", "-s", "x"]),
            PermissionRequest::Mode(None)
        ));
        assert!(matches!(
            invocation(["skyhook", "--mode", "m"]),
            Ok(Invocation::Interactive(request)) if request.mode.as_ref().is_some_and(|mode| mode.as_str() == "m")
        ));
        // A model is named where it is given: an unqualified name says how.
        assert!(matches!(
            invocation(["skyhook", "-m", "local/first"]),
            Ok(Invocation::Interactive(request))
                if request.execution.model.as_ref().is_some_and(|model| model.to_string() == "local/first")
        ));
        let Err(unqualified) = invocation(["skyhook", "batch", "-p", "x", "-m", "first"]) else {
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
        assert!(invocation(["skyhook", "dump", "--capabilities", "read"]).is_err());
        assert!(parse(&["dump", "unknown"]).is_none());
        assert!(
            parse(&["-c", "c.yaml", "-a", "-p", "x"])
                .unwrap()
                .session
                .config
                .approve_all
        );
        assert!(invocation(["skyhook", "dump", "skills", "--config", "other.yaml"]).is_err());
        let stats = |args: &[&str]| match invocation([&["skyhook", "stats"], args].concat()) {
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
            invocation(["skyhook", "auth", "status", "codex"]).unwrap(),
            Invocation::Auth(AuthCommand::Status { check: false, .. })
        ));
        assert!(invocation(["skyhook", "--approve-all", "auth", "status", "codex"]).is_err());
        assert!(matches!(
            invocation(["skyhook", "dump"]).unwrap(),
            Invocation::Dump(DumpRequest::Config(_, _))
        ));
        assert!(matches!(
            invocation(["skyhook", "dump", "skills"]).unwrap(),
            Invocation::Dump(DumpRequest::Skills(_))
        ));
        assert!(matches!(
            invocation(["skyhook"]).unwrap(),
            Invocation::Interactive(InteractiveRequest { input: None, .. })
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
                let input = match invocation(args).unwrap() {
                    Invocation::Batch(request) if headless => request.input,
                    Invocation::Interactive(request) if !headless => request.input.unwrap(),
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
        type Options = ResearchArgs;
        fn identity(&self) -> skyhook::product::Identity {
            skyhook::product::Identity {
                host: "research".parse().unwrap(),
                ..skyhook::product::Skyhook.identity()
            }
        }
    }
    /// The `Research` invocation for `args`.
    fn research(args: &[&str]) -> Result<Invocation<ResearchCommand, ResearchArgs>, clap::Error> {
        parse_from::<ResearchArgs, ResearchCommand, _>(&Research, "", args)
    }

    /// Product options join the session grammar at the root, under `batch` and for the
    /// config dump; shared commands and the product's own take only their own arguments.
    #[test]
    fn products_compose_session_args_and_commands_without_changing_shared_grammar() {
        let product = ["--run", "first", "-m", "provider/model", "--prompt", "go"];
        match research(&[&["research"], &product[..]].concat()).unwrap() {
            Invocation::Interactive(InteractiveRequest {
                execution,
                input: Some(InitialInput::Prompt { text, .. }),
                options,
                ..
            }) => {
                assert_eq!(execution.model.unwrap().to_string(), "provider/model");
                assert_eq!(options.run.as_deref(), Some("first"));
                assert_eq!(text, "go");
            }
            _ => panic!("interactive invocation"),
        }
        match research(&[&["research", "batch"], &product[..]].concat()).unwrap() {
            Invocation::Batch(request) => assert_eq!(request.options.run.as_deref(), Some("first")),
            _ => panic!("batch invocation"),
        }
        assert!(matches!(
            research(&["research", "dump", "--run", "first"]).unwrap(),
            Invocation::Dump(DumpRequest::Config(_, args)) if args.run.as_deref() == Some("first")
        ));
        assert!(matches!(
            research(&["research", "runs"]).unwrap(),
            Invocation::Product(ResearchCommand::Runs)
        ));
        assert!(research(&["research", "--run", "first", "runs"]).is_err());
        assert!(research(&["research", "stats", "--list", "--run", "first"]).is_err());
    }
}
