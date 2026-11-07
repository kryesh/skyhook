use std::sync::Arc;

use skyhook::{
    agent::{
        HarnessBuilder, HostEventsFuture, HostState, HostStateFuture, ResumeFuture, ResumeHook,
        SessionHandle,
    },
    identity::{AgentId, SessionId},
    session::{ExtensionDb, HostCursor, HostEvent, SessionStore},
};

use cli::{
    HookFuture, Opening, PreparedSession, SessionContext, SessionExtension, SessionFactory,
    form::{Choice, Field, FieldKind, ScopeForm, ScopeReply},
    page::Disclosed,
};

use crate::{
    cvss::Severity,
    prompts, research_view,
    store::{Coverage, EventId, LivePolicy, RunFacts, RunOptions, RunState, Store},
    tools,
};

/// What a session starts with: configuration and flags first, then the scope form. A new run
/// takes its options and its live policy, which it keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scope {
    pub options: RunOptions,
    pub live: LivePolicy,
}

/// The scope form's pages, in order: a new session is a new run.
#[derive(Clone, Copy)]
enum ScopeField {
    Shortlist,
    Live,
    Workers,
}

impl ScopeField {
    const ALL: [Self; 3] = [Self::Shortlist, Self::Live, Self::Workers];

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
            Self::Live => Field {
                name: "Live work".into(),
                prompt: "Whether agents work against a running target: probing it and demonstrating validated findings in an authorized environment".into(),
                kind: choice(
                    &LivePolicy::ALL.map(|policy| policy.as_str()),
                    LivePolicy::ALL
                        .iter()
                        .position(|policy| *policy == scope.live)
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
                ScopeField::Shortlist if let Some(index) = value.choice() => {
                    self.options.threshold = Severity::ALL[index];
                }
                ScopeField::Live if let Some(index) = value.choice() => {
                    self.live = LivePolicy::ALL[index];
                }
                ScopeField::Workers
                    if let Some(workers) = value.integer().and_then(std::num::NonZeroU16::new) =>
                {
                    self.options.workers = workers;
                }
                ScopeField::Shortlist | ScopeField::Live | ScopeField::Workers => {}
            }
        }
        self
    }
}

#[derive(Clone)]
pub struct ResearchFactory {
    scope: Scope,
    /// A worker count the command line set, which resizes a resumed run too.
    resize: Option<std::num::NonZeroU16>,
    /// The session the launch resumes, with the live policy configuration decided it runs under.
    resumed: Option<(SessionId, LivePolicy)>,
}

impl ResearchFactory {
    /// A factory whose new sessions start from `scope`; a resumed session keeps its run's
    /// threshold, and its workers unless `resize` sets them.
    pub fn new(scope: Scope, resize: Option<std::num::NonZeroU16>) -> Self {
        Self {
            scope,
            resize,
            resumed: None,
        }
    }

    /// The factory, opening session `id` under `live`, as [`resumed_policy`] decided it.
    #[must_use]
    pub fn resuming(self, id: SessionId, live: LivePolicy) -> Self {
        Self {
            resumed: Some((id, live)),
            ..self
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
            // A new session's form answers, when the terminal sent them, over the defaults.
            let scope = match context.opening {
                Opening::New(Some(reply)) => self.scope.answered(&ScopeField::ALL, reply),
                Opening::New(None) | Opening::Resume(_) => self.scope,
            };
            let live = match (context.opening, self.resumed) {
                (Opening::New(_), _) => scope.live,
                (Opening::Resume(id), Some((resumed, live))) if resumed == id => live,
                // Another session the terminal opens.
                (Opening::Resume(id), _) => {
                    resumed_policy(context.workspace, id, scope.live).await?
                }
            };
            let research = Arc::new(ResearchSession {
                scope,
                resize: self.resize,
                facts: Cached::default(),
                coverage: Cached::default(),
                page: Cached::default(),
                opened: research_view::Opened::now(),
                quiet: Default::default(),
            });
            let builder = builder
                .prompts(prompts::prompts(live))
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
    /// A new run's threshold, workers and live policy.
    scope: Scope,
    resize: Option<std::num::NonZeroU16>,
    /// The run's facts the orchestrator's host state or the Research page last read, reused
    /// until research or a job moves on.
    facts: Cached<Watermark, RunFacts>,
    /// What the run holds as the Research page last read it, reused until research is written:
    /// no job's state bears on it.
    coverage: Cached<Generation, Coverage>,
    /// The last Research page, with what it was read at and for: an unchanged store,
    /// disclosure and uptime read nothing.
    page: Cached<(Watermark, Disclosed, research_view::Uptime), cli::page::Page>,
    opened: research_view::Opened,
    /// Each agent found with nothing to be told at a research generation and cursor: it stays
    /// so until research is written.
    quiet: std::sync::Mutex<std::collections::HashMap<AgentId, (u64, Option<HostCursor>)>>,
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
        read: impl Future<Output = Result<T, cli::Error>>,
    ) -> Result<Arc<T>, cli::Error> {
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

/// One session's research as of its write generation.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Generation {
    session: SessionId,
    generation: Option<u64>,
}

/// One session's research as of its write generation and the journal's latest job transition:
/// a job beginning or ending is no research write, yet it moves what is running, exhausted or
/// being proven.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Watermark {
    research: Generation,
    journal: Option<i64>,
}

impl Watermark {
    /// Read before the snapshot it keys begins, so a value cached under it is never older.
    async fn of(store: &Store) -> Result<Self, cli::Error> {
        let research = Generation {
            session: store.id(),
            generation: store.generation(),
        };
        let reader = store.clone();
        let journal = store
            .reading(move || reader.journal_mark().map_err(cli::Error::from))
            .await?;
        Ok(Self { research, journal })
    }
}

/// The research of a session opened by this product's harness.
fn research(session: &SessionStore) -> Result<Store, cli::Error> {
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

    /// What others changed of the work the agent's job began, since its cursor.
    fn events<'a>(
        &'a self,
        extension: &'a ExtensionDb,
        agent: &'a AgentId,
        since: Option<HostCursor>,
    ) -> HostEventsFuture<'a> {
        Box::pin(async move {
            let store = Store::of(extension);
            let generation = store.generation();
            let quiet = |generation| {
                let quiet = self.quiet.lock().expect("research quiet agents");
                quiet.get(agent) == Some(&(generation, since))
            };
            if generation.is_some_and(quiet) {
                return Ok(None);
            }
            let after = since
                .map(|cursor| EventId::try_from(cursor.get()))
                .transpose()?;
            let (reader, actor) = (store.clone(), agent.clone());
            let read =
                store.reading(move || reader.changes(&actor, after).map_err(cli::Error::from));
            match read.await? {
                Some((through, changes)) => {
                    let serde_json::Value::Object(view) = serde_json::to_value(changes)? else {
                        return Err("research changes are not an object".into());
                    };
                    Ok(Some(HostEvent {
                        through: HostCursor::try_from(through.get())?,
                        view: view.try_into()?,
                    }))
                }
                None => {
                    if let Some(generation) = generation {
                        let mut quiet = self.quiet.lock().expect("research quiet agents");
                        quiet.insert(agent.clone(), (generation, since));
                    }
                    Ok(None)
                }
            }
        })
    }
}

/// What this launch changes about a run it reopens, applied once its work begins: its workers,
/// when this launch's flag resizes them.
impl ResumeHook for ResearchSession {
    fn resumed<'a>(&'a self, extension: &'a ExtensionDb) -> ResumeFuture<'a> {
        Box::pin(async move {
            let Some(workers) = self.resize else {
                return Ok(());
            };
            let store = Store::of(extension);
            store
                .clone()
                .writing(move || store.set_workers(workers).map_err(cli::Error::from))
                .await
        })
    }
}

/// The live policy resumed session `id` of `workspace` runs under: a reopened run keeps the
/// policy it started under; one whose setup an interruption left undone starts under
/// `launch`.
pub async fn resumed_policy(
    workspace: &std::path::Path,
    id: SessionId,
    launch: LivePolicy,
) -> Result<LivePolicy, cli::Error> {
    let root = skyhook::config::workspace_session_root(workspace);
    let store = Store::inspect(&root, id).await?;
    let reader = store.clone();
    store
        .reading(move || {
            let live = match reader.initialized()? {
                true => reader.run()?.live,
                false => launch,
            };
            Ok::<_, cli::Error>(live)
        })
        .await
}

impl SessionExtension for ResearchSession {
    fn views(&self) -> &[cli::ExtensionView] {
        research_view::VIEWS
    }

    fn refresh(
        &self,
        _view: usize,
        session: SessionHandle,
        open: Disclosed,
    ) -> HookFuture<'_, Arc<cli::page::Page>> {
        Box::pin(async move {
            let store = research(session.store())?;
            let (mark, uptime) = (Watermark::of(&store).await?, self.opened.uptime());
            let read = async {
                let facts = cached(&self.facts, mark, store.clone(), Store::run_facts).await?;
                let coverage = cached(
                    &self.coverage,
                    mark.research,
                    store.clone(),
                    Store::coverage,
                )
                .await?;
                let (reader, open) = (store.clone(), open.clone());
                store
                    .reading(move || {
                        let state = RunState {
                            facts,
                            coverage,
                            open_tests: reader.open_tests()?,
                        };
                        (state.page(&reader, &open, Some(uptime))).map_err(cli::Error::from)
                    })
                    .await
            };
            self.page.get((mark, open.clone(), uptime), read).await
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
                .reading(move || reader.initialized().map_err(cli::Error::from))
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
            store
                .writing(move || {
                    started.start_run(scope.options, scope.live)?;
                    Ok::<_, cli::Error>(())
                })
                .await?;
            Ok(())
        })
    }

    fn scope<'a>(&'a self, session: &'a SessionHandle) -> HookFuture<'a, Option<ScopeForm>> {
        Box::pin(async move {
            let store = research(session.store())?;
            let reader = store.clone();
            let run = store
                .reading(move || {
                    let run = match reader.initialized()? {
                        true => Some(reader.run()?),
                        false => None,
                    };
                    Ok::<_, cli::Error>(run)
                })
                .await?;
            // A resize this launch applies once the run's work resumes is shown from the start.
            let scope = run.map(|run| Scope {
                options: RunOptions {
                    workers: self.resize.unwrap_or(run.options.workers),
                    ..run.options
                },
                live: run.live,
            });
            Ok(scope.map(|scope| scope.form("Scope".to_owned(), &ScopeField::ALL, String::new())))
        })
    }
}

impl ResearchSession {
    async fn root_state(&self, store: Store) -> Result<String, cli::Error> {
        let mark = Watermark::of(&store).await?;
        let facts = cached(&self.facts, mark, store, Store::run_facts).await?;
        Ok(facts.state())
    }
}

/// What `read` gives for the session's research, reused while it stands at `mark`.
async fn cached<K: PartialEq, T: Send + Sync + 'static>(
    cache: &Cached<K, T>,
    mark: K,
    store: Store,
    read: fn(&Store) -> crate::store::Result<T>,
) -> Result<Arc<T>, cli::Error> {
    let reader = store.clone();
    let read = store.reading(move || read(&reader).map_err(cli::Error::from));
    cache.get(mark, read).await
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use skyhook::provider::{
        Provider, ProviderContext, ProviderError, ProviderErrorKind, ResponseStream,
        protocol::{ContextId, ModelRequest},
    };

    use super::*;
    use crate::{
        store::tests::{harness as builder, harness_on},
        tools::tests::bounded,
    };

    fn context<'a>(workspace: &'a Path, opening: Opening<'a>) -> SessionContext<'a> {
        SessionContext {
            workspace,
            opening,
            headless: false,
        }
    }

    fn scope(live: LivePolicy) -> Scope {
        Scope {
            options: RunOptions::default(),
            live,
        }
    }

    fn factory(live: LivePolicy, resize: Option<std::num::NonZeroU16>) -> ResearchFactory {
        ResearchFactory::new(scope(live), resize)
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

    /// The extension over a session of this product, with nothing read yet.
    fn host() -> ResearchSession {
        ResearchSession {
            scope: scope(LivePolicy::No),
            resize: None,
            facts: Cached::default(),
            coverage: Cached::default(),
            page: Cached::default(),
            opened: research_view::Opened::now(),
            quiet: Default::default(),
        }
    }

    /// A provider that records each request and refuses it.
    #[derive(Clone, Default)]
    struct Recording(Arc<std::sync::Mutex<Vec<ModelRequest>>>);
    impl Provider for Recording {
        fn open_context(&self, _: ContextId) -> Result<Box<dyn ProviderContext>, ProviderError> {
            Ok(Box::new(self.clone()))
        }
    }
    impl ProviderContext for Recording {
        fn invoke(&mut self, request: ModelRequest) -> ResponseStream {
            self.0.lock().unwrap().push(request);
            let refused = ProviderErrorKind::Authentication.error("recorded");
            Box::pin(futures_util::stream::iter([Err(refused)]))
        }
    }

    /// A root launched as the terminal launches it, from the product's configuration with
    /// the ceiling of every mode it can switch to, schedules the run and writes its reports
    /// out, from its own tool surface and from a script it runs, new and reopened.
    #[tokio::test]
    async fn the_launched_root_coordinates_the_run() {
        use cli::Product;
        use skyhook::{config::Config, tool::policy::Capability};
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path();
        let explicit = workspace.join("security.yaml");
        std::fs::write(&explicit, "{}").unwrap();
        let mut config = Config::from_yaml("{}").unwrap();
        let args = crate::product::Args {
            live: None,
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
                let tools = &requests.last().unwrap().tools;
                let tool = tools.iter().find(|offered| offered.name == tool).unwrap();
                let actions = tool.input_schema["properties"]["action"]["enum"].as_array();
                actions.unwrap().iter().any(|offered| offered == action)
            };
            assert!(offered("research_query", "startable"));
            assert!(offered("research", "generate"));
            let script = session.run_script(
                "return [await tool.research_query({action: 'startable', limit: 1}), await tool.research({action: 'generate'})].map(r => r.error);",
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

    /// A child is told, as a host event, of the root's change to the work its job began, past
    /// its cursor once; the root, which has no task, is told nothing.
    #[tokio::test]
    async fn a_child_is_told_of_changes_to_its_begun_work() {
        let directory = tempfile::tempdir().unwrap();
        let (session, _) = open(
            &factory(LivePolicy::No, None),
            directory.path(),
            Opening::New(None),
        )
        .await;
        let store = research(session.store()).unwrap();
        let root = session.root_agent().clone();
        let worker = store.clone();
        let (child, path) = tokio::task::spawn_blocking(move || {
            use crate::store::tests::{begun, component, path, text};
            let decoder = component(&worker, &root, "decoder");
            let stream = path(&worker, &root, "stream", &[decoder]);
            let (child, _, _) = begun(&worker, &crate::store::Begin::Sweep(vec![stream]));
            let renamed = crate::store::Upsert::Path(crate::store::Change::Edit(
                stream,
                crate::store::Edit {
                    title: None,
                    body: Some(text("socket to inflate")),
                    detail: (),
                },
            ));
            worker.upsert(&root, &renamed).unwrap();
            (child, stream)
        })
        .await
        .unwrap();
        let research = session.store().extension().unwrap();
        let host = host();
        let told = host.events(research, &child, None).await.unwrap().unwrap();
        assert_eq!(
            serde_json::Value::Object(told.view.fields().clone()),
            serde_json::json!({"research_changes": [{"phase": "sweep", "node": path,
                "title": "stream", "count": 1, "latest": told.through.get(), "kinds": ["edited"],
                "recent": [{"id": told.through.get(), "kind": "edited",
                    "nodes": [{"node": path, "title": "stream", "effect": "changed"}]}]}]})
        );
        assert!(
            host.events(research, &child, Some(told.through))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            host.events(research, session.root_agent(), None)
                .await
                .unwrap()
                .is_none()
        );
        let unknown = HostCursor::try_from(0).unwrap();
        assert!(
            host.events(research, &child, Some(unknown)).await.is_err(),
            "a cursor the store cannot read fails rather than reads as nothing"
        );
        // The child stands in for a launch the journal never recorded, which shutdown trips on.
        let _ = session.shutdown().await;
    }

    /// Viewing a reopened session changes nothing; a worker flag applies once its work
    /// resumes. The run keeps the live policy it started under, and so do its agents'
    /// instructions. A session whose setup an interruption left undone is set up on open.
    #[tokio::test]
    async fn a_reopened_run_keeps_its_policy_and_resizes_once_its_work_resumes() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path();
        let (session, _) = open(
            &factory(LivePolicy::No, None),
            workspace,
            Opening::New(None),
        )
        .await;
        let id = session.id();
        session.shutdown().await.unwrap();
        session.close().await.unwrap();

        let asking = factory(LivePolicy::Ask, std::num::NonZeroU16::new(4));
        let requests = Recording::default();
        let reopening = context(workspace, Opening::Resume(id));
        let recorded = harness_on(workspace, Arc::new(requests.clone()));
        let prepared = asking.prepare(recorded, reopening).await.unwrap();
        let viewed = prepared.builder.build().await.unwrap();
        let viewed = viewed.resume_session(id).await.unwrap();
        prepared.extension.opened(&viewed, reopening).await.unwrap();
        let store = research(viewed.store()).unwrap();
        let settings = |store: &Store| {
            let run = store.run().unwrap();
            (run.live, run.options.workers.get())
        };
        let configured = RunOptions::default().workers.get();
        // The terminal reads the scope as it opens the session, before its work resumes: it
        // shows the run's own policy, not this launch's, and the resize this launch applies.
        let scope = || async {
            let scope = prepared.extension.scope(&viewed).await.unwrap().unwrap();
            let shown = (scope.fields.iter()).map(|field| match &field.kind {
                FieldKind::Choice(choice) => choice.options()[choice.selected()].clone(),
                FieldKind::Integer { value, .. } => value.to_string(),
            });
            shown.collect::<Vec<_>>()
        };
        let threshold = RunOptions::default().threshold.as_str().to_owned();
        let expected = [threshold, "no".to_owned(), "4".to_owned()];
        assert_eq!(scope().await, expected);
        assert_eq!(settings(&store), (LivePolicy::No, configured));
        viewed.resume_work().await.unwrap();
        assert_eq!(settings(&store), (LivePolicy::No, 4));
        assert_eq!(scope().await, expected);
        assert!(viewed.prompt("continue").await.is_err());
        let system = serde_json::to_string(&requests.0.lock().unwrap()[0].system).unwrap();
        assert!(!system.contains("## PoC gate"), "{system}");
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

    /// A provider whose requests wait until the test lets them fail, telling it each began.
    #[derive(Clone)]
    struct Gated {
        invoked: Arc<tokio::sync::Notify>,
        gate: Arc<tokio::sync::Semaphore>,
    }
    impl Provider for Gated {
        fn open_context(&self, _: ContextId) -> Result<Box<dyn ProviderContext>, ProviderError> {
            Ok(Box::new(self.clone()))
        }
    }
    impl ProviderContext for Gated {
        fn invoke(&mut self, _: ModelRequest) -> ResponseStream {
            self.invoked.notify_one();
            let gate = self.gate.clone();
            Box::pin(futures_util::stream::once(async move {
                let _closed = gate.acquire().await;
                Err(ProviderErrorKind::Authentication.error("released"))
            }))
        }
    }

    /// Only the root's state carries the run, a child's is a plain agent's, and its plan line and
    /// the Research page follow a task's job ending, which the journal records with no research
    /// write.
    #[tokio::test]
    async fn a_job_ending_moves_the_cached_state_and_page() {
        use cli::page::Block;
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path();
        let provider = Gated {
            invoked: Arc::default(),
            gate: Arc::new(tokio::sync::Semaphore::new(0)),
        };
        let factory = factory(LivePolicy::No, None);
        let context = context(workspace, Opening::New(None));
        let builder = harness_on(workspace, Arc::new(provider.clone()));
        let prepared = factory.prepare(builder, context).await.unwrap();
        let session = prepared.builder.build().await.unwrap().new_session().await;
        let session = session.unwrap();
        prepared.extension.opened(&session, context).await.unwrap();
        let host = host();
        let store = research(session.store()).unwrap();
        let root = session.root_agent().clone();
        let decoder = crate::store::tests::component(&store, &root, "decoder");
        let stream = crate::store::tests::path(&store, &root, "stream", &[decoder]);
        let extension = session.store().extension().unwrap();
        let state = host.state(extension, &root).await.unwrap();
        assert!(
            state
                .as_str()
                .starts_with(&format!("run: {}\n", session.id()))
        );
        assert!(host.state(extension, &root.child(1)).await.is_none());
        let plan = async || {
            let state = host.state(extension, &root).await.unwrap();
            let line = state
                .as_str()
                .lines()
                .find(|line| line.starts_with("plan: "));
            line.unwrap().to_owned()
        };
        // The Tasks panel's running sweeps.
        let running = async || {
            let page = (host.refresh(0, session.clone(), Disclosed::default()).await).unwrap();
            let tasks = (page.blocks.iter())
                .find_map(|block| match block {
                    Block::Panels(panels) => panels.iter().find(|panel| panel.title == "Tasks"),
                    _ => None,
                })
                .unwrap();
            let sweep = tasks.rows.iter().find(|row| row.label == "sweep").unwrap();
            sweep.values[0].text.clone()
        };
        let script = session.run_script("return (await tool.agent({prompt: 'Sweep.'})).state;");
        let working = async {
            bounded(provider.invoked.notified()).await;
            let index = store
                .read(|db| {
                    crate::store::stored(
                        db,
                        "SELECT child_index FROM agent WHERE owner_job IS NOT NULL",
                        Vec::new(),
                        |row| Ok(row.get::<u32>(0)?),
                    )
                })
                .unwrap();
            let sweep = crate::store::Begin::Sweep(vec![stream]);
            store.begin(&root.child(index), &sweep).unwrap();
            assert_eq!(plan().await, "plan: nothing owed; 1 running");
            assert_eq!(running().await, "1");
            provider.gate.close();
            store.generation()
        };
        tokio::pin!(script);
        let generation = tokio::select! {
            _ = &mut script => unreachable!("the child waits on its provider"),
            began = working => began,
        };
        let ended = bounded(script).await;
        assert_eq!(ended.unwrap().value["value"], "failed");
        assert_eq!(store.generation(), generation, "no research write");
        assert_eq!(plan().await, "plan: owed: 1 sweep");
        assert_eq!(running().await, "0");
        session.shutdown().await.unwrap();
        session.close().await.unwrap();
    }
}
