//! The session store: opening, creating and locking a session, and the write, read
//! and wait API every change goes through.

use std::{
    fs::OpenOptions as StdOpenOptions,
    path::{Path, PathBuf},
    sync::Arc,
};

use super::{
    Command, CoreState, EventRecord, ExtensionDb, JobRow, Plan, Reads, RecordSeq, RequestSeq,
    Schema, SessionError, SessionEvent, SessionPhase, SessionSummary, SessionTitle, SharedDb, Wait,
    core, db,
};
use crate::db::DbError;
use crate::identity::{AgentId, HostName, SessionId};
use crate::target::{Revisions, TargetRegistry};
use crate::tool::policy::{Capability, PermissionUse, ResourceId};
use futures_util::StreamExt;
use skyhook_base::blocking;
use skyhook_tools::target::TargetRef;
use skyhook_worker::{fs::spill::SpillFile, job::JobId};

pub(super) const DATABASE_FILE: &str = "session.db";

const LOCK_FILE: &str = "lock";

pub(super) struct SessionLock(pub(super) std::fs::File);

impl Drop for SessionLock {
    fn drop(&mut self) {
        // Ownership ends with the store, not with a transient fork-inherited fd.
        let _ = self.0.unlock();
    }
}

/// How long an open waits before its one retry of a held lock: `is_open` holds
/// it shared for an instant, which must not make an open fail.
const LOCK_RETRY: std::time::Duration = std::time::Duration::from_millis(50);

fn lock_session(directory: &Path, id: SessionId) -> Result<SessionLock, SessionError> {
    let lock = StdOpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join(LOCK_FILE))?;
    lock.try_lock()
        .or_else(|_| {
            std::thread::sleep(LOCK_RETRY);
            lock.try_lock()
        })
        .map_err(|_| SessionError::AlreadyOpen(id))?;
    Ok(SessionLock(lock))
}

/// How a store opens its session, with the schema it creates or checks.
enum Opening {
    Create(HostName, Schema),
    /// Held in memory for its process; nothing of it survives.
    #[cfg(test)]
    Ephemeral(HostName, Schema),
    /// A saved session, with a host's schema when that host created it.
    Existing(Option<(HostName, Schema)>),
}

pub(super) struct StoreInner {
    pub(super) host: HostName,
    pub(super) shared: Arc<core::Shared>,
    pub(super) db: SharedDb,
    pub(super) journal: Journal,
    pub(super) core: core::Core,
    pub(super) extension: Option<ExtensionDb>,
}

#[derive(Clone)]
pub struct SessionStore {
    pub(super) inner: Arc<StoreInner>,
}

/// Tells a session's observers when a run's captured output changes; see
/// [`crate::session::RuntimeEvent::OutputChanged`]. It keeps nothing of the
/// session alive.
#[derive(Clone)]
pub(crate) struct OutputChanges(std::sync::Weak<core::Shared>);

impl OutputChanges {
    /// `run` captured output that is now readable.
    pub(crate) fn changed(&self, run: crate::job::Run) {
        if let Some(shared) = self.0.upgrade() {
            shared.output_changed(run);
        }
    }

    /// `run`'s output is being read: what it captures from now on is news.
    pub(crate) fn read(&self, run: crate::job::Run) {
        if let Some(shared) = self.0.upgrade() {
            shared.observed().output_read(run);
        }
    }

    /// `capture`'s target told of the `latest` of the stream it is keeping, which
    /// replaces what it told before; none once the capture is finished or gone.
    pub(crate) fn latest(&self, capture: i64, latest: Option<LiveTail>) {
        if let Some(shared) = self.0.upgrade() {
            let mut observed = shared.observed();
            match latest {
                Some(latest) => observed.latest.insert(capture, latest),
                None => observed.latest.remove(&capture),
            };
        }
    }

    /// The latest of each capture whose target is keeping its stream.
    pub(crate) fn following(&self) -> std::collections::HashMap<i64, LiveTail> {
        let shared = self.0.upgrade();
        shared.map_or_else(Default::default, |shared| shared.observed().latest.clone())
    }
}

/// The latest of a stream its target keeps in a file while its command runs: what
/// the target counted of it so far, and its last bytes. The capture's rows hold its
/// first bytes, and what lies between is on the target until the command ends.
#[derive(Clone)]
pub(crate) struct LiveTail {
    pub extent: db::CaptureExtent,
    pub text: Arc<str>,
}

/// The files the session's runs had their targets create in their spill directories,
/// each with the route its run reached that target along. They are the session's own
/// output: reading one needs no approval wherever that route still reaches, every
/// target on it defined as it was then.
#[derive(Default)]
pub(crate) struct OwnSpills(std::collections::HashMap<ResourceId, Revisions>);

impl OwnSpills {
    /// Those the journal's rows hold, each as its route and its path's components.
    pub(super) fn opened(files: Vec<(Revisions, Vec<String>)>) -> Self {
        let mut own = Self::default();
        for (route, components) in files {
            own.own(route, components);
        }
        own
    }

    /// The file at `components` on the machine `route` ends at, the session host
    /// when it names no target.
    fn own(&mut self, route: Revisions, components: Vec<String>) {
        let target = (route.last()).map_or(TargetRef::Root, |(name, _)| name.clone().into());
        self.0
            .insert(ResourceId::Path { target, components }, route);
    }

    /// Whether `permission` reads one of them on the route `targets` still define
    /// as its run reached it.
    pub(crate) fn covers(&self, permission: &PermissionUse, targets: &TargetRegistry) -> bool {
        permission.capability == Capability::Read
            && (self.0.get(&permission.resource)).is_some_and(|route| targets.current(route))
    }
}

/// The committed journal, read by index: what the core reads back of the jobs it
/// no longer holds. Each read runs in one snapshot of what was committed.
#[derive(Clone)]
pub(crate) struct Journal {
    pub(super) session: SessionId,
    pub(super) reads: Reads,
}

impl Journal {
    /// The status rows of those of `jobs` that exist, in id order.
    pub(crate) fn jobs(&self, jobs: &[JobId]) -> Result<Vec<JobRow>, SessionError> {
        let session = self.session;
        Ok(self.reads.read(|db| db::job_rows(db, session, jobs))?)
    }

    /// The records `agent`'s contract is journaled in, as its loop starts: those of
    /// the `profiled` and `granted` entries and the agent model `context` its state names.
    pub(super) fn agent(
        &self,
        agent: &AgentId,
        entries: [RecordSeq; 2],
        context: Option<RecordSeq>,
    ) -> Result<Option<db::AgentRecords>, SessionError> {
        let session = self.session;
        Ok(self
            .reads
            .read(|db| db::agent(db, session, agent, entries, context))?)
    }
}

/// The state the journal in `db` opens to, as its rows hold it.
pub(super) fn open_state(
    db: &crate::db::Db,
    id: SessionId,
) -> Result<(CoreState, core::Observed), SessionError> {
    core::CoreState::open(db::counters(db)?, db::opening(db, id)?)
}

/// `work` for the core, run once `jobs` are held, in the same call.
fn held<T, E: From<SessionError>>(
    jobs: impl IntoIterator<Item = JobId>,
    work: impl FnOnce(&mut core::Writer) -> Result<T, E> + Send + 'static,
) -> impl FnOnce(&mut core::Writer) -> Result<T, E> + Send + 'static {
    let jobs: Vec<_> = jobs.into_iter().collect();
    move |writer| {
        writer.hold(jobs)?;
        work(writer)
    }
}

/// A store reference that keeps neither the session's core nor its lock.
pub(crate) struct WeakStore(std::sync::Weak<StoreInner>);

impl WeakStore {
    pub(crate) fn upgrade(&self) -> Option<SessionStore> {
        self.0.upgrade().map(|inner| SessionStore { inner })
    }
}

impl SessionStore {
    /// Read a session without taking its lock; a live owner may keep writing.
    pub async fn read_records(
        root: &Path,
        id: SessionId,
    ) -> Result<Vec<Arc<EventRecord>>, SessionError> {
        let read = move |db: &crate::db::Db| db::decode_records(db, id, db::Entries::All);
        Self::read_only(root, id, read).await
    }

    /// The sessions stored under `root`: every entry named as a session id. A missing
    /// root holds none.
    pub async fn ids(root: &Path) -> std::io::Result<Vec<SessionId>> {
        let mut entries = match tokio::fs::read_dir(root).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut ids = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            ids.extend(
                entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse::<SessionId>().ok()),
            );
        }
        Ok(ids)
    }

    /// `read` of every session in `root`, as the reads finish, a few overlapping.
    pub async fn read_each<T, F: Future<Output = T>>(
        root: &Path,
        read: impl FnMut(SessionId) -> F,
    ) -> Result<Vec<T>, SessionError> {
        // Each read holds what one session's read takes.
        const CONCURRENT_READS: usize = 4;
        let reads = futures_util::stream::iter(Self::ids(root).await?).map(read);
        Ok(reads.buffer_unordered(CONCURRENT_READS).collect().await)
    }

    /// Whether a store, in this process or another, holds the session open. The
    /// probe shares the lock for an instant; an open racing it waits that out.
    pub async fn is_open(root: &Path, id: SessionId) -> Result<bool, SessionError> {
        let path = root.join(id.to_string()).join(LOCK_FILE);
        blocking(move || {
            let lock = match StdOpenOptions::new().read(true).open(&path) {
                Ok(lock) => lock,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            match lock.try_lock_shared() {
                Ok(()) => {
                    lock.unlock()?;
                    Ok(false)
                }
                Err(std::fs::TryLockError::WouldBlock) => Ok(true),
                Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
            }
        })
        .await
    }

    /// A session list row, without decoding the session or taking its lock. Given
    /// `owner`, one that host created is refused when [`open`](Self::open) would refuse it.
    pub async fn summary(
        root: &Path,
        id: SessionId,
        owner: Option<(&HostName, Schema)>,
    ) -> Result<SessionSummary, SessionError> {
        let owner = owner.map(|(host, schema)| (host.clone(), schema));
        Self::read_only(root, id, move |db| {
            let summary = db::summary(db)?;
            if let Some((_, schema)) = owner.filter(|(host, _)| summary.host == *host) {
                schema.check(db)?;
            }
            Ok(summary)
        })
        .await
    }

    /// Run `read` against one snapshot of a session that may not be open here.
    pub(super) async fn read_only<T: Send + 'static>(
        root: &Path,
        id: SessionId,
        read: impl FnOnce(&crate::db::Db) -> Result<T, DbError> + Send + 'static,
    ) -> Result<T, SessionError> {
        let path = root.join(id.to_string()).join(DATABASE_FILE);
        blocking(move || {
            if !path.is_file() {
                return Err(SessionError::NotFound(id));
            }
            Ok(Reads::Own(Arc::new(crate::db::Own::new(
                path,
                db::opener(db::OpenMode::ReadOnly),
            )))
            .read(read)?)
        })
        .await
    }

    /// A new session of `host`, holding `schema`.
    pub(crate) async fn create(
        root: &Path,
        host: HostName,
        schema: Schema,
    ) -> Result<Self, SessionError> {
        Self::create_as(root, Opening::Create(host, schema)).await
    }

    #[cfg(test)]
    pub(crate) async fn create_ephemeral(
        root: &Path,
        schema: Schema,
    ) -> Result<Self, SessionError> {
        let host = "test".parse().unwrap();
        Self::create_as(root, Opening::Ephemeral(host, schema)).await
    }

    async fn create_as(root: &Path, opening: Opening) -> Result<Self, SessionError> {
        let root = root.to_owned();
        // One blocking task creates the directory and the store whose close
        // removes it again, so a caller that gives up leaves nothing behind.
        let create = move || {
            std::fs::create_dir_all(&root)?;
            let id = SessionId::generate()?;
            let directory = root.join(id.to_string());
            std::fs::create_dir(&directory)?;
            Self::start(id, directory, opening)
        };
        blocking(create).await
    }

    /// Open a saved session. Given `owner`, one that host created opens with its schema,
    /// checked; another host's session, or any without `owner`, opens with the journal's.
    pub async fn open(
        root: &Path,
        id: SessionId,
        owner: Option<(&HostName, Schema)>,
    ) -> Result<Self, SessionError> {
        let directory = root.join(id.to_string());
        let owner = owner.map(|(host, schema)| (host.clone(), schema));
        blocking(move || Self::start(id, directory, Opening::Existing(owner))).await
    }

    fn start(id: SessionId, directory: PathBuf, opening: Opening) -> Result<Self, SessionError> {
        let existing = matches!(opening, Opening::Existing(_));
        let path = directory.join(DATABASE_FILE);
        let place = match opening {
            Opening::Existing(_) if !path.is_file() => return Err(SessionError::NotFound(id)),
            #[cfg(test)]
            Opening::Ephemeral(..) => crate::db::Place::Memory,
            _ => crate::db::Place::File(&path),
        };
        let durable = matches!(place, crate::db::Place::File(_));
        // Acquire ownership before opening a potentially active database.
        let lock = durable.then(|| lock_session(&directory, id)).transpose()?;
        let log = durable.then(|| Arc::new(db::Log::of(&path)));
        let (db, host, schema) = match opening {
            Opening::Create(host, schema) => (schema.create(place)?, host, schema),
            #[cfg(test)]
            Opening::Ephemeral(host, schema) => (schema.create(place)?, host, schema),
            Opening::Existing(owner) => {
                let db = db::open(&path, db::OpenMode::Open)?;
                let host = db::host(&db)?;
                let schema = owner
                    .filter(|(keeper, _)| *keeper == host)
                    .map_or(Schema::JOURNAL, |(_, schema)| schema);
                schema.check(&db)?;
                (db, host, schema)
            }
        };
        let db = SharedDb::new(db, log);
        // The core reads on a connection of its own, so that it never waits behind
        // other reads.
        let (reads, core_reads) = match durable {
            true => (
                Reads::Pool(Arc::new(crate::db::ReadPool::new(
                    path.clone(),
                    db::opener(db::OpenMode::Reader),
                ))),
                Reads::Own(Arc::new(crate::db::Own::new(
                    path,
                    db::opener(db::OpenMode::Reader),
                ))),
            ),
            false => {
                let reads = Reads::writer(db.clone());
                (reads.clone(), reads)
            }
        };
        let (mut state, observed) = open_state(&db.lock(), id)?;
        // A reopened session runs nothing until something changes it.
        if existing {
            state.set_phase(SessionPhase::Viewing);
        }
        let state = (state, observed);
        let core_journal = Journal {
            session: id,
            reads: core_reads,
        };
        let extension = schema
            .has_product_tables()
            .then(|| ExtensionDb::new(id, directory.clone(), db.clone(), reads.clone()));
        let journal = Journal { session: id, reads };
        let connections = (db.clone(), core_journal);
        let (shared, core) = core::Core::start(id, directory, state, connections, lock)?;
        let inner = Arc::new(StoreInner {
            host,
            shared,
            db,
            journal,
            core,
            extension,
        });
        Ok(Self { inner })
    }

    pub fn id(&self) -> SessionId {
        self.inner.shared.id
    }

    /// The tables of the extension this session was opened with, if any.
    pub fn extension(&self) -> Option<&ExtensionDb> {
        self.inner.extension.as_ref()
    }

    /// The host that created this session; opening it never changes its origin.
    pub fn host(&self) -> &HostName {
        &self.inner.host
    }

    pub fn directory(&self) -> &Path {
        &self.inner.shared.directory
    }

    /// The session's live part and the updates after it, without a gap between
    /// them. It copies nothing of the history, which pages read.
    pub fn observe(&self) -> crate::session::Observation {
        let shared = &self.inner.shared;
        // The core publishes only under the state's write lock, and posts take
        // only `observed`: subscribing under both, then copying the committed
        // state before the read lock drops, leaves every update either in the
        // snapshot or after it.
        let state = shared.read();
        let (posted, updates) = {
            let mut observed = shared.observed();
            (observed.posted(), shared.events.subscribe())
        };
        let mut snapshot = state.snapshot(posted);
        drop(state);
        snapshot.read_phase();
        crate::session::Observation { snapshot, updates }
    }

    /// The committed journal, read by index.
    pub(crate) fn journal(&self) -> Journal {
        self.inner.journal.clone()
    }

    /// The revision of the newest update published to observers.
    pub fn revision(&self) -> u64 {
        self.inner.shared.observed().revision()
    }

    /// Fold and publish an update the journal does not carry. Nothing the core
    /// plans reads these views, so they wait neither behind its commands nor
    /// behind readers of its state, only on the observation's lock: a commit holds
    /// it from its records' fold into these views to their publication, after COMMIT.
    pub(crate) fn post(&self, event: crate::session::RuntimeEvent) {
        self.inner.shared.post(event);
    }

    /// Fold `event`, streamed for `agent`'s `request`, as [`post`](Self::post) does.
    /// Observers are told the response changed once until they
    /// [read it](Self::response), however much streams meanwhile.
    pub(crate) fn stream(
        &self,
        agent: &AgentId,
        request: RequestSeq,
        event: crate::provider::protocol::ResponseEvent,
    ) {
        self.inner.shared.stream((agent.clone(), request), event);
    }

    /// `agent`'s `request` as it streamed so far, which an observer reads when told
    /// it changed: what streams next is news again. None once the response is gone,
    /// as its commit or the agent's next request lets go of it, which updates of its
    /// own tell observers.
    pub fn response(
        &self,
        agent: &AgentId,
        request: RequestSeq,
    ) -> Option<crate::session::ObservedResponse> {
        let request = (agent.clone(), request);
        self.inner.shared.observed().response(&request)
    }

    /// Read the core's state.
    pub(crate) fn read<T>(&self, read: impl FnOnce(&CoreState) -> T) -> T {
        read(&self.inner.shared.read())
    }

    /// Append one event; dropping this waiter does not cancel persistence.
    pub(crate) async fn append(
        &self,
        agent: AgentId,
        event: SessionEvent,
    ) -> Result<Arc<EventRecord>, SessionError> {
        Ok(self.append_all(vec![(agent, event)]).await?.remove(0))
    }

    /// Append several events committed in one transaction.
    pub(crate) async fn append_all(
        &self,
        events: Vec<(AgentId, SessionEvent)>,
    ) -> Result<Vec<Arc<EventRecord>>, SessionError> {
        let records = |_: &mut CoreState, records: &[Arc<EventRecord>]| records.to_vec();
        let plan = move |_: &CoreState| Ok(Plan::new(events, records));
        self.commit(Command::Journal, plan).await
    }

    /// Run `plan` on the core thread against the state it commits to, if the
    /// session's phase admits `command`; dropping this waiter does not cancel it.
    pub(crate) async fn commit<T: Send + 'static, E: From<SessionError> + Send + 'static>(
        &self,
        command: Command,
        plan: impl FnOnce(&CoreState) -> Result<Plan<T>, E> + Send + 'static,
    ) -> Result<T, E> {
        self.commit_on(None, command, plan).await
    }

    /// [`commit`](Self::commit) a plan that reads `jobs` by id: each is held first,
    /// as the journal leaves it, if it was ever created.
    pub(crate) async fn commit_on<T: Send + 'static, E: From<SessionError> + Send + 'static>(
        &self,
        jobs: impl IntoIterator<Item = JobId>,
        command: Command,
        plan: impl FnOnce(&CoreState) -> Result<Plan<T>, E> + Send + 'static,
    ) -> Result<T, E> {
        let run = move |writer: &mut core::Writer| writer.run(command, plan);
        self.inner.core.call(held(jobs, run)).await?
    }

    /// Change facts the journal does not record, in order with every commit.
    pub(crate) async fn live<T: Send + 'static>(
        &self,
        change: impl FnOnce(&mut CoreState) -> T + Send + 'static,
    ) -> T {
        let live = move |writer: &mut core::Writer| writer.change(change);
        let changed = self.inner.core.call(live).await;
        changed.expect("only a panicking change fails")
    }

    /// Change facts the journal does not record, behind every command queued
    /// before it, without waiting for the change.
    pub(crate) fn enqueue(&self, change: impl FnOnce(&mut CoreState) + Send + 'static) {
        self.inner.core.post(move |writer| writer.change(change));
    }

    /// Resolve once `wait` decides, as it reads the state when registered and
    /// after every change to what it declares, consuming what it decides on as
    /// it decides: its waiter must lose nothing by abandoning the reply. Refused
    /// as closed, or with its failure, once the session decides nothing more.
    /// The jobs it declares are held as it registers, so it first decides on
    /// their facts; one let go after is as it was then.
    pub(crate) async fn until<T: Send + 'static>(&self, wait: Wait<T>) -> Result<T, SessionError> {
        self.register(wait.jobs(), |_| (), |()| wait).await
    }

    /// [`until`](Self::until) the wait `wait` builds from what `first` returns,
    /// registered in the change that applies `first`, so it and every other wait
    /// first read the state as `first` leaves it, which holds the jobs it reads.
    pub(crate) async fn until_after<F: Send + 'static, T: Send + 'static>(
        &self,
        first: impl FnOnce(&mut CoreState) -> F + Send + 'static,
        wait: impl FnOnce(F) -> Wait<T> + Send + 'static,
    ) -> Result<T, SessionError> {
        self.register(Vec::new(), first, wait).await
    }

    /// Hold `jobs`, then register the wait `wait` builds from what `first` returns.
    async fn register<F: Send + 'static, T: Send + 'static>(
        &self,
        jobs: Vec<JobId>,
        first: impl FnOnce(&mut CoreState) -> F + Send + 'static,
        wait: impl FnOnce(F) -> Wait<T> + Send + 'static,
    ) -> Result<T, SessionError> {
        let (reply, outcome) = tokio::sync::oneshot::channel();
        let register = move |writer: &mut core::Writer| {
            writer.change(|state| {
                let first = first(state);
                state.wait(wait(first), reply);
            });
            Ok::<_, SessionError>(())
        };
        self.inner.core.call(held(jobs, register)).await??;
        match outcome.await {
            Ok(outcome) => Ok(outcome),
            Err(_) => Err(self.behind(|state| state.phase().refusal()).await),
        }
    }

    /// Queues changes on this session's core without keeping it open.
    pub(crate) fn weak_core(&self) -> core::WeakCore {
        self.inner.core.downgrade()
    }

    pub(crate) fn downgrade(&self) -> WeakStore {
        WeakStore(Arc::downgrade(&self.inner))
    }

    /// Read the state behind the command that dropped a reply, which may not have
    /// moved the phase yet, as a command that panics drops it first; as it
    /// stands once the core has stopped.
    pub(crate) async fn behind<T: Send + 'static>(&self, read: fn(&CoreState) -> T) -> T {
        let behind = move |writer: &mut core::Writer| read(&writer.shared.read());
        let stopped = self.inner.core.call(behind).await;
        stopped.unwrap_or_else(|_| self.read(read))
    }

    /// Await accepted appends and report the failure that stopped the session,
    /// if its journal failed, without closing admission.
    pub(crate) async fn drain(&self) -> Result<(), SessionError> {
        // Its reply follows the sync of every commit queued before it.
        self.inner.core.call(|_| ()).await?;
        self.read(|state| state.phase().failed())
    }

    /// The token whose cancellation stops everything the session runs, as its
    /// journal failing does.
    pub(crate) fn root(&self) -> crate::job::CancellationToken {
        self.inner.shared.root.clone()
    }

    /// Close behind every command queued before this, without waiting.
    pub(crate) fn close_soon(&self) {
        self.inner.core.post(|writer| drop(writer.close()));
    }

    /// Stop admission, wait out operations already queued, release the session
    /// for another open, and report the failure that stopped the session, if its
    /// journal failed. Retained handles, including output streams, can still read
    /// but can no longer write.
    pub async fn close(&self) -> Result<(), SessionError> {
        self.inner.core.call(core::Writer::close).await?
    }

    /// The title the session list shows: see [`SessionSummary::title`].
    pub async fn title(&self) -> Result<Option<SessionTitle>, SessionError> {
        self.snapshot(|db| Ok(db::title(db)?)).await
    }

    /// Run `read` against one snapshot of the committed session, off the async workers.
    pub(super) async fn snapshot<T: Send + 'static>(
        &self,
        read: impl FnOnce(&crate::db::Db) -> Result<T, SessionError> + Send + 'static,
    ) -> Result<T, SessionError> {
        let reads = self.reads();
        self.reads().hold(move || reads.read(read)).await
    }

    /// The entries that created those of `jobs` that `agent` created, with their
    /// creations, in id order.
    pub(crate) async fn job_creations(
        &self,
        jobs: Vec<JobId>,
        agent: &AgentId,
    ) -> Result<Vec<(RecordSeq, SessionEvent)>, SessionError> {
        let session = self.id();
        let read = move |db: &crate::db::Db, row| db::creations(db, session, &jobs, row);
        self.read_agent(agent, Vec::new(), read).await
    }

    /// Record where `run` keeps files of its output: on the machine its connection
    /// reached along `route`, each target on it at the revision that connection was
    /// admitted at, none for the session host. The files in `spilled`, which that
    /// machine created in its spill directory, are the session's own from then on,
    /// in this and every later opening. Rows the journal cannot take stop the
    /// session, as an append does.
    pub(crate) async fn keep(
        &self,
        run: crate::job::Run,
        route: Revisions,
        spilled: Vec<SpillFile>,
    ) -> Result<(), SessionError> {
        let keep = move |writer: &mut core::Writer| {
            writer.shared.read().phase().admits(Command::Journal)?;
            writer.write(|db, _| Ok(db::keep(db, run, &route, &spilled)?))?;
            writer.change(|state| {
                for file in spilled {
                    state.spills.own(route.clone(), file.components);
                }
            });
            Ok(())
        };
        self.inner.core.call(keep).await?
    }

    /// The connection job output streams write through.
    pub(crate) fn outputs(&self) -> SharedDb {
        self.inner.db.clone()
    }

    /// Where reads that must not wait on the writer run.
    pub(crate) fn reads(&self) -> Reads {
        self.inner.journal.reads.clone()
    }

    /// What output streams tell observers through.
    pub(crate) fn output_changes(&self) -> OutputChanges {
        OutputChanges(Arc::downgrade(&self.inner.shared))
    }
}
