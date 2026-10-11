//! What the core folds from the journal: the one in-memory copy of session state.
use std::collections::BTreeMap;

use super::{EventRecord, JournalFailure, SessionError, SessionEvent, SessionPhase};
use crate::{
    agent::{
        ObservationSnapshot, ObservedEvent, RuntimeEvent,
        todo::{self, AgentTodos},
    },
    identity::{AgentId, JobId},
    job::Jobs,
    session::{RecordSeq, RequestLedger},
};

#[derive(Default)]
pub(crate) struct CoreState {
    pub(crate) phase: SessionPhase,
    pub(crate) records: Vec<EventRecord>,
    /// Every request's lifecycle.
    pub(crate) ledger: RequestLedger,
    pub(crate) todos: BTreeMap<AgentId, AgentTodos>,
    pub(crate) jobs: Jobs,
    /// Never journaled: the agents whose loops run.
    pub(crate) agents: crate::agent::Agents,
    pub(crate) targets: crate::target::TargetRegistry,
    /// Whether the session was ever given a title.
    pub(crate) titled: bool,
    /// Registered predicates, never journaled: each resolves by replying, or goes
    /// once nobody waits for it.
    waits: Vec<Waiter>,
    /// What the change being applied does once its critical section ends. Effects
    /// carry futures that are not `Sync`, such as an agent loop's provider context:
    /// the lock keeps the state `Sync`, and only `get_mut` reaches it.
    effects: std::sync::Mutex<Vec<Effect>>,
}

/// A registered wait: true once it replied or its waiter gave up.
type Waiter = Box<dyn FnMut(&mut CoreState) -> bool + Send + Sync>;

/// A non-blocking step a change takes outside the critical section that applies it.
pub(crate) type Effect = Box<dyn FnOnce() + Send>;

/// What observers see beside the committed state: the views the runtime posts
/// without journaling them, as streaming deltas, and the revision every
/// publication takes.
#[derive(Default)]
pub(in crate::session) struct Observed {
    responses: crate::agent::Responses,
    context: crate::agent::ContextGauge,
    /// The newest record applied; journal sequences have no gaps.
    applied: RecordSeq,
    /// Counts publications, so an observer applies each one once.
    revision: u64,
}

impl CoreState {
    /// Replay a journal, refusing a transition the state it reaches does not admit.
    pub(in crate::session) fn replay(
        records: &[EventRecord],
    ) -> Result<(Self, Observed), SessionError> {
        let (mut state, mut observed) = <(Self, Observed)>::default();
        for record in records {
            state.admit(std::slice::from_ref(record))?;
            state.apply(&mut observed, record);
        }
        Ok((state, observed))
    }

    /// Refuse records whose transitions the state, as each leaves it, does not admit.
    pub(super) fn admit(&self, records: &[EventRecord]) -> Result<(), SessionError> {
        match self.jobs.refused(records) {
            Some(sequence) => Err(SessionError::IllegalTransition { sequence }),
            None => Ok(()),
        }
    }

    /// The records committed after `sequence`.
    pub(crate) fn records_after(&self, sequence: RecordSeq) -> &[EventRecord] {
        let start = (self.records).partition_point(|record| record.sequence <= sequence);
        &self.records[start..]
    }

    /// The sequence the next record takes.
    pub(crate) fn next_sequence(&self) -> RecordSeq {
        (self.records.last()).map_or(RecordSeq::new(1), |record| record.sequence.next())
    }

    /// Fold one committed record: the same for replay and for live commits.
    /// Returns the job whose lifecycle it moved.
    pub(super) fn apply(&mut self, observed: &mut Observed, record: &EventRecord) -> Option<JobId> {
        let views = crate::agent::Views {
            ledger: &mut self.ledger,
            responses: &mut observed.responses,
            context: &mut observed.context,
        };
        views.record(record);
        observed.applied = record.sequence;
        todo::apply(&mut self.todos, record);
        let job = self.jobs.apply(record);
        self.targets.apply(&record.event);
        self.agents.observe(record);
        self.titled |= matches!(record.event, SessionEvent::TitleSet { .. });
        self.records.push(record.clone());
        job
    }

    /// Run `effect` once the change being applied leaves its critical section,
    /// after the effects deferred before it.
    pub(crate) fn defer(&mut self, effect: impl FnOnce() + Send + 'static) {
        self.effects().push(Box::new(effect));
    }

    fn effects(&mut self) -> &mut Vec<Effect> {
        self.effects
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Register `resolve`, replying on `reply` once it decides.
    pub(in crate::session) fn wait<T: Send + 'static>(
        &mut self,
        mut resolve: impl FnMut(&mut CoreState) -> Option<T> + Send + Sync + 'static,
        reply: tokio::sync::oneshot::Sender<T>,
    ) {
        if matches!(self.phase, SessionPhase::Closed | SessionPhase::Failed(_)) {
            return;
        }
        let mut reply = Some(reply);
        self.waits.push(Box::new(move |state| {
            if reply.as_ref().is_none_or(|reply| reply.is_closed()) {
                return true;
            }
            let Some(outcome) = resolve(state) else {
                return false;
            };
            let _ = reply.take().map(|reply| reply.send(outcome));
            true
        }));
    }

    /// Resolve what the change just applied decides, and take the effects it deferred.
    pub(super) fn settle(&mut self) -> Vec<Effect> {
        let mut waits = std::mem::take(&mut self.waits);
        waits.retain_mut(|wait| !wait(self));
        self.waits.splice(0..0, waits);
        std::mem::take(self.effects())
    }

    /// Drop every wait: a closed session decides nothing more.
    pub(super) fn close_waits(&mut self) {
        self.waits.clear();
    }

    /// Stop for `failure`: no agent runs, and nothing waits or follows from the
    /// change that failed.
    pub(super) fn fail(&mut self, failure: JournalFailure) {
        let failed = failure.clone();
        (self.agents).stop(|| SessionError::JournalFailed(failed.clone()).into());
        self.phase = SessionPhase::Failed(failure);
        self.close_waits();
        self.effects().clear();
    }

    /// The committed state and the `observed` views, as one revision shows them.
    pub(in crate::session) fn snapshot(&self, observed: &Observed) -> ObservationSnapshot {
        let mut snapshot = ObservationSnapshot {
            phase: self.phase.clone(),
            revision: observed.revision,
            records: (self.records.iter())
                .map(|record| (record.sequence, record.clone()))
                .collect(),
            ledger: self.ledger.clone(),
            jobs: self.jobs.views(),
            responses: observed.responses.clone(),
            activity: crate::agent::activities(self),
            context: observed.context.clone(),
        };
        snapshot.read_phase();
        snapshot
    }
}

impl Observed {
    /// Fold an update the journal does not carry.
    pub(in crate::session) fn post(&mut self, event: RuntimeEvent) {
        let applied = self.applied;
        let committed = |sequence| sequence <= applied;
        crate::agent::live(&mut self.responses, None, committed, event);
    }

    /// The next publication.
    pub(in crate::session) fn publish(&mut self, event: RuntimeEvent) -> ObservedEvent {
        self.revision += 1;
        ObservedEvent {
            revision: self.revision,
            event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{JobSpec, JobTransition};

    #[tokio::test]
    async fn replay_refuses_a_transition_the_state_does_not_admit() {
        let runtime = crate::tests::TestRuntime::new().await;
        let spec = JobSpec::test(runtime.agent.clone(), "exec");
        let job = runtime.jobs.test_running(spec).await.into_test_id();
        let mut records = runtime.store.records();
        let sequence = records.len() as u64 + 1;
        let state = JobTransition::Running;
        let regressed = SessionEvent::JobStateChanged { job, state };
        records.push(crate::session::tests::record(
            &runtime.agent,
            sequence,
            regressed,
        ));
        assert!(matches!(CoreState::replay(&records),
            Err(SessionError::IllegalTransition { sequence: refused }) if refused.get() == sequence));
    }
}
