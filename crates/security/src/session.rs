use std::sync::Arc;

use skyhook::identity::{AgentId, SessionId};
use skyhook::{
    agent::{
        HarnessBuilder, HostEventsFuture, HostState, HostStateFuture, ResumeFuture, ResumeHook,
        SessionHandle,
    },
    session::{ExtensionDb, HostCursor, HostEvent},
};

use skyhook::product::{
    Attendance, Error, HookFuture, Opening, PreparedSession, SessionContext, SessionExtension,
    SessionFactory,
    form::{Choice, Field, FieldKind, ScopeForm, ScopeReply},
    page::{Disclosed, Page},
};

use crate::{
    cvss::Severity,
    prompts, research_view,
    store::{Coverage, EventId, LivePolicy, RunFacts, RunOptions, RunState, Store},
    tools,
};

/// The scope form's pages, in order: a new session is a new run.
#[derive(Clone, Copy)]
enum ScopeField {
    Shortlist,
    Live,
    Workers,
}

impl ScopeField {
    const ALL: [Self; 3] = [Self::Shortlist, Self::Live, Self::Workers];

    fn field(self, options: &RunOptions) -> Field {
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
                        .position(|severity| *severity == options.threshold)
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
                        .position(|policy| *policy == options.live)
                        .expect("policy is listed"),
                ),
            },
            Self::Workers => Field {
                name: "Workers".into(),
                prompt: "How many agents the orchestrator's scheduler runs at once".into(),
                kind: integer(options.workers),
            },
        }
    }

    /// The scope form showing `options`.
    fn form(options: &RunOptions, message: String) -> ScopeForm {
        ScopeForm {
            title: "Scope".to_owned(),
            fields: Self::ALL.iter().map(|field| field.field(options)).collect(),
            message,
        }
    }

    /// `options` with a form's answers applied, field by field in form order.
    fn answered(mut options: RunOptions, reply: &ScopeReply) -> RunOptions {
        for (field, value) in Self::ALL.iter().zip(reply) {
            match field {
                Self::Shortlist
                    if let Some(&threshold) =
                        value.choice().and_then(|at| Severity::ALL.get(at)) =>
                {
                    options.threshold = threshold;
                }
                Self::Live
                    if let Some(&live) = value.choice().and_then(|at| LivePolicy::ALL.get(at)) =>
                {
                    options.live = live;
                }
                Self::Workers
                    if let Some(workers) = value.integer().and_then(std::num::NonZeroU16::new) =>
                {
                    options.workers = workers;
                }
                Self::Shortlist | Self::Live | Self::Workers => {}
            }
        }
        options
    }
}

#[derive(Clone)]
pub(crate) struct ResearchFactory {
    options: RunOptions,
    /// A worker count the command line set, which resizes a resumed run too.
    resize: Option<std::num::NonZeroU16>,
}

impl ResearchFactory {
    /// A factory whose new sessions start from `options`; a resumed session keeps its run's,
    /// but for its workers when `resize` sets them.
    pub(crate) fn new(options: RunOptions, resize: Option<std::num::NonZeroU16>) -> Self {
        Self { options, resize }
    }
}

impl SessionFactory for ResearchFactory {
    fn form(&self, context: SessionContext<'_>) -> Option<ScopeForm> {
        if let Opening::Resume(_) = context.opening {
            return None;
        }
        Some(ScopeField::form(
            &self.options,
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
            let options = match context.opening {
                Opening::New(Some(reply)) => ScopeField::answered(self.options, reply),
                Opening::New(None) | Opening::Resume(_) => self.options,
            };
            // The policy is checked on what the opened run is under.
            let live = match context.opening {
                Opening::New(_) => options.live,
                Opening::Resume(id) => resumed_policy(context.workspace, id, options.live).await?,
            };
            if context.attendance == Attendance::Batch && live == LivePolicy::Ask {
                return Err(crate::product::Refused::BatchAsk.into());
            }
            let research = Arc::new(ResearchSession {
                options,
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
pub(crate) struct ResearchSession {
    /// A new run's options.
    options: RunOptions,
    resize: Option<std::num::NonZeroU16>,
    /// The run's facts the orchestrator's host state or the Research page last read, reused
    /// until research or a job moves on.
    facts: Cached<Watermark, RunFacts>,
    /// What the run holds as the Research page last read it, reused until research is written:
    /// no job's state bears on it.
    coverage: Cached<Generation, Coverage>,
    /// The last Research page, with what it was read at and for: an unchanged store,
    /// disclosure and uptime read nothing.
    page: Cached<(Watermark, Disclosed, research_view::Uptime), Page>,
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
        read: impl Future<Output = Result<T, Error>>,
    ) -> Result<Arc<T>, Error> {
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
    async fn of(store: &Store) -> Result<Self, Error> {
        let research = Generation {
            session: store.id(),
            generation: store.generation(),
        };
        let reader = store.clone();
        let journal = store
            .reading(move || reader.journal_mark().map_err(Error::from))
            .await?;
        Ok(Self { research, journal })
    }
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
            let read = store.reading(move || reader.changes(&actor, after).map_err(Error::from));
            match read.await? {
                Some((through, changes)) => {
                    let serde_json::Value::Object(view) = serde_json::to_value(changes)? else {
                        return Err("research changes are not an object".into());
                    };
                    Ok(Some(HostEvent {
                        through: HostCursor::try_from(i64::from(through))?,
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
                .writing(move || store.set_workers(workers).map_err(Error::from))
                .await
        })
    }
}

/// The live policy resumed session `id` of `workspace` runs under: a reopened run keeps the
/// policy it started under; one whose setup an interruption left undone starts under
/// `launch`.
async fn resumed_policy(
    workspace: &std::path::Path,
    id: SessionId,
    launch: LivePolicy,
) -> Result<LivePolicy, Error> {
    let root = skyhook::config::workspace_session_root(workspace);
    let store = Store::inspect(&root, id).await?;
    let reader = store.clone();
    store
        .reading(move || {
            let run = reader.started()?;
            Ok::<_, Error>(run.map_or(launch, |run| run.options.live))
        })
        .await
}

impl SessionExtension for ResearchSession {
    fn views(&self) -> &[&'static str] {
        research_view::VIEWS
    }

    fn refresh(
        &self,
        _view: usize,
        session: SessionHandle,
        open: Disclosed,
    ) -> HookFuture<'_, Arc<Page>> {
        Box::pin(async move {
            let store = Store::of_session(session.store().extension())?;
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
                        (state.page(&reader, &open, Some(uptime))).map_err(Error::from)
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
            let store = Store::of_session(session.store().extension())?;
            let started = store.clone();
            let reader = store.clone();
            if store
                .reading(move || reader.started().map_err(Error::from))
                .await?
                .is_some()
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
            let options = self.options;
            store
                .writing(move || {
                    started.start_run(options)?;
                    Ok::<_, Error>(())
                })
                .await?;
            Ok(())
        })
    }

    fn scope<'a>(&'a self, session: &'a SessionHandle) -> HookFuture<'a, Option<ScopeForm>> {
        Box::pin(async move {
            let store = Store::of_session(session.store().extension())?;
            let reader = store.clone();
            let run = store
                .reading(move || reader.started().map_err(Error::from))
                .await?;
            // A resize this launch applies once the run's work resumes is shown from the start.
            let options = run.map(|run| RunOptions {
                workers: self.resize.unwrap_or(run.options.workers),
                ..run.options
            });
            Ok(options.map(|options| ScopeField::form(&options, String::new())))
        })
    }
}

impl ResearchSession {
    async fn root_state(&self, store: Store) -> Result<String, Error> {
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
) -> Result<Arc<T>, Error> {
    let reader = store.clone();
    let read = store.reading(move || read(&reader).map_err(Error::from));
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
        store::{
            Listed,
            tests::{harness as builder, harness_on},
        },
        tools::tests::bounded,
    };

    fn context<'a>(workspace: &'a Path, opening: Opening<'a>) -> SessionContext<'a> {
        SessionContext {
            workspace,
            opening,
            attendance: Attendance::Attended,
        }
    }

    fn factory(live: LivePolicy, resize: Option<std::num::NonZeroU16>) -> ResearchFactory {
        let options = RunOptions {
            live,
            ..RunOptions::default()
        };
        ResearchFactory::new(options, resize)
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
            options: RunOptions::default(),
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
        use skyhook::config::Config;
        use skyhook::product::Product;
        use skyhook::tool::policy::Capability;
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path();
        let mut config = Config::from_yaml("{}").unwrap();
        let args = crate::product::Options {
            live: None,
            shortlist: None,
            workers: None,
            config: Default::default(),
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
    /// its cursor once.
    #[tokio::test]
    async fn a_child_is_told_of_changes_to_its_begun_work() {
        let directory = tempfile::tempdir().unwrap();
        let (session, _) = open(
            &factory(LivePolicy::No, None),
            directory.path(),
            Opening::New(None),
        )
        .await;
        let store = Store::of_session(session.store().extension()).unwrap();
        let root = session.root_agent().clone();
        let worker = store.clone();
        let (child, path) = tokio::task::spawn_blocking(move || {
            use crate::store::tests::{begun, component, path, text};
            let decoder = component(&worker, &root, "decoder");
            let stream = path(&worker, &root, "stream", &[decoder]);
            let (child, _, _) = begun(&worker, &crate::store::Begin::Sweep(Listed::of([stream])));
            let renamed =
                crate::store::Upsert::Edit(crate::store::NodeEdit::Path(crate::store::PathEdit {
                    id: stream,
                    title: None,
                    body: Some(text("socket to inflate")),
                    steps: None,
                }));
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
                "title": "stream", "count": 1, "latest": i64::from(told.through), "kinds": ["edited"],
                "recent": [{"id": i64::from(told.through), "kind": "edited",
                    "nodes": [{"node": path, "title": "stream", "effect": "changed"}]}]}]})
        );
        assert!(
            host.events(research, &child, Some(told.through))
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
        let store = Store::of_session(viewed.store().extension()).unwrap();
        let settings = |store: &Store| {
            let run = store.started().unwrap().unwrap();
            (run.options.live, run.options.workers.get())
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
        let store = Store::of_session(recovered.store().extension()).unwrap();
        assert!(store.started().unwrap().is_some());
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
        use skyhook::product::page::Block;
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
        let store = Store::of_session(session.store().extension()).unwrap();
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
            let sweep = crate::store::Begin::Sweep(Listed::of([stream]));
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
