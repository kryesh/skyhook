//! One agent's context window and independently owned provider conversation.

use crate::{
    identity::AgentId,
    provider::{
        ProviderContext,
        profile::{ModelProfile, StateMode},
        protocol::{ModelRequest, Usage},
    },
    session::{
        CompactionTrigger, CoreState, EventRecord, Message, ModelContext, Projection, RecordSeq,
        RequestLedger, RuntimeState, SessionEvent, UserPart, project_history,
    },
};

pub(super) struct AgentContext {
    /// What its requests are sent under: the profile it was opened for, which a
    /// different selection reopens it for, and the prompt and tools it offers.
    pub model: ModelContext,
    pub projected: Projection,
    pub provider: Box<dyn ProviderContext>,
    /// The last request's blobs, which the next request reuses.
    pub blobs: crate::media::LoadedBlobs,
    /// What the journal leaves for the next request to compact for.
    reported: Reported,
    /// The last journal sequence reflected in `projected`.
    through: RecordSeq,
}

/// The occupancy the agent's last committed response reported, while no compaction
/// has answered it and its model and mode are unchanged since: a fold of its journal,
/// so a reopened context decides as the live one would.
#[derive(Default)]
struct Reported {
    pending: Option<Usage>,
}

impl Reported {
    /// Fold `record`, which `ledger` has observed.
    fn fold(&mut self, ledger: &RequestLedger, record: &EventRecord) {
        match &record.event {
            SessionEvent::ResponseCompleted { attempt, .. } => {
                let requested = ledger.get(attempt.request);
                if let Some(usage) = requested.and_then(|request| request.phase.completed_usage()) {
                    self.pending = Some(usage);
                }
            }
            SessionEvent::CompactionFailed { error, .. } if error.keeps_need() => {}
            SessionEvent::Compaction { .. }
            | SessionEvent::CompactionFailed { .. }
            | SessionEvent::ModelChanged { .. }
            | SessionEvent::ModeChanged { .. } => self.pending = None,
            _ => {}
        }
    }
}

impl AgentContext {
    pub fn open(
        agent: &AgentId,
        model: ModelContext,
        provider: Box<dyn ProviderContext>,
        state: &CoreState,
    ) -> Self {
        let records = &state.records;
        let mut reported = Reported::default();
        for record in records.iter().filter(|record| &record.agent == agent) {
            reported.fold(&state.ledger, record);
        }
        Self {
            model,
            projected: project_history(records, agent),
            provider,
            blobs: Default::default(),
            reported,
            through: records
                .last()
                .map_or(RecordSeq::default(), |record| record.sequence),
        }
    }

    /// Project what the journal committed since the last refresh, from any producer.
    /// A compaction among it replaces this agent's history, so all is re-projected.
    pub fn refresh(&mut self, store: &crate::session::SessionStore, agent: &AgentId) {
        store.read(|state| {
            let window = state.records_after(self.through);
            let mut compacted = false;
            for record in window.iter().filter(|record| &record.agent == agent) {
                self.reported.fold(&state.ledger, record);
                compacted |= matches!(record.event, SessionEvent::Compaction { .. });
            }
            if compacted {
                self.projected = project_history(&state.records, agent);
            } else {
                (self.projected).extend(&state.ledger, &state.records, window, agent);
            }
            self.through = window.last().map_or(self.through, |record| record.sequence);
        });
    }

    /// Build the runtime-state tail only when the profile sends it. A persisting
    /// profile commits it to history instead, before the request.
    pub fn tail(&self, runtime: impl FnOnce() -> RuntimeState) -> Option<Message> {
        match self.model.profile.profile.state_mode {
            StateMode::None => None,
            StateMode::Dynamic | StateMode::Persist => Some(Message::State(runtime())),
        }
    }

    /// The next request as the provider receives it: the projected history, then the
    /// tail as this request's own state.
    pub fn request(&self, tail: Option<&Message>) -> ModelRequest {
        ModelRequest {
            context: self.projected.context(tail),
            ..self.model.template()
        }
    }

    /// The reported occupancy the next request compacts for first, if over the threshold:
    /// the completed response's, including cached input and generated output.
    pub fn compaction_due(&self) -> Option<CompactionTrigger> {
        let tokens = self.reported.pending?.occupancy()?;
        needs_compaction(&self.model.profile.profile, tokens)
            .then_some(CompactionTrigger::Occupancy(tokens))
    }

    pub fn contains_images(&self) -> bool {
        self.projected
            .messages
            .iter()
            .any(|projected| match &projected.message {
                Message::User(content) => content.iter().any(UserPart::is_image),
                Message::Tool(result) => !result.images.is_empty(),
                Message::Assistant(_) | Message::State(_) => false,
            })
    }
}

/// The share of the input budget (the context window less the output reserve), as a
/// fraction, whose reported use compacts it.
const COMPACTION_THRESHOLD: (u128, u128) = (9, 10);

fn needs_compaction(profile: &ModelProfile, tokens: u64) -> bool {
    let (numerator, denominator) = COMPACTION_THRESHOLD;
    let budget = profile.max_context.get() - profile.max_output.get();
    u128::from(tokens) * denominator >= u128::from(budget) * numerator
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::agent::runtime::tests::{
        Requests, Script, Step, answer, child_launch, ephemeral_session, send, test_harness,
    };
    use crate::provider::{ProviderErrorKind, protocol::ContextId};
    use crate::session::ModelPurpose;

    #[test]
    fn completed_usage_compacts_at_ninety_percent_after_reserving_configured_output() {
        let limits = [
            (128_000, 16_384, 100_455),
            (128_001, 1, 115_200),
            (128_002, 1, 115_201),
            (128_000, 127_999, 1),
            (u64::MAX, 1, 16_602_069_666_338_596_453),
        ];
        for (capacity, max_output, threshold) in limits {
            let profile = ModelProfile {
                max_context: crate::tests::limit(capacity),
                max_output: crate::tests::limit(max_output),
                ..crate::tests::profile("test", false)
            };
            // Cache writes are already included in input and must not count twice.
            let total = |tokens: u64| {
                let third = tokens / 3;
                let usage = Usage::prompted(third, third, third, tokens - 2 * third);
                usage.occupancy().unwrap()
            };
            for tokens in [threshold - 1, threshold, threshold + 1] {
                assert_eq!(
                    super::needs_compaction(&profile, total(tokens)),
                    tokens >= threshold,
                    "{capacity} {max_output:?} {tokens}"
                );
            }
            // Each partial sum overflows a u64; a wrapped total would be tiny.
            for overflow in [tests::usage(u64::MAX, 1, 0), tests::usage(u64::MAX, 0, 1)] {
                assert!(super::needs_compaction(
                    &profile,
                    overflow.occupancy().unwrap()
                ));
            }
        }
    }

    /// A committed response over the threshold leaves the need through failed summary
    /// rounds a fresh summary could pass, but not through an overflowing one: that
    /// round already shrank its summary, so the next request decides.
    #[test]
    fn only_a_summary_failure_a_retry_could_pass_keeps_the_need() {
        use super::super::context::Reported;
        use crate::session::{
            AttemptRef, CompactionFailure, CompletedOutcome, ModelContext, RequestLedger,
            tests::{self, attempt, record, requested},
        };
        use crate::{agent::CompactionFault, identity::SessionId};
        let agent = AgentId::root(SessionId::from_bytes([3; 16]));
        let request = 2.into();
        let completed = [
            SessionEvent::ModelContext {
                context: ModelContext::test(ModelPurpose::Agent, tests::profile()),
            },
            requested(1.into()),
            attempt(request, 1),
            SessionEvent::Usage {
                request,
                usage: tests::usage(1_000, 0, 10),
            },
            SessionEvent::ResponseCompleted {
                attempt: AttemptRef {
                    request,
                    attempt: 1,
                },
                message: 3.into(),
                outcome: CompletedOutcome::Answer,
            },
        ];
        let failed = |error| SessionEvent::CompactionFailed {
            failure: CompactionFailure::BeforeRequest,
            error,
        };
        let (mut ledger, mut reported) = (RequestLedger::default(), Reported::default());
        for (sequence, event) in (1..).zip(completed) {
            let record = record(&agent, sequence, event);
            ledger.observe(&record);
            reported.fold(&ledger, &record);
        }
        for (error, keeps) in [
            (CompactionFault::Interrupted, true),
            (CompactionFault::Truncated, true),
            (CompactionFault::Overflowed, false),
        ] {
            reported.fold(&ledger, &record(&agent, 9, failed(error.clone())));
            assert_eq!(reported.pending.is_some(), keeps, "{error:?}");
        }
    }

    #[tokio::test]
    async fn cancelled_children_release_but_failed_children_retain_their_context() {
        let root = tempfile::tempdir().unwrap();
        let steps = [
            Step::new(answer("done")).gated(),
            Step::new(answer("done")),
            Step::fail(ProviderErrorKind::Protocol.error("not retried")),
            Step::new(answer("done")),
        ];
        let script = Script::new(steps, &Requests::default());
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, script.clone()).await;
        let session = ephemeral_session(&harness).await;
        let root_context = ContextId::from(&session.root);
        let jobs = &session.runtime.jobs;
        for (index, fail) in [(1, false), (2, true)] {
            let child = session.root.child(index);
            let spec = crate::job::JobSpec {
                role: crate::job::JobRole::Agent,
                ..crate::job::JobSpec::test(session.root.clone(), "agent")
            };
            let owner_job = jobs.create(spec).await.unwrap().into_test_id();
            let launch = child_launch(&session, None);
            let spawned = session
                .runtime
                .spawn_agent(child.clone(), Some(owner_job), launch);
            spawned.await.unwrap();
            let (done, received) = oneshot::channel();
            let input = AgentCommand::Input {
                options: Default::default(),
                content: vec![UserPart::Text {
                    text: "work".into(),
                }],
                done: Some(RequestCompletion::Child(done)),
            };
            send(&session, &child, input).await;
            if !fail {
                script.request(0).await;
                session.runtime.interrupt_tree(&child).await;
            }
            assert!(received.await.unwrap().is_err());
            let context = ContextId::from(&child);
            if fail {
                // Failed child turns are retained for retry with their provider
                // session/history; explicit interruption still tears one down.
                assert!(
                    !script.dropped(&context),
                    "failed child context must remain live for retry"
                );
            } else {
                script.released(&context).await;
            }
            session.prompt("root still usable").await.unwrap();
            assert!(!script.dropped(&root_context));
        }
        assert_eq!(script.opened.load(Ordering::SeqCst), 3);
        session.shutdown().await.unwrap();
        script.released(&root_context).await;
    }
}
