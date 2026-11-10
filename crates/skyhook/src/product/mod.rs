//! What a product supplies every host: its identity, its configuration edit, and the
//! extension each of its sessions carries, with the pages and forms that extension shows.
pub mod form;
pub mod page;
pub mod text;

use crate::identity::{HostName, SessionId};
use crate::tool::policy::HostCapabilityDeclaration;
use crate::{
    agent::{HarnessBuilder, SessionHandle},
    config::Config,
    session::Schema,
};
use form::{ScopeForm, ScopeReply};
use page::{Disclosed, Page};
use std::{future::Future, path::Path, pin::Pin, sync::Arc};

/// A product failure, displayed by the host with terminal controls escaped.
pub type Error = Box<dyn std::error::Error + Send + Sync>;
/// Object-safe asynchronous session work.
pub type HookFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

/// What a product is to every host.
#[derive(Clone, Debug)]
pub struct Identity {
    /// Tags the sessions this product creates; other hosts' sessions open view-only.
    pub host: HostName,
    /// The schema of every session this product creates, with any tables of its own.
    /// One of its sessions without them at their format is neither listed nor resumed.
    pub schema: Schema,
    pub display_name: &'static str,
    pub root_mode: RootMode,
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
    /// A new session, with the scope form's answers when the host sent them; a launch
    /// refuses answers its factory's form does not offer before the factory sees them.
    New(Option<&'a ScopeReply>),
    Resume(SessionId),
}

/// Context available before constructing a harness, including the identity of a resumed session.
#[derive(Clone, Copy, Debug)]
pub struct SessionContext<'a> {
    pub workspace: &'a Path,
    pub opening: Opening<'a>,
    pub attendance: Attendance,
}

/// Whether a person attends the session, to answer its approvals and
/// questions, or it runs as a batch job with nobody to ask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attendance {
    Attended,
    Batch,
}

/// A product resolves its configuration before runtime admission.
///
/// `configure` runs inside the async runtime; `identity` must not write stdout or start
/// threads, as a host may still be starting up when it asks.
pub trait Product: Send + Sync + 'static {
    /// What a launch of the product takes beside the shared configuration: its own options
    /// and its own parsed configuration, which each host reads in its own way.
    type Options: Send + 'static;

    fn identity(&self) -> Identity;

    /// Edit resolved configuration before admission; a host's configuration dump shows the
    /// result and sessions run with it. Returns what the product warns of the configuration
    /// it was handed, which the host shows, and the factory for actual session openings.
    /// Drafts are lazy; their factory is called only when first submitted. Must not read
    /// configuration files, create runs, connect MCP, or write stdout.
    fn configure(
        &self,
        _config: &mut Config,
        _options: Self::Options,
        _context: SessionContext<'_>,
    ) -> impl Future<Output = Result<Configured, Error>> + Send {
        async { Ok(Configured::default()) }
    }
}

/// What a product's configuration edit produced.
pub struct Configured {
    /// What the product warns of its configuration, complete as the host shows it.
    pub warnings: Vec<String>,
    pub factory: Arc<dyn SessionFactory>,
}

impl Default for Configured {
    fn default() -> Self {
        Self {
            warnings: Vec::new(),
            factory: Arc::new(()),
        }
    }
}

/// The plain coding-agent harness: no product options or session tables.
pub struct Skyhook;
impl Product for Skyhook {
    type Options = ();

    fn identity(&self) -> Identity {
        Identity {
            host: "skyhook".parse().expect("the host name is nonblank"),
            schema: Schema::JOURNAL,
            display_name: "Skyhook",
            root_mode: RootMode::Switchable,
            capabilities: Vec::new(),
        }
    }
}

/// How a product's root agent takes its mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RootMode {
    /// The remembered mode, an explicit `--mode`, or the configured default; the
    /// host can switch between every configured mode.
    #[default]
    Switchable,
    /// Always the configured default: the host offers no mode switch. Children still
    /// take modes from the agent tool.
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

/// Resources retained with an opened session, including its views and actions.
/// Hooks always belong to the session that supplied them, even after the user switches sessions.
pub trait SessionExtension: Send + Sync + 'static {
    /// The titles of stable read-only tabs, each a page beside the session's live agents.
    /// The host owns cycling, headers, and refresh scheduling.
    fn views(&self) -> &[&'static str] {
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
    /// its work belongs in a [`crate::agent::ResumeHook`] on the prepared builder. On failure the host shuts the
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
    /// frozen above the conversation of a session the host opens that it did not
    /// start from a draft. Read once, after [`Self::opened`] and before any work resumes.
    fn scope<'a>(&'a self, _session: &'a SessionHandle) -> HookFuture<'a, Option<ScopeForm>> {
        Box::pin(async { Ok(None) })
    }
}
impl SessionExtension for () {}
