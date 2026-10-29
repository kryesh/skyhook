use std::{path::Path, sync::Arc};

use skyhook::{
    agent::{HarnessBuilder, HostState, HostStateFuture, ResumeFuture, ResumeHook, SessionHandle},
    identity::{AgentId, SessionId},
    session::{ExtensionDb, SessionStore},
};

use skyhook_cli::{
    HookFuture, Opening, PreparedSession, SessionContext, SessionExtension, SessionFactory,
    form::{Choice, Field, FieldKind, ScopeForm, ScopeReply},
    page::Disclosed,
};

use crate::{
    cvss::Severity,
    presets, research_view,
    store::{OpenTest, PocPolicy, Revision, RunFacts, RunOptions, RunState, Store, Tree},
    tools,
};

/// What a session starts with: configuration and flags first, then the scope form. A new run
/// takes its options; the launch works the run under its PoC policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scope {
    pub options: RunOptions,
    pub poc: PocPolicy,
}

/// The scope form's pages, in order: a new session is a new run.
#[derive(Clone, Copy)]
enum ScopeField {
    Shortlist,
    Poc,
    Workers,
}

impl ScopeField {
    const ALL: [Self; 3] = [Self::Shortlist, Self::Poc, Self::Workers];

    fn field(self, scope: &Scope) -> Field {
        let choice = |labels: &[&str], selected: usize| {
            let options = labels.iter().map(|label| (*label).to_owned()).collect();
            FieldKind::Choice(Choice::new(options, selected).expect("the selection is listed"))
        };
        let integer = |value: std::num::NonZeroU16| FieldKind::Integer {
            value: value.get(),
            range: 1..=u16::MAX,
        };
        match self {
            Self::Shortlist => Field {
                name: "Shortlist".into(),
                prompt: "The lowest conditional severity that sends a candidate to review; lower-rated candidates stay recorded".into(),
                kind: choice(
                    &Severity::ALL.map(|severity| severity.as_str()),
                    Severity::ALL
                        .iter()
                        .position(|severity| *severity == scope.options.threshold)
                        .expect("severity is listed"),
                ),
            },
            Self::Poc => Field {
                name: "Live PoC".into(),
                prompt: "Whether validated findings are demonstrated against an authorized environment".into(),
                kind: choice(
                    &PocPolicy::ALL.map(|policy| policy.as_str()),
                    PocPolicy::ALL
                        .iter()
                        .position(|policy| *policy == scope.poc)
                        .expect("policy is listed"),
                ),
            },
            Self::Workers => Field {
                name: "Workers".into(),
                prompt: "How many agents the orchestrator's scheduler runs at once".into(),
                kind: integer(scope.options.workers),
            },
        }
    }
}

impl Scope {
    fn form(&self, title: String, fields: &[ScopeField], message: String) -> ScopeForm {
        ScopeForm {
            title,
            fields: fields.iter().map(|field| field.field(self)).collect(),
            message,
        }
    }

    /// The scope with a form's answers applied, field by field in form order.
    fn answered(mut self, fields: &[ScopeField], reply: &ScopeReply) -> Self {
        for (field, value) in fields.iter().zip(reply) {
            match field {
                ScopeField::Shortlist => {
                    if let Some(index) = value.choice() {
                        self.options.threshold = Severity::ALL[index];
                    }
                }
                ScopeField::Poc => {
                    if let Some(index) = value.choice() {
                        self.poc = PocPolicy::ALL[index];
                    }
                }
                ScopeField::Workers => {
                    if let Some(workers) = value.integer().and_then(std::num::NonZeroU16::new) {
                        self.options.workers = workers;
                    }
                }
            }
        }
        self
    }
}

#[derive(Clone)]
pub struct ResearchFactory {
    scope: Scope,
    /// The configured modes, rendered for each session's PoC policy.
    modes: indexmap::IndexMap<skyhook::tool::policy::ModeName, skyhook::tool::policy::Mode>,
    /// A worker count the command line set, which resizes a resumed run too.
    resize: Option<std::num::NonZeroU16>,
}

impl ResearchFactory {
    /// A factory whose new sessions start from `scope`; a resumed session keeps its run's
    /// threshold, and its workers unless `resize` sets them.
    pub fn new(
        scope: Scope,
        resize: Option<std::num::NonZeroU16>,
        modes: indexmap::IndexMap<skyhook::tool::policy::ModeName, skyhook::tool::policy::Mode>,
    ) -> Self {
        Self {
            scope,
            modes,
            resize,
        }
    }
}

impl SessionFactory for ResearchFactory {
    fn form(&self, context: SessionContext<'_>) -> Option<ScopeForm> {
        if let Opening::Resume(_) = context.opening {
            return None;
        }
        Some(self.scope.form(
            "Scope".to_owned(),
            &ScopeField::ALL,
            "Analyze the workspace.".to_owned(),
        ))
    }

    fn prepare<'a>(
        &'a self,
        builder: HarnessBuilder,
        context: SessionContext<'a>,
    ) -> HookFuture<'a, PreparedSession> {
        Box::pin(async move {
            let builder = builder.persona(
                include_str!("prompts/persona.md")
                    .parse()
                    .expect("embedded persona"),
            );
            // A new session's form answers, when the terminal sent them, over the defaults.
            let scope = match context.opening {
                Opening::New(Some(reply)) => self.scope.answered(&ScopeField::ALL, reply),
                Opening::New(None) | Opening::Resume(_) => self.scope,
            };
            let research = Arc::new(ResearchSession {
                scope,
                resize: self.resize,
                workspace: context.workspace.to_owned(),
                current: std::sync::Mutex::new(revision(context.workspace).await),
                facts: Cached::default(),
                state: Cached::default(),
                page: Cached::default(),
            });
            let builder = builder
                .modes(presets::for_policy(self.modes.clone(), scope.poc))
                .host_state(research.clone())
                .resume_hook(research.clone())
                .tools(tools::registry()?);
            Ok(PreparedSession {
                builder,
                extension: research,
            })
        })
    }
}

/// A security session's resources: the run lives in the session itself, so each hook
/// reads it from the session it is handed.
pub struct ResearchSession {
    /// A new run's threshold and workers, and the PoC policy this launch works the run under.
    scope: Scope,
    resize: Option<std::num::NonZeroU16>,
    workspace: std::path::PathBuf,
    /// The checkout as last read, as this launch's work began; drift is measured against the
    /// run's.
    current: std::sync::Mutex<Revision>,
    /// The run's facts the orchestrator's host state last read, reused until research
    /// changes.
    facts: Cached<Generation, RunFacts>,
    /// The run state the Research page last read, reused until research changes.
    state: Cached<Generation, RunState>,
    /// The last Research page, with what it was read at and for: an unchanged store and
    /// disclosure read nothing.
    page: Cached<PageKey, skyhook_cli::page::Page>,
}

/// The last value read, with the key it was read under.
struct Cached<K, T>(std::sync::Mutex<Option<(K, Arc<T>)>>);

impl<K, T> Default for Cached<K, T> {
    fn default() -> Self {
        Self(std::sync::Mutex::default())
    }
}

impl<K: PartialEq, T> Cached<K, T> {
    /// The value read under `key`, read again by `read` once the key moved on.
    async fn get(
        &self,
        key: K,
        read: impl Future<Output = Result<T, skyhook_cli::Error>>,
    ) -> Result<Arc<T>, skyhook_cli::Error> {
        if let Some((cached, value)) = &*self.0.lock().expect("research cache")
            && *cached == key
        {
            return Ok(Arc::clone(value));
        }
        let value = Arc::new(read.await?);
        *self.0.lock().expect("research cache") = Some((key, Arc::clone(&value)));
        Ok(value)
    }
}

/// One session's research at the write generation it was read at.
type Generation = (SessionId, Option<u64>);

/// What a Research page was read at and for: the open tests are read on every refresh,
/// since a job ending is no research write.
type PageKey = (Generation, Disclosed, Vec<OpenTest>);

/// The research of a session opened by this product's harness.
fn research(session: &SessionStore) -> Result<Store, skyhook_cli::Error> {
    session
        .extension()
        .map(Store::of)
        .ok_or_else(|| "this session keeps no security research".into())
}

/// Only the orchestrator steers the run; the agents it starts get what their work needs in
/// their briefs.
impl HostState for ResearchSession {
    fn state<'a>(&'a self, extension: &'a ExtensionDb, agent: &'a AgentId) -> HostStateFuture<'a> {
        Box::pin(async move {
            if !agent.path().is_empty() {
                return None;
            }
            let text = match self.root_state(Store::of(extension)).await {
                Ok(text) => text,
                Err(error) => format!("research state unavailable: {error}"),
            };
            text.parse().ok()
        })
    }
}

/// What this launch changes about a run it reopens, applied once its work begins.
impl ResumeHook for ResearchSession {
    fn resumed<'a>(&'a self, extension: &'a ExtensionDb) -> ResumeFuture<'a> {
        Box::pin(async move {
            // The resumed run keeps its workers unless this launch's flag resizes them, and
            // continues under this launch's PoC policy.
            self.checkout().await;
            let store = Store::of(extension);
            let (resize, poc) = (self.resize, self.scope.poc);
            store
                .clone()
                .writing(move || {
                    if let Some(workers) = resize {
                        store.set_workers(workers)?;
                    }
                    store.set_poc_policy(poc).map_err(skyhook_cli::Error::from)
                })
                .await
        })
    }
}

impl SessionExtension for ResearchSession {
    fn views(&self) -> &[skyhook_cli::ExtensionView] {
        research_view::VIEWS
    }

    fn refresh(
        &self,
        _view: usize,
        session: SessionHandle,
        open: Disclosed,
    ) -> HookFuture<'_, Arc<skyhook_cli::page::Page>> {
        Box::pin(async move {
            let store = research(session.store())?;
            // Read before the snapshot begins, so a page cached under it is never older.
            let generation = store.generation();
            let reader = store.clone();
            let tests = store
                .reading(move || reader.open_tests().map_err(skyhook_cli::Error::from))
                .await?;
            let key = ((store.id(), generation), open.clone(), tests.clone());
            let read = async move {
                let state = self.snapshot(session.store()).await?;
                let (reader, current) = (store.clone(), self.current());
                store
                    .reading(move || {
                        (state.page(&reader, &current, &open, &tests))
                            .map_err(skyhook_cli::Error::from)
                    })
                    .await
            };
            self.page.get(key, read).await
        })
    }

    fn opened<'a>(
        &'a self,
        session: &'a SessionHandle,
        context: SessionContext<'a>,
    ) -> HookFuture<'a, ()> {
        Box::pin(async move {
            let store = research(session.store())?;
            let started = store.clone();
            let reader = store.clone();
            if store
                .reading(move || reader.initialized().map_err(skyhook_cli::Error::from))
                .await?
            {
                return Ok(());
            }
            if let Opening::Resume(_) = context.opening {
                session
                    .record_status(
                        session.root_agent().clone(),
                        "The run's setup was interrupted; it starts now under this launch's settings.".to_owned(),
                    )
                    .await?;
            }
            let scope = self.scope;
            let current = self.current();
            store
                .writing(move || {
                    started.start_run(scope.options, &current, scope.poc)?;
                    Ok::<_, skyhook_cli::Error>(())
                })
                .await?;
            Ok(())
        })
    }
}

impl ResearchSession {
    /// Research facts can be cached; whether the workflow needs its owner cannot, since
    /// the journal records job transitions independently of research writes.
    async fn root_state(&self, store: Store) -> Result<String, skyhook_cli::Error> {
        let facts = cached(&self.facts, store.clone(), Store::run_facts).await?;
        let reader = store.clone();
        let waiting = store
            .reading(move || reader.waiting_workflows().map_err(skyhook_cli::Error::from))
            .await?;
        Ok(facts.state_with(&self.current(), &waiting))
    }

    async fn snapshot(&self, session: &SessionStore) -> Result<Arc<RunState>, skyhook_cli::Error> {
        cached(&self.state, research(session)?, Store::run_state).await
    }

    /// Read the checkout again as this launch's work begins.
    async fn checkout(&self) -> Revision {
        let current = revision(&self.workspace).await;
        current.clone_into(&mut self.current.lock().expect("checkout"));
        current
    }

    /// The checkout as last read.
    fn current(&self) -> Revision {
        self.current.lock().expect("checkout").clone()
    }
}

/// What `read` gives for the session's research, reused while no research write followed.
async fn cached<T: Send + Sync + 'static>(
    cache: &Cached<Generation, T>,
    store: Store,
    read: fn(&Store) -> crate::store::Result<T>,
) -> Result<Arc<T>, skyhook_cli::Error> {
    // Read before the snapshot begins, so a value cached under it is never older.
    let key = (store.id(), store.generation());
    let reader = store.clone();
    let read = store.reading(move || read(&reader).map_err(skyhook_cli::Error::from));
    cache.get(key, read).await
}

pub fn drift_warning(recorded: &Revision, current: &Revision) -> String {
    format!(
        "Warning: revision drift: the run recorded {recorded}, the checkout is now {current}. Recheck affected evidence; a tree state is not a content snapshot."
    )
}

pub(crate) async fn revision(workspace: &Path) -> Revision {
    async fn git(workspace: &Path, args: &[&str]) -> Option<Vec<u8>> {
        let mut command = tokio::process::Command::new("git");
        command
            .arg("-C")
            .arg(workspace)
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(std::time::Duration::from_secs(5), command.output())
            .await
            .ok()?
            .ok()?;
        output.status.success().then_some(output.stdout)
    }
    let commit = git(workspace, &["rev-parse", "--verify", "HEAD"])
        .await
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| text.trim().parse().ok());
    let tree = match git(
        workspace,
        &["status", "--porcelain", "--untracked-files=normal"],
    )
    .await
    {
        Some(output) if output.is_empty() => Tree::Clean,
        Some(_) => Tree::Dirty,
        None => Tree::Unknown,
    };
    Revision { commit, tree }
}

#[cfg(test)]
mod tests {
    use skyhook::provider::{
        Provider, ProviderContext, ProviderError, ProviderErrorKind, ResponseStream,
        protocol::{ContextId, ModelRequest, ToolDefinition},
    };

    use super::*;
    use crate::store::tests::{harness as builder, harness_on};

    fn context<'a>(workspace: &'a Path, opening: Opening<'a>) -> SessionContext<'a> {
        SessionContext {
            workspace,
            opening,
            headless: false,
        }
    }

    fn scope(poc: PocPolicy) -> Scope {
        Scope {
            options: RunOptions::default(),
            poc,
        }
    }

    fn factory(poc: PocPolicy, resize: Option<std::num::NonZeroU16>) -> ResearchFactory {
        ResearchFactory::new(scope(poc), resize, presets::modes())
    }

    async fn open(
        factory: &ResearchFactory,
        workspace: &Path,
        opening: Opening<'_>,
    ) -> (SessionHandle, Arc<dyn SessionExtension>) {
        let context = context(workspace, opening);
        let prepared = factory.prepare(builder(workspace), context).await.unwrap();
        let harness = prepared.builder.build().await.unwrap();
        let session = match opening {
            Opening::New(_) => harness.new_session().await.unwrap(),
            Opening::Resume(id) => harness.resume_session(id).await.unwrap(),
        };
        prepared.extension.opened(&session, context).await.unwrap();
        (session, prepared.extension)
    }

    /// A provider that records each request's tools and refuses the request.
    #[derive(Clone, Default)]
    struct Recording(Arc<std::sync::Mutex<Vec<Vec<ToolDefinition>>>>);
    impl Provider for Recording {
        fn open_context(&self, _: ContextId) -> Result<Box<dyn ProviderContext>, ProviderError> {
            Ok(Box::new(self.clone()))
        }
    }
    impl ProviderContext for Recording {
        fn invoke(&mut self, request: ModelRequest) -> ResponseStream {
            self.0.lock().unwrap().push(request.tools);
            let refused = ProviderErrorKind::Authentication.error("recorded");
            Box::pin(futures_util::stream::iter([Err(refused)]))
        }
    }

    /// A root launched as the terminal launches it, from the product's configuration with
    /// the ceiling of every mode it can switch to, schedules the run and writes its reports
    /// out, from its own tool surface and from a script it runs, new and reopened.
    #[tokio::test]
    async fn the_launched_root_coordinates_the_run() {
        use skyhook::{config::Config, tool::policy::Capability};
        use skyhook_cli::Product;
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path();
        let explicit = workspace.join("security.yaml");
        std::fs::write(&explicit, "{}").unwrap();
        let mut config = Config::from_yaml("{}").unwrap();
        let args = crate::product::Args {
            poc: None,
            shortlist: None,
            workers: None,
            security_config: Some(explicit),
        };
        let new = context(workspace, Opening::New(None));
        let configured = crate::product::Security.configure(&mut config, args, new);
        let factory = configured.await.unwrap().factory;
        let mut ceiling = config.ceiling();
        ceiling.insert(Capability::Interactive);
        let requests = Recording::default();
        let mut reopened = None;
        for _ in 0..2 {
            let opening = reopened.map_or(Opening::New(None), Opening::Resume);
            let context = context(workspace, opening);
            let builder = harness_on(workspace, Arc::new(requests.clone()))
                .modes(config.modes.clone())
                .mode(config.default_mode.clone())
                .capabilities(ceiling.clone());
            let prepared = factory.prepare(builder, context).await.unwrap();
            let harness = prepared.builder.build().await.unwrap();
            let session = match reopened {
                None => harness.new_session().await.unwrap(),
                Some(id) => harness.resume_session(id).await.unwrap(),
            };
            prepared.extension.opened(&session, context).await.unwrap();
            assert!(session.prompt("start").await.is_err());
            let offered = |tool: &str, action: &str| {
                let requests = requests.0.lock().unwrap();
                let tools = requests.last().unwrap();
                let tool = tools.iter().find(|offered| offered.name == tool).unwrap();
                let actions = tool.input_schema["properties"]["action"]["enum"].as_array();
                actions.unwrap().iter().any(|offered| offered == action)
            };
            assert!(offered("research_query", "startable"));
            assert!(offered("finding_report", "generate"));
            let script = session.run_script(
                "return [await tool.research_query({action: 'startable', limit: 1}), await tool.finding_report({action: 'generate'})].map(r => r.error);",
            );
            assert_eq!(
                script.await.unwrap().value["value"],
                serde_json::json!([null, null])
            );
            reopened = Some(session.id());
            session.shutdown().await.unwrap();
            session.close().await.unwrap();
        }
    }

    /// Only the root's state carries the run; a child's is a plain agent's.
    #[tokio::test]
    async fn only_the_root_gets_the_research_state() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path();
        let (session, _) = open(&factory(PocPolicy::No, None), workspace, Opening::New(None)).await;
        let host = ResearchSession {
            scope: scope(PocPolicy::No),
            resize: None,
            workspace: workspace.to_owned(),
            current: std::sync::Mutex::new(revision(workspace).await),
            facts: Cached::default(),
            state: Cached::default(),
            page: Cached::default(),
        };
        let root = session.root_agent();
        let extension = session.store().extension().unwrap();
        let state = host.state(extension, root).await.unwrap();
        assert!(
            state
                .as_str()
                .starts_with(&format!("run: {}\n", session.id()))
        );
        assert!(host.state(extension, &root.child(1)).await.is_none());
        session.shutdown().await.unwrap();
        session.close().await.unwrap();
    }

    /// Viewing a reopened session changes nothing; this launch's settings apply once its
    /// work resumes. A session whose setup an interruption left undone is set up on open.
    #[tokio::test]
    async fn a_reopened_run_changes_only_when_its_work_resumes() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path();
        let (session, _) = open(&factory(PocPolicy::No, None), workspace, Opening::New(None)).await;
        let id = session.id();
        session.shutdown().await.unwrap();
        session.close().await.unwrap();

        let asking = factory(PocPolicy::Ask, std::num::NonZeroU16::new(4));
        let (viewed, _) = open(&asking, workspace, Opening::Resume(id)).await;
        let store = research(viewed.store()).unwrap();
        let settings = |store: &Store| {
            let run = store.run().unwrap();
            (run.poc, run.options.workers.get())
        };
        let configured = RunOptions::default().workers.get();
        assert_eq!(settings(&store), (PocPolicy::No, configured));
        viewed.resume_work().await.unwrap();
        assert_eq!(settings(&store), (PocPolicy::Ask, 4));
        viewed.shutdown().await.unwrap();
        viewed.close().await.unwrap();

        // A session created without its run is set up when it is next opened.
        let prepared = asking
            .prepare(builder(workspace), context(workspace, Opening::New(None)))
            .await
            .unwrap();
        let unset = prepared
            .builder
            .build()
            .await
            .unwrap()
            .new_session()
            .await
            .unwrap();
        let id = unset.id();
        unset.shutdown().await.unwrap();
        unset.close().await.unwrap();
        let (recovered, _) = open(&asking, workspace, Opening::Resume(id)).await;
        let store = research(recovered.store()).unwrap();
        assert!(store.initialized().unwrap());
        recovered.shutdown().await.unwrap();
        recovered.close().await.unwrap();
    }
}
