//! One agent's context window and independently owned provider conversation.

use super::HarnessError;
use crate::{
    identity::AgentId,
    provider::{
        ProviderContext,
        profile::StateMode,
        protocol::{ModelRequest, Usage},
    },
    session::{
        CommittedUsage, CompactionTrigger, EventRecord, Message, ModelContext, ModelPurpose,
        ModelRequestTemplate, ProfileSnapshot, Projected, Projection, RecordSeq, RuntimeState,
        SessionEvent, UserPart, project_history,
    },
};

pub(super) struct AgentContext {
    /// The profile this context was opened for; a different selection reopens it.
    pub profile: ProfileSnapshot,
    pub template: ModelRequestTemplate,
    pub projected: Projection,
    pub provider: Box<dyn ProviderContext>,
    /// The last request's blobs, which the next request reuses.
    pub blobs: crate::media::LoadedBlobs,
    /// Pinned tools the live registry no longer provides as journaled.
    pub unavailable_tools: std::sync::Arc<std::collections::HashSet<String>>,
    /// What the journal leaves for the next request to compact for.
    reported: Reported,
    /// The agent's latest request context and its record, while its mode is unchanged:
    /// a mode pins its prompt and tools with a context of its own.
    journaled: Option<(RecordSeq, ModelContext)>,
    /// The last journal sequence reflected in `projected`.
    through: RecordSeq,
    /// Checkpoint and retained messages, which precede later commits in `projected`.
    prefix: usize,
}

/// The occupancy the agent's last committed response reported, while no compaction
/// has answered it and its model and mode are unchanged since: a fold of its journal,
/// so a reopened context decides as the live one would.
#[derive(Default)]
struct Reported {
    committed: CommittedUsage,
    pending: Option<Usage>,
}

impl Reported {
    fn fold(&mut self, record: &EventRecord) {
        if let Some((_, usage)) = self.committed.fold(record) {
            self.pending = Some(usage);
        }
        match &record.event {
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
        profile: ProfileSnapshot,
        template: ModelRequestTemplate,
        provider: Box<dyn ProviderContext>,
        records: &[EventRecord],
    ) -> Self {
        let projected = project_history(records, agent);
        let mut reported = Reported::default();
        let mut journaled = None;
        for record in records.iter().filter(|record| &record.agent == agent) {
            reported.fold(record);
            fold_journaled(&mut journaled, record);
        }
        Self {
            profile,
            template,
            prefix: history_prefix(&projected.messages, records, agent),
            projected,
            provider,
            blobs: Default::default(),
            unavailable_tools: Default::default(),
            reported,
            journaled,
            through: records
                .last()
                .map_or(RecordSeq::default(), |record| record.sequence),
        }
    }

    /// Project what the journal committed since the last refresh, from any producer.
    /// A compaction among it replaces this agent's history, so all is re-projected.
    pub fn refresh(
        &mut self,
        store: &crate::session::SessionStore,
        agent: &AgentId,
    ) -> Result<(), HarnessError> {
        let (mut compacted, mut through) = (false, self.through);
        let mut reported = std::mem::take(&mut self.reported);
        store.read(|state| {
            let window = state.records_after(self.through);
            for record in window.iter().filter(|record| &record.agent == agent) {
                reported.fold(record);
                fold_journaled(&mut self.journaled, record);
                compacted |= matches!(record.event, SessionEvent::Compaction { .. });
            }
            if !compacted {
                (self.projected).extend(&state.ledger, &state.records, window, agent);
            }
            through = window.last().map_or(through, |record| record.sequence);
        });
        self.reported = reported;
        if compacted {
            store.read(|state| {
                let records = &state.records;
                self.projected = project_history(records, agent);
                self.prefix = history_prefix(&self.projected.messages, records, agent);
                self.through = records.last().map_or(through, |record| record.sequence);
            });
            return Ok(());
        }
        self.through = through;
        Ok(())
    }

    /// Build the runtime-state tail only when the profile sends it. A persisting
    /// profile commits it to history instead, before the request.
    pub fn tail(&self, runtime: impl FnOnce() -> RuntimeState) -> Option<Message> {
        match self.profile.profile.state_mode {
            StateMode::None => None,
            StateMode::Dynamic | StateMode::Persist => Some(Message::State(runtime())),
        }
    }

    /// The next request as the provider receives it: the projected history, then the
    /// tail as this request's own state.
    pub fn request(&self, tail: Option<&Message>) -> ModelRequest {
        ModelRequest {
            context: self.projected.context(tail),
            ..self.template.to_request()
        }
    }

    /// The record of the next request's context, journaled unless it is the agent's
    /// latest: requests sharing a record keep their reported prompts comparable.
    pub async fn context_record(
        &mut self,
        store: &crate::session::SessionStore,
        agent: &AgentId,
    ) -> Result<RecordSeq, crate::session::SessionError> {
        let template = self.template.to_request();
        let context = ModelContext {
            purpose: ModelPurpose::Agent,
            profile: self.profile.clone(),
            system: template.system,
            tools: template.tools,
            response_schema: template.response_schema,
        };
        if let Some((sequence, journaled)) = &self.journaled
            && *journaled == context
        {
            return Ok(*sequence);
        }
        let event = SessionEvent::ModelContext {
            context: context.clone(),
        };
        let record = store.append(agent.clone(), event).await?;
        self.journaled = Some((record.sequence, context));
        Ok(record.sequence)
    }

    /// The reported occupancy the next request compacts for first, if over the threshold:
    /// the completed response's, including cached input and generated output.
    pub fn compaction_due(&self) -> Option<CompactionTrigger> {
        let tokens = self.reported.pending?.occupancy()?;
        self.needs_compaction(tokens)
            .then_some(CompactionTrigger::Occupancy(tokens))
    }

    fn needs_compaction(&self, tokens: u64) -> bool {
        let (numerator, denominator) = COMPACTION_THRESHOLD;
        let profile = &self.profile.profile;
        let budget = profile.max_context.get() - profile.max_output.get();
        u128::from(tokens) * denominator >= u128::from(budget) * numerator
    }

    /// A mode switch invalidates bound reasoning in the history before it.
    pub fn strip_bound_reasoning(&mut self) {
        self.projected.unbind(self.prefix);
    }

    pub fn contains_images(&self) -> bool {
        self.projected
            .messages
            .iter()
            .any(|projected| match &projected.message {
                Message::User(content) => content.iter().any(UserPart::is_image),
                Message::Tool(results) => results.iter().any(|result| !result.images.is_empty()),
                Message::Assistant(_) | Message::State(_) => false,
            })
    }
}

fn fold_journaled(journaled: &mut Option<(RecordSeq, ModelContext)>, record: &EventRecord) {
    match &record.event {
        SessionEvent::ModelContext { context } if context.purpose == ModelPurpose::Agent => {
            *journaled = Some((record.sequence, context.clone()));
        }
        SessionEvent::ModeChanged { .. } => *journaled = None,
        _ => {}
    }
}

/// The share of the input budget (the context window less the output reserve), as a
/// fraction, whose reported use compacts it.
const COMPACTION_THRESHOLD: (u128, u128) = (9, 10);

/// The projected checkpoint and the retained messages at or before its frontier.
fn history_prefix(projected: &[Projected], records: &[EventRecord], agent: &AgentId) -> usize {
    let frontier = records.iter().rev().find_map(|record| match &record.event {
        SessionEvent::Compaction { checkpoint } if &record.agent == agent => {
            Some(checkpoint.frontier)
        }
        _ => None,
    });
    frontier.map_or(0, |frontier| {
        1 + projected
            .iter()
            .skip(1)
            .take_while(|projected| projected.sequence <= frontier)
            .count()
    })
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::agent::runtime::tests::{
        Requests, Script, Step, answer, child_launch, ephemeral_session, model_ref, send,
        test_harness,
    };
    use crate::provider::{ProviderErrorKind, protocol::ContextId};

    fn test_context(capacity: u64, max_output: u64) -> super::AgentContext {
        let profile = ModelProfile {
            max_context: crate::tests::limit(capacity),
            max_output: crate::tests::limit(max_output),
            ..crate::tests::profile("test", false)
        };
        let request = ModelRequest {
            max_output_tokens: Some(profile.max_output),
            ..ModelRequest::test(profile.model.as_str())
        };
        let script = Script::new([], &Requests::default());
        let provider = script.open_context("test".parse().unwrap()).unwrap();
        super::AgentContext {
            profile: crate::session::ProfileSnapshot {
                name: model_ref("test"),
                profile,
            },
            template: request.try_into().unwrap(),
            projected: crate::session::Projection::default(),
            provider,
            blobs: Default::default(),
            unavailable_tools: Default::default(),
            reported: Default::default(),
            journaled: None,
            through: RecordSeq::default(),
            prefix: 0,
        }
    }

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
            let context = test_context(capacity, max_output);
            // Cache writes are already included in input and must not count twice.
            let total = |tokens: u64| {
                let third = tokens / 3;
                let usage = Usage::prompted(third, third, third, tokens - 2 * third);
                usage.occupancy().unwrap()
            };
            for tokens in [threshold - 1, threshold, threshold + 1] {
                assert_eq!(
                    context.needs_compaction(total(tokens)),
                    tokens >= threshold,
                    "{capacity} {max_output:?} {tokens}"
                );
            }
            // Each partial sum overflows a u64; a wrapped total would be tiny.
            for overflow in [tests::usage(u64::MAX, 1, 0), tests::usage(u64::MAX, 0, 1)] {
                assert!(context.needs_compaction(overflow.occupancy().unwrap()));
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
            AttemptRef, CompactionFailure, CompletedOutcome,
            tests::{attempt, record},
        };
        use crate::{agent::CompactionFault, identity::SessionId};
        let agent = AgentId::root(SessionId::from_bytes([3; 16]));
        let request = 2.into();
        let completed = [
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
        let mut reported = Reported::default();
        for (sequence, event) in (1..).zip(completed) {
            reported.fold(&record(&agent, sequence, event));
        }
        for (error, keeps) in [
            (CompactionFault::Interrupted, true),
            (CompactionFault::Truncated, true),
            (CompactionFault::Overflowed, false),
        ] {
            reported.fold(&record(&agent, 9, failed(error.clone())));
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
