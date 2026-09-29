//! One agent's context window and independently owned provider conversation.

use std::collections::HashMap;

use super::{HarnessError, compact::TokenMeter};
use crate::{
    agent::ContextUsage,
    identity::AgentId,
    provider::{
        ProviderContext,
        profile::StateMode,
        protocol::{ModelRequest, Usage},
    },
    session::{
        AttemptRef, EventRecord, Message, ModelPurpose, ModelRequestTemplate, ProfileSnapshot,
        Projection, RecordSeq, RequestLedger, SessionEvent, UserPart, project_history,
        render_history,
    },
};

pub(super) struct AgentContext {
    /// The profile this context was opened for; a different selection reopens it.
    pub profile: ProfileSnapshot,
    pub template: ModelRequestTemplate,
    /// The estimate of what `template` adds to every request.
    pub template_tokens: u64,
    pub projected: Projection,
    pub meter: TokenMeter,
    pub provider: Box<dyn ProviderContext>,
    /// The last request's blobs, which the next request reuses.
    pub blobs: crate::media::LoadedBlobs,
    /// Pinned tools the live registry no longer provides as journaled.
    pub unavailable_tools: std::sync::Arc<std::collections::HashSet<String>>,
    /// The last journal sequence reflected in `projected`.
    through: RecordSeq,
    /// Checkpoint and retained messages, which precede later commits in `projected`.
    prefix: usize,
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
        Self {
            profile,
            template_tokens: super::compaction::estimate_request(&template.to_request()),
            template,
            prefix: history_prefix(&projected.messages, records, agent),
            projected,
            meter: TokenMeter::default(),
            provider,
            blobs: Default::default(),
            unavailable_tools: Default::default(),
            through: records
                .last()
                .map_or(RecordSeq::default(), |record| record.sequence),
        }
    }

    /// Project what the journal committed since the last refresh, from any producer.
    /// A compaction among it replaces this agent's history, so all is re-projected.
    pub async fn refresh(
        &mut self,
        store: &crate::session::SessionStore,
        agent: &AgentId,
    ) -> Result<(), HarnessError> {
        let (mut compacted, mut through, mut committed) = (false, self.through, Vec::new());
        store
            .visit_records_after(self.through, |records| {
                for record in records.iter().filter(|record| &record.agent == agent) {
                    match &record.event {
                        SessionEvent::Compaction { .. } => compacted = true,
                        SessionEvent::MessageCommitted { message } => {
                            committed.push((record.sequence, message.clone()));
                        }
                        _ => {}
                    }
                }
                through = records.last().map_or(through, |record| record.sequence);
            })
            .await;
        if compacted {
            let reproject = |records: &[EventRecord]| {
                self.projected = project_history(records, agent);
                self.prefix = history_prefix(&self.projected.messages, records, agent);
                self.through = records.last().map_or(through, |record| record.sequence);
            };
            store
                .visit_records_after(RecordSeq::default(), reproject)
                .await;
            return Ok(());
        }
        self.through = through;
        self.projected.messages.extend(committed);
        Ok(())
    }

    /// The request's tail: the runtime state, unless the profile's state mode omits
    /// it. A persisting profile commits it to history instead, before the request.
    pub fn tail(&self, runtime: UserPart) -> Option<Message> {
        match self.profile.profile.state_mode {
            StateMode::None => None,
            StateMode::Dynamic | StateMode::Persist => Some(Message::User(vec![runtime])),
        }
    }

    /// The next request as the provider receives it: projected history, then the tail.
    pub fn request(&self, tail: Option<&Message>) -> ModelRequest {
        ModelRequest {
            history: render_history(self.projected.history()),
            tail: tail.map(Message::render).into_iter().collect(),
            ..self.template.to_request()
        }
    }

    pub fn needs_compaction(&self, usage: Usage) -> bool {
        // Only the completed response's reported occupancy, including cached input
        // and generated output; estimates never trigger it.
        let tokens = occupancy(usage);
        let (numerator, denominator) = COMPACTION_THRESHOLD;
        let profile = &self.profile.profile;
        let budget = profile.max_context.get() - profile.max_output.get();
        u128::from(tokens) * denominator >= u128::from(budget) * numerator
    }

    /// A mode switch invalidates bound reasoning in the history before it.
    pub fn strip_bound_reasoning(&mut self) {
        let later = self.projected.messages.split_off(self.prefix);
        let kept = later
            .into_iter()
            .map(|(sequence, message)| (sequence, message.without_bound_reasoning()));
        self.projected.messages.extend(kept);
    }

    pub fn contains_images(&self) -> bool {
        self.projected
            .messages
            .iter()
            .any(|(_, message)| match message {
                Message::User(content) => content.iter().any(UserPart::is_image),
                Message::Tool(results) => results.iter().any(|result| !result.images.is_empty()),
                Message::Assistant(_) => false,
            })
    }
}

/// The share of the input budget (the context window less the output reserve), as a
/// fraction, whose reported use compacts it.
const COMPACTION_THRESHOLD: (u128, u128) = (9, 10);

pub(super) fn occupancy(usage: Usage) -> u64 {
    let input = usage.input_tokens.saturating_add(usage.cached_input_tokens);
    input.saturating_add(usage.output_tokens)
}

/// The projected checkpoint and the retained messages at or before its frontier.
fn history_prefix(
    projected: &[(RecordSeq, Message)],
    records: &[EventRecord],
    agent: &AgentId,
) -> usize {
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
            .take_while(|(sequence, _)| *sequence <= frontier)
            .count()
    })
}

/// Context occupancy for historical agents without consulting current config: each
/// agent's last occupancy reported by an attempt that committed its response, or a
/// later checkpoint's estimate, at the capacity of its latest request. Output of an
/// attempt that committed nothing never joined the context. A zero report is a
/// backend reporting nothing, as the meter treats it too.
pub(in crate::agent) fn recorded_context(
    records: &[EventRecord],
    ledger: &RequestLedger,
) -> HashMap<AgentId, ContextUsage> {
    let committed = |attempt: AttemptRef| {
        ledger.get(attempt.request).is_some_and(|request| {
            request.purpose == ModelPurpose::Agent
                && request.phase.committed_attempt() == Some(attempt.attempt)
        })
    };
    // Requests are sequential per agent, so a usage report belongs to the agent's
    // latest started attempt.
    let mut attempts = HashMap::new();
    let mut tokens = HashMap::new();
    for record in records {
        let reported = match &record.event {
            SessionEvent::ModelAttemptStarted(attempt) => {
                attempts.insert(&record.agent, *attempt);
                continue;
            }
            SessionEvent::Usage { request, usage }
                if attempts
                    .get(&record.agent)
                    .is_some_and(|attempt| attempt.request == *request && committed(*attempt)) =>
            {
                match occupancy(*usage) {
                    0 => continue,
                    reported => reported,
                }
            }
            SessionEvent::Compaction { checkpoint } => checkpoint.after_tokens,
            _ => continue,
        };
        tokens.insert(&record.agent, reported);
    }
    let latest = |agent| ledger.get(ledger.latest(agent)?);
    tokens
        .into_iter()
        .filter_map(|(agent, tokens)| {
            let capacity = latest(agent)?.profile.profile.max_context.get();
            Some((agent.clone(), ContextUsage { tokens, capacity }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::agent::runtime::tests::{
        Requests, Script, Step, answer, child_launch, ephemeral_session, model_ref, recoverable,
        scripted_session, shutdown_session, test_harness,
    };
    use crate::provider::{
        ProviderErrorKind,
        protocol::{ContextId, ResponseEvent},
    };

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
            template_tokens: crate::agent::runtime::compaction::estimate_request(&request),
            template: request.try_into().unwrap(),
            projected: crate::session::Projection::default(),
            meter: super::TokenMeter::default(),
            provider,
            blobs: Default::default(),
            unavailable_tools: Default::default(),
            through: RecordSeq::default(),
            prefix: 0,
        }
    }

    #[test]
    fn meter_scales_later_estimates_by_the_reported_prompt_in_either_direction() {
        let mut context = test_context(128_000, 1_000);
        let request = |context: &super::AgentContext| {
            let text = "x".repeat(40_000);
            context.request(context.tail(UserPart::Text { text }).as_ref())
        };
        let raw = context.meter.estimate(&request(&context));
        for actual in [raw / 2, raw * 3] {
            context.meter.observe(raw, tests::usage(actual, 0, 7));
            assert_eq!(context.meter.estimate(&request(&context)), actual);
            // A zero report keeps that baseline.
            context.meter.observe(raw, Usage::default());
            assert_eq!(context.meter.estimate(&request(&context)), actual);
            // New history is scaled by the same ratio.
            let text = "y".repeat(80_000);
            context.projected.messages =
                vec![(1.into(), Message::User(vec![UserPart::Text { text }]))];
            let grown = super::super::compaction::estimate_request(&request(&context));
            let expected = u128::from(grown) * u128::from(actual) / u128::from(raw);
            let found = context.meter.estimate(&request(&context));
            assert_eq!(u128::from(found), expected);
            context.projected.messages.clear();
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
            let total = |tokens: u64| Usage {
                input_tokens: tokens / 3,
                cached_input_tokens: tokens / 3,
                // Cache writes are already included in input and must not count twice.
                cache_write_input_tokens: tokens / 3,
                output_tokens: tokens - 2 * (tokens / 3),
            };
            assert!(!context.needs_compaction(Usage::default()));
            for tokens in [threshold - 1, threshold, threshold + 1] {
                assert_eq!(
                    context.needs_compaction(total(tokens)),
                    tokens >= threshold,
                    "{capacity} {max_output:?} {tokens}"
                );
            }
            // Each partial sum overflows a u64; a wrapped total would be tiny.
            for overflow in [tests::usage(u64::MAX, 1, 0), tests::usage(u64::MAX, 0, 1)] {
                assert!(context.needs_compaction(overflow));
            }
        }
    }

    /// A backend that reports no usage leaves resumed occupancy unknown, not zero.
    #[tokio::test]
    async fn resumed_context_skips_unreported_usage() {
        let (_root, _, session) = scripted_session([answer("done")]).await;
        session.prompt("work").await.unwrap();
        let records = session.runtime.store.records().await;
        let resumed = crate::agent::observation::RuntimeEvents::new(&records).observe();
        assert!(!resumed.snapshot.context.contains_key(&session.root));
        shutdown_session(session).await;
    }

    /// Only a committed response's report restores occupancy: output streamed by an
    /// attempt that failed, was interrupted or was retried never joined the context.
    #[tokio::test(start_paused = true)]
    async fn resumed_context_counts_only_committed_attempts() {
        let reported = |input, output| Ok(ResponseEvent::Usage(tests::usage(input, 0, output)));
        let committed = |events: Vec<ResponseEvent>, input, output| {
            let mut events: Vec<_> = events.into_iter().map(Ok).collect();
            events.insert(events.len() - 1, reported(input, output));
            Step::stream(events)
        };
        let failed = |error| Step::stream(vec![reported(2_000, 20_000), Err(error)]);
        let steps = [
            committed(answer("done"), 1_000, 10),
            failed(ProviderErrorKind::Protocol.error("not retried")),
            committed(answer("never released"), 3_000, 30_000).midstream(),
            failed(recoverable()),
            // The retry that commits reports nothing.
            Step::new(answer("retried")),
        ];
        let script = Script::new(steps, &Requests::default());
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, script.clone()).await;
        let session = ephemeral_session(&harness).await;
        let resumed = async || {
            let records = session.runtime.store.records().await;
            let observation = crate::agent::observation::RuntimeEvents::new(&records).observe();
            observation.snapshot.context[&session.root].tokens
        };
        session.prompt("committed").await.unwrap();
        assert_eq!(resumed().await, 1_010);
        session.prompt("failed").await.unwrap_err();
        assert_eq!(resumed().await, 1_010);
        let interrupted = session.prompt("interrupted");
        let interrupt = async {
            script.held(2).await;
            session.interrupt().await;
        };
        let (result, ()) = tokio::join!(interrupted, interrupt);
        result.unwrap_err();
        assert_eq!(resumed().await, 1_010);
        session.prompt("retried").await.unwrap();
        assert_eq!(resumed().await, 1_010);
        shutdown_session(session).await;
    }

    #[tokio::test(start_paused = true)]
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
            let sender = spawned.await.unwrap();
            let (done, received) = oneshot::channel();
            let input = AgentCommand::Input {
                options: Default::default(),
                content: vec![UserPart::Text {
                    text: "work".into(),
                }],
                done: Some(RequestCompletion::Child(done)),
            };
            sender.send(input).await.unwrap();
            if !fail {
                script.request(0).await;
                session
                    .runtime
                    .interrupt_tree(&child, CancelScope::Outcome)
                    .await;
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
