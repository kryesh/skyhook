//! The jobs the core holds, and the indexes that find the few a change or a
//! request reads. Every change to a job's facts goes through [`Jobs::modify`],
//! which keeps the indexes and records what it touched for the waits that read it.
//!
//! Only the resident set is held: jobs a lifecycle rule can still change without
//! being addressed by id, and the ancestors of those. Each job leaves once a change
//! settles it, and is read back from its status row by id when addressed again.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use super::*;
use crate::session::Dirty;

/// The resident jobs, with the live facts the core keeps beside them.
#[derive(Default)]
pub(crate) struct Jobs {
    entries: HashMap<JobId, JobFacts>,
    index: Index,
    /// The job each launched child agent answers through, held or not: one per child.
    launches: HashMap<AgentId, JobId>,
    /// Per owner, the job whose installed child holds each name, whatever its end:
    /// one per child.
    names: HashMap<AgentId, HashMap<JobName, JobId>>,
    /// The highest id created so far.
    last: u64,
    pub(super) stamps: Stamps,
    calls: Calls,
    dirty: Dirty,
    /// Jobs whose status changed since the last take, which their transaction writes.
    changed: BTreeSet<JobId>,
    /// Jobs a change may have settled, which the next sweep evicts unless a lifecycle
    /// rule or a held descendant keeps them.
    leaving: Vec<JobId>,
    /// Jobs evicted since the last take.
    evicted: Vec<JobId>,
    /// Jobs evicted with a live-only cancellation request their restoring would not
    /// imply: the session remembers it, a reopen forgets it.
    requested: HashSet<JobId>,
}

/// Where a job's facts are: held, created but let go with `U`, or never created. A
/// job is let go only once nothing changes it without addressing it, and addressing
/// it holds it again first, so one let go is as it was last held.
pub(super) enum Lookup<H, U = ()> {
    Held(H),
    Unheld(U),
    Unknown,
}

/// What the job folds count across the whole journal, which an open that reads only
/// the resident jobs' rows reads by index instead.
pub(crate) struct JobSeeds {
    pub(crate) last: u64,
    /// Every launched child, the job it answers through, that job's owner and name,
    /// in job order.
    pub(crate) launches: Vec<(AgentId, JobId, AgentId, Option<JobName>)>,
    /// Per owner, the latest response it holds jobs of, and how many jobs that
    /// response's calls created.
    pub(crate) ended: Vec<(AgentId, MessageSeq, usize)>,
}

/// Live only: model and script calls between their issue and their outcome, as
/// a `wait` beside them needs to know them.
#[derive(Default)]
struct Calls {
    /// Per parent job, its issued calls whose jobs are not published yet.
    issuing: HashMap<JobId, usize>,
    /// Per agent, its latest response with a call that ended without a job: a
    /// response's calls all end before the next one commits.
    jobless: HashMap<AgentId, MessageSeq>,
    /// Per agent, its latest response with jobs, and how many jobs it created.
    ended: HashMap<AgentId, Ended>,
}

struct Ended {
    response: MessageSeq,
    jobs: usize,
}

/// What the indexes hold of one job: where it stands with its owner, unless
/// settled, and since when it has had something pending for it.
#[derive(PartialEq, Eq)]
struct Footprint([Option<Facet>; 2]);

/// How a job concerns its owner, in the order an owner's index holds them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Facet {
    /// It can still run.
    Live,
    /// Interrupted with nothing to resume it: settled, but cancellation still
    /// changes its outcome.
    Dormant,
    /// It has something pending for its owner, the newest since this stamp.
    Pending(u64),
}

impl Footprint {
    fn of(entry: &JobFacts) -> Self {
        let standing = match (entry.live(), entry.cancellable()) {
            (true, _) => Some(Facet::Live),
            (false, true) => Some(Facet::Dormant),
            (false, false) => None,
        };
        let pending = entry.child().and_then(Child::queued);
        let pending = pending.max(entry.lifecycle_pending()).map(Facet::Pending);
        Self([standing, pending])
    }
}

#[derive(Default)]
#[cfg_attr(debug_assertions, derive(Debug, PartialEq))]
struct Index {
    /// Each owner's jobs that a change or a request reads, by facet. Ordered
    /// rather than hashed: a session has few owners, and comparing agents is far
    /// cheaper than hashing one.
    owned: BTreeMap<AgentId, BTreeMap<Facet, BTreeSet<JobId>>>,
    children: HashMap<JobId, BTreeSet<JobId>>,
}

impl Index {
    fn add(&mut self, id: JobId, owner: &AgentId, footprint: &Footprint) {
        let mut facets = footprint.0.iter().flatten().peekable();
        if facets.peek().is_none() {
            return;
        }
        let owned = self.owned.entry(owner.clone()).or_default();
        for facet in facets {
            owned.entry(*facet).or_default().insert(id);
        }
    }

    fn remove(&mut self, id: JobId, owner: &AgentId, footprint: &Footprint) {
        if let Some(owned) = self.owned.get_mut(owner) {
            for facet in footprint.0.iter().flatten() {
                if let Some(ids) = owned.get_mut(facet) {
                    ids.remove(&id);
                    if ids.is_empty() {
                        owned.remove(facet);
                    }
                }
            }
            if owned.is_empty() {
                self.owned.remove(owner);
            }
        }
    }

    fn adopt(&mut self, parent: Option<JobId>, id: JobId) {
        if let Some(parent) = parent {
            self.children.entry(parent).or_default().insert(id);
        }
    }

    /// `owner`'s jobs of `facets`, in facet order, then in id order.
    fn faceted(
        &self,
        owner: &AgentId,
        facets: impl std::ops::RangeBounds<Facet>,
    ) -> impl Iterator<Item = JobId> {
        let owned = self.owned.get(owner).map(|owned| owned.range(facets));
        owned
            .into_iter()
            .flatten()
            .flat_map(|(_, ids)| ids)
            .copied()
    }
}

impl Jobs {
    pub(super) fn get(&self, id: JobId) -> Option<&JobFacts> {
        self.entries.get(&id)
    }

    pub(super) fn contains(&self, id: JobId) -> bool {
        self.entries.contains_key(&id)
    }

    /// Every held job: only explicit reads of the resident set, such as what
    /// reopening settles, visit them all.
    pub(super) fn iter(&self) -> impl Iterator<Item = (JobId, &JobFacts)> {
        self.entries.iter().map(|(id, entry)| (*id, entry))
    }

    /// Add job `id`, which its creation record describes.
    pub(super) fn create(&mut self, id: JobId, entry: JobFacts) {
        self.last = self.last.max(id.get());
        if let Some(response) = entry.origin.as_ref().map(|origin| origin.message) {
            let ended = self.calls.ended.entry(entry.agent.clone());
            let ended = ended.or_insert(Ended { response, jobs: 0 });
            if ended.response < response {
                *ended = Ended { response, jobs: 0 };
            }
            ended.jobs += usize::from(ended.response == response);
        }
        self.changed.insert(id);
        self.hold(id, entry);
    }

    /// Hold job `id`, read back from its status row as the fold left it, until a sweep
    /// finds nothing keeps it. Its parent is held first: a requested cancellation
    /// lives only while held, so the one its own end or its parent's implies, or its
    /// eviction remembered, is requested again.
    pub(super) fn restore(&mut self, id: JobId, mut entry: JobFacts) {
        let cancelled = self.requested.remove(&id) | self.cancel_implied(&entry);
        if cancelled && entry.cancel == CancelState::Open {
            entry.cancel = CancelState::Requested { watched: None };
        }
        self.leaving.push(id);
        self.hold(id, entry);
    }

    /// Whether `entry`'s own end or its parent's request implies its cancellation.
    fn cancel_implied(&self, entry: &JobFacts) -> bool {
        let parent = entry.parent.and_then(|parent| self.get(parent));
        entry.end() == Some(JobEnd::Cancelled) || parent.is_some_and(JobFacts::cancel_requested)
    }

    fn hold(&mut self, id: JobId, entry: JobFacts) {
        self.index.adopt(entry.parent, id);
        self.index.add(id, &entry.agent, &Footprint::of(&entry));
        self.dirty.job(id);
        self.dirty.agent(&entry.agent);
        self.entries.insert(id, entry);
    }

    /// Change job `id`'s facts with `change`, given the stamps that order what
    /// becomes pending, keeping every index and marking what it touched: the
    /// job, its owner, and the child it launched. A job holds handles only
    /// while it can still run.
    pub(super) fn modify<T>(
        &mut self,
        id: JobId,
        change: impl FnOnce(&mut JobFacts, &Stamps) -> T,
    ) -> Option<T> {
        let entry = self.entries.get_mut(&id)?;
        let (before, status) = (Footprint::of(entry), JobStatus::of(entry));
        let changed = change(entry, &self.stamps);
        if entry.end().is_some() && entry.restart().is_none() {
            entry.handles = None;
        }
        if JobStatus::of(entry) != status {
            self.changed.insert(id);
        }
        let after = Footprint::of(entry);
        self.dirty.job(id);
        self.dirty.agent(&entry.agent);
        if let Some(child) = entry.child().and_then(|child| child.agent.as_ref()) {
            self.dirty.agent(child);
        }
        if !entry.resident() {
            self.leaving.push(id);
        }
        if before != after {
            self.index.remove(id, &entry.agent, &before);
            self.index.add(id, &entry.agent, &after);
        }
        Some(changed)
    }

    /// Evict every job a change left that no lifecycle rule keeps resident and
    /// no held job descends from: its facts, indexes and handles.
    pub(crate) fn sweep(&mut self) {
        while let Some(id) = self.leaving.pop() {
            let kept = |entry: &JobFacts| entry.resident() || self.index.children.contains_key(&id);
            if self.get(id).is_none_or(kept) {
                continue;
            }
            let entry = self.entries.remove(&id).expect("a held job");
            if matches!(entry.cancel, CancelState::Requested { .. }) && !self.cancel_implied(&entry)
            {
                self.requested.insert(id);
            }
            self.index.remove(id, &entry.agent, &Footprint::of(&entry));
            if let Some(parent) = entry.parent
                && let Some(children) = self.index.children.get_mut(&parent)
            {
                children.remove(&id);
                if children.is_empty() {
                    self.index.children.remove(&parent);
                    self.leaving.push(parent);
                }
            }
            self.evicted.push(id);
        }
    }

    /// Report each job `old` held that these do not as evicted.
    pub(crate) fn let_go(&mut self, old: &Jobs) {
        let gone = old
            .entries
            .keys()
            .filter(|id| !self.entries.contains_key(id));
        self.evicted.extend(gone);
    }

    /// The jobs the sweeps since the last take evicted.
    pub(crate) fn take_evicted(&mut self) -> Vec<JobId> {
        let mut evicted = std::mem::take(&mut self.evicted);
        evicted.sort_unstable();
        evicted
    }

    /// The status of each job that changed since the last take, as it is now.
    pub(crate) fn take_changed(&mut self) -> Vec<(JobId, JobStatus)> {
        let changed = std::mem::take(&mut self.changed).into_iter();
        let status = |id| Some((id, JobStatus::of(self.get(id)?)));
        changed.filter_map(status).collect()
    }

    /// Whether every status change has been taken.
    pub(crate) fn written(&self) -> bool {
        self.changed.is_empty()
    }

    /// The held jobs' facts.
    #[cfg(debug_assertions)]
    pub(super) fn entries(self) -> HashMap<JobId, JobFacts> {
        self.entries
    }

    /// Hold what an open reads in place of folding the whole journal: the `resident`
    /// jobs with their ancestors, and what the folds count across it.
    pub(crate) fn open(&mut self, seeds: JobSeeds, resident: Settled) {
        let JobSeeds {
            last,
            launches,
            ended,
        } = seeds;
        self.last = last;
        for (child, job, owner, name) in launches {
            self.record_launch(child, job, owner, name);
        }
        let ended =
            (ended.into_iter()).map(|(owner, response, jobs)| (owner, Ended { response, jobs }));
        self.calls.ended.extend(ended);
        (resident.into_iter()).for_each(|(id, entry)| self.hold(id, entry));
    }

    /// Record that agent job `job` launched `child`, unless it launched another.
    pub(super) fn launch(&mut self, job: JobId, child: &AgentId) {
        let launched = self.modify(job, |entry, _| {
            let (owner, name) = (entry.agent.clone(), entry.name.clone());
            let installed = (child.parent().as_ref() == Some(&owner))
                .then(|| entry.child_mut())
                .flatten()
                .filter(|launched| launched.agent.as_ref().is_none_or(|agent| agent == child));
            installed.map(|launched| {
                launched.agent = Some(child.clone());
                (owner, name)
            })
        });
        if let Some((owner, name)) = launched.flatten() {
            self.record_launch(child.clone(), job, owner, name);
            self.dirty.agent(child);
        }
    }

    /// `child` answers through `job`, which keeps its name for `owner` unless an
    /// earlier launch holds it.
    fn record_launch(&mut self, child: AgentId, job: JobId, owner: AgentId, name: Option<JobName>) {
        self.launches.insert(child, job);
        if let Some(name) = name {
            self.names
                .entry(owner)
                .or_default()
                .entry(name)
                .or_insert(job);
        }
    }

    /// The job `child` answers through, held or not.
    pub(super) fn launched(&self, child: &AgentId) -> Option<JobId> {
        self.launches.get(child).copied()
    }

    /// The job `child` answers through, while that can still run it.
    pub(crate) fn resumes(&self, child: &AgentId) -> Option<JobId> {
        let job = self.launched(child)?;
        (self.get(job)?.restart() == Some(Restart::Child)).then_some(job)
    }

    /// Whether `owner` owns a job that is live, interrupted or pending.
    pub(crate) fn owns_work(&self, owner: &AgentId) -> bool {
        self.index.owned.contains_key(owner)
    }

    fn held(&self, ids: impl Iterator<Item = JobId>) -> impl Iterator<Item = (JobId, &JobFacts)> {
        ids.map(|id| (id, &self.entries[&id]))
    }

    /// `owner`'s live jobs, in id order.
    pub(super) fn live(&self, owner: &AgentId) -> impl Iterator<Item = (JobId, &JobFacts)> {
        self.held(self.index.faceted(owner, Facet::Live..=Facet::Live))
    }

    /// `owner`'s unfinished jobs, in id order.
    pub(super) fn unfinished(&self, owner: &AgentId) -> impl Iterator<Item = (JobId, &JobFacts)> {
        self.live(owner).filter(|(_, entry)| entry.end().is_none())
    }

    /// The owners of jobs that are live, interrupted, or pending.
    pub(super) fn owners(&self) -> impl Iterator<Item = &AgentId> {
        self.index.owned.keys()
    }

    /// Every owner's unfinished jobs.
    pub(super) fn running(&self) -> impl Iterator<Item = (JobId, &JobFacts)> {
        self.owners().flat_map(|owner| self.unfinished(owner))
    }

    /// `owner`'s jobs whose outcome a cancellation changes: its live jobs, then its
    /// dormant ones, each in id order.
    pub(super) fn cancellable(&self, owner: &AgentId) -> impl Iterator<Item = (JobId, &JobFacts)> {
        self.held(self.index.faceted(owner, Facet::Live..=Facet::Dormant))
    }

    /// `owner`'s jobs with something pending for it, in the order they became so.
    pub(super) fn pending(&self, owner: &AgentId) -> impl Iterator<Item = (JobId, &JobFacts)> {
        self.held(self.index.faceted(owner, Facet::Pending(0)..))
    }

    /// When the newest of what `owner` has pending became pending.
    pub(super) fn newest_pending(&self, owner: &AgentId) -> Option<u64> {
        match self.index.owned.get(owner)?.last_key_value()?.0 {
            Facet::Pending(stamp) => Some(*stamp),
            Facet::Live | Facet::Dormant => None,
        }
    }

    /// The held jobs created under `parent`: every one a lifecycle rule can still
    /// change, as a held job's ancestors are held too.
    pub(super) fn children(&self, parent: JobId) -> impl Iterator<Item = JobId> + '_ {
        self.index
            .children
            .get(&parent)
            .into_iter()
            .flatten()
            .copied()
    }

    /// `roots` and every held job below them, each once.
    pub(super) fn trees(&self, mut worklist: Vec<JobId>) -> Vec<JobId> {
        let mut visited = HashSet::new();
        let mut trees = Vec::new();
        while let Some(job) = worklist.pop() {
            if visited.insert(job) {
                worklist.extend(self.children(job));
                trees.push(job);
            }
        }
        trees
    }

    /// How many jobs the calls of `owner`'s `response` launched are no longer live,
    /// while it is `owner`'s latest response with jobs: a live job is held.
    pub(super) fn ended_from(&self, owner: &AgentId, response: MessageSeq) -> usize {
        let Some(ended) = (self.calls.ended.get(owner)).filter(|ended| ended.response == response)
        else {
            return 0;
        };
        let of = |(_, entry): &(JobId, &JobFacts)| {
            (entry.origin.as_ref()).is_some_and(|origin| origin.message == response)
        };
        ended
            .jobs
            .saturating_sub(self.live(owner).filter(of).count())
    }

    /// The job of `owner` whose installed child holds `name`.
    pub(super) fn named(&self, owner: &AgentId, name: &JobName) -> Option<JobId> {
        self.names.get(owner)?.get(name).copied()
    }

    /// The held jobs `keep` selects, in id order: a scan of the resident set, which
    /// only session-wide commands (retry, interrupt, continue) make.
    pub(super) fn select(&self, keep: impl Fn(&JobFacts) -> bool) -> Vec<(JobId, &JobFacts)> {
        let mut jobs: Vec<_> = self.iter().filter(|(_, entry)| keep(entry)).collect();
        jobs.sort_unstable_by_key(|(id, _)| *id);
        jobs
    }

    /// Whether job `id` was ever created, held or not.
    pub(super) fn created(&self, id: JobId) -> bool {
        id.get() <= self.last
    }

    /// Where job `id`'s facts are.
    pub(super) fn lookup(&self, id: JobId) -> Lookup<&JobFacts> {
        match self.get(id) {
            Some(entry) => Lookup::Held(entry),
            None if self.created(id) => Lookup::Unheld(()),
            None => Lookup::Unknown,
        }
    }

    /// Those of `jobs` that were created but are not held.
    pub(crate) fn unheld(&self, jobs: impl IntoIterator<Item = JobId>) -> Vec<JobId> {
        let unheld = jobs
            .into_iter()
            .filter(|job| self.created(*job) && !self.contains(*job));
        let mut unheld: Vec<_> = unheld.collect();
        unheld.sort_unstable();
        unheld.dedup();
        unheld
    }

    /// The jobs `events` change once created, which committing them holds first.
    pub(crate) fn addressed(
        &self,
        events: &[(AgentId, SessionEvent)],
    ) -> impl Iterator<Item = JobId> {
        let launched = |child: &AgentId| self.launched(child);
        (events.iter()).filter_map(move |(agent, event)| addressed(agent, event, launched))
    }

    /// The ids the next `count` created jobs take, in order.
    pub(super) fn next_ids(&self, count: usize) -> Result<Vec<JobId>, JobError> {
        let end = (self.last.checked_add(count as u64)).ok_or(JobError::InvalidId)?;
        Ok((self.last..end)
            .map(|id| JobId::new(id + 1).expect("ids after the last are nonzero"))
            .collect())
    }

    /// What every change since the last take touched.
    pub(crate) fn take_dirty(&mut self) -> Dirty {
        std::mem::take(&mut self.dirty)
    }

    /// The indexes are what the facts they index make them, and only a job
    /// that can still run holds handles.
    #[cfg(debug_assertions)]
    pub(crate) fn assert_indexed(&self) {
        let mut rebuilt = Index::default();
        for (&id, entry) in &self.entries {
            assert!(
                entry.handles.is_none() || entry.end().is_none() || entry.restart().is_some(),
                "job {id:?} holds handles though it cannot run"
            );
            rebuilt.adopt(entry.parent, id);
            rebuilt.add(id, &entry.agent, &Footprint::of(entry));
        }
        assert_eq!(
            self.index, rebuilt,
            "a job's facts changed without its indexes"
        );
    }

    pub(super) fn issue(&mut self, parent: JobId) {
        *self.calls.issuing.entry(parent).or_default() += 1;
    }

    pub(super) fn issued(&mut self, parent: JobId) {
        if let Some(count) = self.calls.issuing.get_mut(&parent) {
            *count -= 1;
            if *count == 0 {
                self.calls.issuing.remove(&parent);
                self.dirty.job(parent);
            }
        }
    }

    /// Whether every call `parent` issued has its job published.
    pub(super) fn published(&self, parent: JobId) -> Option<()> {
        (!self.calls.issuing.contains_key(&parent)).then_some(())
    }

    /// Record that a call of `agent`'s response committed at `response` ended
    /// without a job.
    pub(super) fn settled_jobless(&mut self, agent: AgentId, response: MessageSeq) {
        self.dirty.agent(&agent);
        self.calls.jobless.insert(agent, response);
    }

    /// The latest response of `agent` with a call that ended without a job.
    pub(super) fn jobless(&self, agent: &AgentId) -> Option<MessageSeq> {
        self.calls.jobless.get(agent).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `continue` and input restart an interruption while something can.
    /// Without a restarter, as a session without its agent runtime has, neither
    /// restarts it, but a cancellation still ends it.
    #[tokio::test]
    async fn an_interruption_restarts_only_while_its_handler_can() {
        let crate::tests::TestRuntime {
            root, jobs, agent, ..
        } = crate::tests::TestRuntime::on_disk().await;
        let spec = JobSpec {
            accepts_input: true,
            role: JobRole::Agent,
            ..JobSpec::test(agent.clone(), "agent")
        };
        let id = jobs.test_running(spec).await.into_test_id();
        let handler: ResumeHandler = Arc::new(|_, mut input| {
            Box::pin(async move {
                input.recv().await;
                Ok(ToolOutput::default())
            })
        });
        jobs.test_resume_with(id, handler);
        let interrupted = async |jobs: &JobManager| {
            let interrupted = ToolError::interrupted().into();
            jobs.finish(id, interrupted).await.unwrap();
        };
        let continued = async |jobs: &JobManager| {
            let manager = jobs.clone();
            let plan = move |state: &CoreState| {
                let (running, restart) = manager.continue_resumable(state);
                Ok::<_, SessionError>(Plan::new(running, move |state, _| restart(state)))
            };
            jobs.store().commit(Command::Continue, plan).await.unwrap()
        };
        let running = async |jobs: &JobManager| {
            assert_eq!(jobs.snapshot(id).await.unwrap().state, JobState::Running);
        };
        interrupted(&jobs).await;
        assert_eq!(continued(&jobs).await, 1);
        running(&jobs).await;
        interrupted(&jobs).await;
        jobs.send(id, Value::Null).await.unwrap();
        running(&jobs).await;
        interrupted(&jobs).await;
        let restored = super::super::tests::reopen(jobs, root.path()).await;
        assert_eq!(continued(&restored).await, 0);
        assert!(matches!(
            restored.send(id, Value::Null).await,
            Err(JobError::InputUnavailable {
                reason: InputUnavailableReason::ResumeUnavailable,
                ..
            })
        ));
        assert_eq!(
            restored.snapshot(id).await.unwrap().state,
            JobState::Interrupted
        );
        assert_eq!(restored.cancel_all(&agent, CancelScope::Outcome).await, 1);
    }
}
