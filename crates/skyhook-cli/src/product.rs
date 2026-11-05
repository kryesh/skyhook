//! The typed command-line boundary and the per-session host extension boundary.
use super::form::{ScopeForm, ScopeReply};
use super::page::{Disclosed, Page, ViewFrame};
use super::tui::Theme;
use skyhook::{
    agent::{HarnessBuilder, SessionHandle},
    config::{Config, ConfigReport},
    identity::{HostName, SessionId},
    session::Schema,
    tool::policy::HostCapabilityDeclaration,
};
use std::{future::Future, path::Path, pin::Pin, sync::Arc};

/// A product failure, displayed by the shared host with terminal controls escaped.
pub type Error = Box<dyn std::error::Error + Send + Sync>;
/// Object-safe asynchronous session work.
pub type HookFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

/// Branding and UI preferences. Session storage remains `.skyhook/sessions` for every product.
#[derive(Clone, Debug)]
pub struct Identity {
    /// Tags the sessions this product creates; other hosts' sessions open view-only.
    pub host: HostName,
    /// The schema of every session this product creates, with any tables of its own.
    /// One of its sessions without them at their format is neither listed nor resumed.
    pub schema: Schema,
    pub display_name: &'static str,
    pub about: &'static str,
    /// Shown in an empty conversation before the first message.
    pub placeholder: &'static str,
    /// Path relative to the workspace's `.skyhook` directory, independent of session storage.
    pub state_file: &'static str,
    pub root_mode: RootMode,
    /// UI colours; the default syntax palette keeps code highlighting shared.
    pub theme: Theme,
    /// The host capabilities this product's modes grant, declared on every session it
    /// opens, another host's included.
    pub capabilities: Vec<HostCapabilityDeclaration>,
}

impl Identity {
    /// The command name, as help and diagnostics print it.
    pub fn name(&self) -> &str {
        self.host.as_str()
    }

    /// The host and schema a session this product created must have to be its own.
    pub fn owner(&self) -> (&HostName, Schema) {
        (&self.host, self.schema)
    }
}

/// What a session is opened as. Only a new session carries scope answers.
#[derive(Clone, Copy, Debug)]
pub enum Opening<'a> {
    /// A new session, with the scope form's answers when the terminal sent them.
    New(Option<&'a ScopeReply>),
    Resume(SessionId),
}

/// Context available before constructing a harness, including the identity of a resumed session.
#[derive(Clone, Copy, Debug)]
pub struct SessionContext<'a> {
    pub workspace: &'a Path,
    pub opening: Opening<'a>,
    pub headless: bool,
}

/// A product supplies syntax and resolves its configuration before runtime admission.
///
/// Configuration hooks and `command` run inside the async runtime. `identity`, `input`, and
/// Clap value parsers must not write stdout or start threads: the host loads the invocation's
/// `.env` after syntax admission.
pub trait Product: Send + Sync + 'static {
    type Args: clap::Args + Send + Sync + 'static;
    type Command: clap::Subcommand + Send + 'static;

    fn identity(&self) -> Identity;

    /// Edit resolved configuration before admission; `dump config` serializes the result and
    /// sessions run with it. Returns the product's own loader report, which the host renders,
    /// and the factory for actual session openings. Drafts are lazy; their factory is called
    /// only when first submitted. Must not create runs, connect MCP, or write stdout.
    fn configure(
        &self,
        _config: &mut Config,
        _args: Self::Args,
        _context: SessionContext<'_>,
    ) -> impl Future<Output = Result<Configured, Error>> + Send {
        async { Ok(Configured::default()) }
    }

    fn command(&self, command: Self::Command) -> impl Future<Output = Result<(), Error>> + Send;
}

/// What a product's configuration edit produced.
pub struct Configured {
    pub report: ConfigReport,
    pub factory: Arc<dyn SessionFactory>,
}

impl Default for Configured {
    fn default() -> Self {
        Self {
            report: ConfigReport::default(),
            factory: Arc::new(()),
        }
    }
}

/// The plain coding-agent harness: no product arguments, commands or session tables.
pub struct Skyhook;
impl Product for Skyhook {
    type Args = NoArgs;
    type Command = NoCommand;

    fn identity(&self) -> Identity {
        Identity {
            host: "skyhook".parse().expect("the host name is nonblank"),
            schema: Schema::JOURNAL,
            display_name: "Skyhook",
            about: "Programmable coding-agent harness",
            placeholder: "Start a conversation, or /sessions to open one.",
            state_file: "state.json",
            root_mode: RootMode::Switchable,
            theme: Theme::default(),
            capabilities: Vec::new(),
        }
    }

    async fn command(&self, command: NoCommand) -> Result<(), Error> {
        match command {}
    }
}

/// Product options/subcommands for a host that adds none.
#[derive(clap::Args)]
pub struct NoArgs {}
#[derive(clap::Subcommand)]
pub enum NoCommand {}

/// Unread command-line input. Paths are read only after a session has opened (and a batch
/// session's ID has been flushed to stdout).
pub enum InitialInput {
    Prompt {
        text: String,
        images: Vec<std::path::PathBuf>,
    },
    Script(std::path::PathBuf),
}

/// How a product's root agent takes its mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RootMode {
    /// The remembered mode, an explicit `--mode`, or the configured default; the
    /// terminal can switch between every configured mode.
    #[default]
    Switchable,
    /// Always the configured default: `--mode` and batch capabilities are refused and
    /// the terminal offers no mode switch. Children still take modes from the agent tool.
    Fixed,
}

/// Shared process configuration, preparing a separate extension for each session.
///
/// A resumed session's resources are its own: tables the product keeps in its sessions
/// (see [`Identity::schema`]) reach tools through their context and the extension through
/// the session it is handed. The host prepares only this product's own sessions; another
/// host's session opens view-only with no extension.
pub trait SessionFactory: Send + Sync + 'static {
    /// The scope a new session's draft is written with: shown above the conversation
    /// and sent with the first message, whose text the form supplies when the draft is
    /// empty. The answers reach `prepare` with the new opening. Built from what the
    /// factory already holds; nothing is read here.
    fn form(&self, _context: SessionContext<'_>) -> Option<ScopeForm> {
        None
    }

    fn prepare<'a>(
        &'a self,
        builder: HarnessBuilder,
        _context: SessionContext<'a>,
    ) -> HookFuture<'a, PreparedSession> {
        Box::pin(async {
            Ok(PreparedSession {
                builder,
                extension: Arc::new(()),
            })
        })
    }
}
impl SessionFactory for () {}

/// A customized builder and resources belonging to this session, not the process's current run.
pub struct PreparedSession {
    pub builder: HarnessBuilder,
    pub extension: Arc<dyn SessionExtension>,
}

/// Resources retained with an opened session, including its terminal views and actions.
/// Hooks always belong to the session that supplied them, even after the user switches sessions.
pub trait SessionExtension: Send + Sync + 'static {
    /// Stable read-only tabs. The host owns cycling, headers, and refresh scheduling.
    fn views(&self) -> &[ExtensionView] {
        &[]
    }

    /// Read `views()[view]` off the UI thread, as `session`, the session that supplied
    /// this extension, sees it, supplying records and detail only for what `open` shows.
    /// The host lays the page out with its own theme, reflow and scrolling. The host runs
    /// one refresh per session at a time and bounds it; the future must be
    /// cancellation-safe and must not spawn detached work or block an executor. Return
    /// the same `Arc` for an unchanged page: the host lays a page out again only when it
    /// receives another one.
    fn refresh(
        &self,
        _view: usize,
        _session: SessionHandle,
        _open: Disclosed,
    ) -> HookFuture<'_, Arc<Page>> {
        Box::pin(async { Err("view is not available".into()) })
    }

    /// Called before first input, after the batch ID is flushed. Set up what a new
    /// session needs here, and recover what an interrupted setup left undone; a reopened
    /// session may only be viewed, so change nothing else: what this launch changes about
    /// its work belongs in a [`skyhook::agent::ResumeHook`] on the prepared builder. On failure the host shuts the
    /// session down.
    fn opened<'a>(
        &'a self,
        _session: &'a SessionHandle,
        _context: SessionContext<'a>,
    ) -> HookFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    /// The scope `session` runs under in this launch, as the factory's form with those
    /// values, what this launch changes once the session's work resumes included: shown
    /// frozen above the conversation of a session the terminal opens that it did not
    /// start from a draft. Read once, after [`Self::opened`] and before any work resumes.
    fn scope<'a>(&'a self, _session: &'a SessionHandle) -> HookFuture<'a, Option<ScopeForm>> {
        Box::pin(async { Ok(None) })
    }
}
impl SessionExtension for () {}

/// A read-only tab supplied by the session's extension.
#[derive(Clone, Copy, Debug)]
pub struct ExtensionView {
    pub title: &'static str,
    pub frame: ViewFrame,
}
