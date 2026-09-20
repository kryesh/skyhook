//! One agent's context window and independently owned provider conversation.

use std::collections::HashMap;

use super::{
    HarnessError,
    compact::{TokenMeter, retention_budget},
};
use crate::{
    agent::ContextUsage,
    identity::AgentId,
    provider::{
        Provider, ProviderContext,
        profile::{ModelProfile, StateMode},
        protocol::{HistoryLifetime, Message, ModelRequest, Usage, UserContent},
    },
    session::{EventRecord, ModelRequestTemplate, SessionEvent, project_history},
};

pub(super) struct AgentContext {
    pub profile: ModelProfile,
    pub template: ModelRequestTemplate,
    pub projected: Vec<(u64, Message)>,
    pub meter: TokenMeter,
    pub provider: Box<dyn ProviderContext>,
    /// Pinned tools the live registry no longer provides as journaled.
    pub unavailable_tools: std::sync::Arc<std::collections::HashSet<String>>,
    /// The last journal sequence reflected in `projected`.
    through: u64,
    /// Checkpoint and retained messages, which precede later commits in `projected`.
    prefix: usize,
    /// Occupancy at which a summary last failed to shrink the context.
    skipped_at: Option<u64>,
}

impl AgentContext {
    pub fn open(
        agent: &AgentId,
        profile: ModelProfile,
        template: ModelRequestTemplate,
        factory: &dyn Provider,
        records: &[EventRecord],
        restore_meter: bool,
    ) -> Result<Self, HarnessError> {
        let projected = project_history(records, agent)?;
        let provider = factory.open_context(agent.to_string())?;
        let meter = if restore_meter {
            TokenMeter::restore(records, agent)
        } else {
            TokenMeter::default()
        };
        Ok(Self {
            profile,
            template,
            prefix: history_prefix(&projected, records, agent),
            projected,
            meter,
            provider,
            unavailable_tools: Default::default(),
            through: records.last().map_or(0, |record| record.sequence),
            skipped_at: None,
        })
    }

    /// The journal remains authoritative, including compactions committed externally.
    /// Only records committed since the last refresh are visited, unless one of them
    /// replaces this agent's history.
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
            let records = store.records().await;
            self.projected = project_history(&records, agent)?;
            self.prefix = history_prefix(&self.projected, &records, agent);
            self.skipped_at = None;
            self.through = records.last().map_or(0, |record| record.sequence);
            return Ok(());
        }
        // The turn pushes its own commits as it makes them; other producers'
        // commits interleave, so later history is kept in journal order.
        let suffix = &self.projected[self.prefix..];
        let known: std::collections::HashSet<_> =
            suffix.iter().map(|(sequence, _)| *sequence).collect();
        for (sequence, message) in committed {
            if !known.contains(&sequence) && !message.is_content_free() {
                self.projected.push((sequence, message));
            }
        }
        self.projected[self.prefix..].sort_by_key(|(sequence, _)| *sequence);
        self.through = through;
        Ok(())
    }

    /// Build the next agent request: projected history, then runtime state as tail unless the
    /// state mode is none.
    /// Its history is marked as ending once the request itself reaches the
    /// compaction threshold.
    pub fn request(&self, runtime: UserContent) -> ModelRequest {
        let mut request = ModelRequest {
            history: crate::session::merge_tool_results(
                self.projected.iter().map(|(_, message)| message.clone()),
            ),
            // The caller commits persisted state to history before sending.
            tail: match self.profile.state_mode {
                StateMode::None => Vec::new(),
                StateMode::Dynamic | StateMode::Persist => vec![Message::User(vec![runtime])],
            },
            ..self.template.to_request()
        };
        if self.reaches_compaction(self.meter.estimate(&request)) {
            request.history_lifetime = HistoryLifetime::Ending;
        }
        request
    }

    pub fn needs_compaction(&self, usage: Usage) -> bool {
        // Only the completed response's reported occupancy, including cached input
        // and generated output; estimates and output limits never trigger it.
        let tokens = occupancy(usage);
        // A summary that could not shrink this context is not worth repeating until
        // there is another retained tail's worth of history to fold into it.
        let grown = self.skipped_at.is_none_or(|skipped| {
            tokens >= skipped.saturating_add(retention_budget(self.profile.max_context))
        });
        grown && self.reaches_compaction(tokens)
    }

    /// A summary at this occupancy could not shrink the context.
    pub fn compaction_skipped(&mut self, usage: Usage) {
        self.skipped_at = Some(occupancy(usage));
    }

    /// A mode switch invalidates bound reasoning in the history before it.
    pub fn strip_bound_reasoning(&mut self) {
        let later = self.projected.split_off(self.prefix);
        let kept = later
            .into_iter()
            .filter_map(|(sequence, message)| Some((sequence, message.without_bound_reasoning()?)));
        self.projected.extend(kept);
    }

    fn reaches_compaction(&self, tokens: u64) -> bool {
        u128::from(tokens) * 5 >= u128::from(self.profile.max_context) * 4
    }

    pub fn contains_images(&self) -> bool {
        self.projected
            .iter()
            .any(|(_, message)| super::contains_images(std::slice::from_ref(message)))
    }
}

fn occupancy(usage: Usage) -> u64 {
    let input = usage.input_tokens.saturating_add(usage.cached_input_tokens);
    input.saturating_add(usage.output_tokens)
}

/// The projected checkpoint and the retained messages at or before its frontier.
fn history_prefix(projected: &[(u64, Message)], records: &[EventRecord], agent: &AgentId) -> usize {
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

/// Reconstruct context occupancy for historical agents without consulting current config.
pub(in crate::agent) fn recorded_context(
    records: &[EventRecord],
) -> HashMap<AgentId, ContextUsage> {
    let mut contexts = HashMap::new();
    let capacities: HashMap<_, _> = records
        .iter()
        .filter_map(|record| match &record.event {
            SessionEvent::AgentStarted {
                profile: Some(profile),
                ..
            }
            | SessionEvent::ModelChanged { profile } => {
                Some((&record.agent, profile.profile.max_context))
            }
            _ => None,
        })
        .collect();
    for (agent, capacity) in capacities {
        let Some(mut request) = records.iter().rev().find_map(|record| {
            if &record.agent == agent
                && matches!(
                    record.event,
                    SessionEvent::ModelRequested {
                        purpose: crate::session::ModelPurpose::Agent,
                        ..
                    }
                )
            {
                crate::session::reconstruct_model_request(records, record.sequence)
                    .ok()
                    .map(|(_, request)| request)
            } else {
                None
            }
        }) else {
            continue;
        };
        // The recorded tail is that request's dynamic runtime state; history is re-projected.
        let tail = std::mem::take(&mut request.tail);
        request.history.clear();
        request.history_lifetime = HistoryLifetime::default();
        let meter = TokenMeter::restore(records, agent);
        let Ok(history) = crate::session::project_history(records, agent) else {
            continue;
        };
        let current = ModelRequest {
            history: crate::session::merge_tool_results(
                history.into_iter().map(|(_, message)| message),
            ),
            tail,
            ..request
        };
        contexts.insert(
            agent.clone(),
            ContextUsage {
                tokens: meter.estimate(&current),
                capacity,
            },
        );
    }
    contexts
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use tokio::sync::Notify;

    use super::super::*;
    use crate::provider::{
        ProviderContext, ProviderError, ProviderFuture, ResponseStream,
        protocol::{StopReason, events_for_content},
    };

    #[derive(Default)]
    struct Tracking {
        next: AtomicUsize,
        opened: Mutex<Vec<(usize, String)>>,
        dropped: Mutex<Vec<usize>>,
        fail_all_calls: AtomicBool,
        entered: Notify,
        released: Notify,
    }

    struct Factory(Arc<Tracking>);
    struct Context {
        tracking: Arc<Tracking>,
        id: usize,
    }

    impl Provider for Factory {
        fn open_context(
            &self,
            correlation: String,
        ) -> Result<Box<dyn ProviderContext>, ProviderError> {
            let id = self.0.next.fetch_add(1, Ordering::SeqCst);
            self.0.opened.lock().unwrap().push((id, correlation));
            Ok(Box::new(Context {
                tracking: self.0.clone(),
                id,
            }))
        }
    }

    impl Drop for Context {
        fn drop(&mut self) {
            self.tracking.dropped.lock().unwrap().push(self.id);
            self.tracking.released.notify_one();
        }
    }

    impl ProviderContext for Context {
        fn invoke(&mut self, request: ModelRequest) -> ProviderFuture {
            let blocks = request.messages().rev().flat_map(|message| match message {
                Message::User(blocks) => blocks.as_slice(),
                _ => &[],
            });
            let mut texts = blocks.filter_map(|block| match block {
                UserContent::Text { text } => Some(text.as_str()),
                _ => None,
            });
            let block = texts.next() == Some("block");
            let tracking = self.tracking.clone();
            Box::pin(async move {
                if tracking.fail_all_calls.load(Ordering::SeqCst) {
                    return Err(ProviderError::protocol("retry this request"));
                }
                if block {
                    // Blocked calls end only through interruption.
                    tracking.entered.notify_one();
                    std::future::pending::<()>().await;
                }
                let mut events = events_for_content(&[AssistantContent::text("text/0", 0, "done")]);
                events.push(ResponseChunk::ResponseEnded {
                    stop_reason: StopReason::EndTurn,
                });
                let events = futures_util::stream::iter(events.into_iter().map(Ok));
                Ok(Box::pin(events) as ResponseStream)
            })
        }
    }

    fn test_context(capacity: u64, max_output: u64) -> super::AgentContext {
        let profile = ModelProfile::new("test", "test", None, capacity, max_output, false);
        let request = ModelRequest {
            model: profile.model.clone(),
            system: vec![],
            history: Vec::new(),
            tail: Vec::new(),
            history_lifetime: Default::default(),
            tools: vec![],
            response_schema: None,
            reasoning: None,
            max_output_tokens: Some(max_output),
            correlation: None,
            blobs: Default::default(),
        };
        let provider = Box::new(Context {
            tracking: Arc::new(Tracking::default()),
            id: 0,
        });
        super::AgentContext {
            profile,
            template: request.try_into().unwrap(),
            projected: vec![],
            meter: super::TokenMeter::default(),
            provider,
            unavailable_tools: Default::default(),
            through: 0,
            prefix: 0,
            skipped_at: None,
        }
    }

    #[test]
    fn history_ends_once_the_request_itself_reaches_compaction() {
        use crate::provider::protocol::HistoryLifetime::*;
        let text = |len| {
            (
                1,
                Message::User(vec![UserContent::Text {
                    text: "x".repeat(len),
                }]),
            )
        };
        let mut context = test_context(10_000, 1_000);
        let runtime = || UserContent::Runtime {
            text: "state".into(),
        };
        context.projected = vec![text(100)];
        assert_eq!(context.request(runtime()).history_lifetime, Continuing);
        // The input alone reaches 80%: the response will compact this history away.
        context.projected.push(text(40_000));
        assert_eq!(context.request(runtime()).history_lifetime, Ending);
    }

    #[test]
    fn meter_scales_later_estimates_by_the_reported_prompt_in_either_direction() {
        let mut context = test_context(128_000, 1_000);
        let request = |context: &super::AgentContext| {
            let text = "x".repeat(40_000);
            context.request(UserContent::Runtime { text })
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
            context.projected = vec![(1, Message::User(vec![UserContent::Text { text }]))];
            let grown = super::super::compaction::estimate_request(&request(&context));
            let expected = u128::from(grown) * u128::from(actual) / u128::from(raw);
            let found = context.meter.estimate(&request(&context));
            assert_eq!(u128::from(found), expected);
            context.projected.clear();
        }
    }

    #[test]
    fn a_skipped_compaction_waits_for_another_retained_tail_of_growth() {
        let mut context = test_context(128_000, 1_000);
        let full = tests::usage(100_000, 2_000, 400);
        assert!(context.needs_compaction(full));
        context.compaction_skipped(full);
        assert!(!context.needs_compaction(full));
        assert!(!context.needs_compaction(tests::usage(100_000, 9_999, 400)));
        assert!(context.needs_compaction(tests::usage(100_000, 10_000, 400)));
    }

    #[test]
    fn completed_usage_compacts_at_eighty_percent_independently_of_output_budget() {
        let limits = [
            (128_000, 102_400),
            (128_001, 102_401),
            (u64::MAX, 14_757_395_258_967_641_292),
        ];
        for (capacity, threshold) in limits {
            for max_output in [1, capacity - 1] {
                let context = test_context(capacity, max_output);
                let total = |tokens: u64| Usage {
                    input_tokens: tokens / 3,
                    cached_input_tokens: tokens / 3,
                    output_tokens: tokens - 2 * (tokens / 3),
                };
                assert!(!context.needs_compaction(Usage::default()));
                for tokens in [threshold - 1, threshold, threshold + 1] {
                    let expected = tokens >= threshold;
                    assert_eq!(
                        context.needs_compaction(total(tokens)),
                        expected,
                        "{tokens}"
                    );
                }
                // Each partial sum overflows a u64; a wrapped total would be tiny.
                for overflow in [tests::usage(u64::MAX, 1, 0), tests::usage(u64::MAX, 0, 1)] {
                    assert!(context.needs_compaction(overflow));
                }
            }
        }
    }

    #[tokio::test]
    async fn cancelled_children_release_but_failed_children_retain_their_context() {
        let root = tempfile::tempdir().unwrap();
        let tracking = Arc::new(Tracking::default());
        let profile = ModelProfile::new("test", "first", None, 128_000, 16_384, false);
        let harness = HarnessBuilder::new(root.path())
            .session_root(root.path().join("sessions"))
            .provider("test", Arc::new(Factory(tracking.clone())))
            .model_profile("first", profile)
            .default_model_profile("first")
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let wait_dropped = async |id| {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let notified = tracking.released.notified();
                    if tracking.dropped.lock().unwrap().contains(&id) {
                        break;
                    }
                    notified.await;
                }
            })
            .await
            .expect("context released")
        };
        let jobs = &session.runtime.jobs;
        for (index, fail) in [(1, false), (2, true)] {
            let child = session.root.child(index);
            let spec = crate::job::JobSpec {
                role: crate::job::JobRole::Agent,
                ..crate::job::JobSpec::test(session.root.clone(), "agent")
            };
            let owner_job = jobs.create(spec).await.unwrap().into_test_id();
            let launch = tests::child_launch(&session, child.clone(), Some(owner_job));
            let sender = session.runtime.spawn_agent(launch).await.unwrap();
            tracking.fail_all_calls.store(fail, Ordering::SeqCst);
            let (done, received) = oneshot::channel();
            let content = vec![UserContent::Text {
                text: "block".into(),
            }];
            let input = AgentCommand::Input {
                options: Default::default(),
                content,
                done: Some(done),
            };
            sender.send(input).await.unwrap();
            if !fail {
                tracking.entered.notified().await;
                session.runtime.interrupt_tree(&child).await;
            }
            assert!(received.await.unwrap().is_err());
            if fail {
                // Failed child turns are retained for retry with their provider
                // session/history; explicit interruption still tears one down.
                let dropped = tracking.dropped.lock().unwrap().contains(&(index as usize));
                assert!(!dropped, "failed child context must remain live for retry");
            } else {
                wait_dropped(index as usize).await;
            }
            tracking.fail_all_calls.store(false, Ordering::SeqCst);
            session.prompt("root still usable").await.unwrap();
            assert!(!tracking.dropped.lock().unwrap().contains(&0));
        }
        assert_eq!(tracking.opened.lock().unwrap().len(), 3);
        session.shutdown().await.unwrap();
        wait_dropped(0).await;
    }
}
