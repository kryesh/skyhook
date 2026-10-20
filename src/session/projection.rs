//! One agent's model-visible history: the journal's committed messages after its
//! latest checkpoint, each with the state its request sent.
use crate::{
    identity::AgentId,
    provider::protocol::{Entry, State, StateLifetime, Usage},
    session::{Message, RuntimeState},
};
use std::sync::Arc;

use super::{
    CheckpointError, EntryKind, EventRecord, SessionError, SessionEvent, SessionStore, db,
};
use crate::session::{MessageSeq, RecordSeq, RequestSeq};

/// One agent's model-visible history: its latest checkpoint's sequence, if any, and
/// the messages in order, the checkpoint's own message first.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Projection {
    pub checkpoint: Option<RecordSeq>,
    pub messages: Vec<Projected>,
}

/// A message of an agent's projected history.
#[derive(Clone, Debug, PartialEq)]
pub struct Projected {
    /// The record that carries it: a commit, or the checkpoint.
    pub sequence: RecordSeq,
    pub message: Message,
    /// The runtime state the request that produced this response sent. Later requests
    /// send it again before the response while reasoning bound to that conversation is
    /// kept: after the latest checkpoint and mode switch.
    pub sent: Option<RuntimeState>,
}

impl Projected {
    /// A message whose bound reasoning and sent state are gone, as before a checkpoint
    /// or a mode switch.
    #[must_use]
    pub fn unbound(sequence: RecordSeq, message: Message) -> Self {
        Self {
            sequence,
            message: message.without_bound_reasoning(),
            sent: None,
        }
    }
}

impl Projection {
    /// The messages the agent committed: those after the checkpoint's own.
    pub fn commits(&self) -> &[Projected] {
        &self.messages[usize::from(self.checkpoint.is_some())..]
    }

    /// The last committed message after the checkpoint, which bounds a request's history.
    pub fn through(&self) -> Option<MessageSeq> {
        Some(self.commits().last()?.sequence.message())
    }

    /// Each message with the state sent before it.
    pub fn history(&self) -> impl Iterator<Item = (&Message, Option<&RuntimeState>)> {
        (self.messages.iter()).map(|projected| (&projected.message, projected.sent.as_ref()))
    }
    /// Drop about the oldest half of the history after its opening exchange (the
    /// checkpoint's message, or the first message with its tool results) so a request
    /// that overflowed may fit, returning the message the kept history resumes at: a
    /// user or assistant message, never tool results without their call. The cut nearest
    /// the middle wins, the older one on a tie; None when no cut drops anything.
    pub(crate) fn trim(&mut self) -> Option<MessageSeq> {
        let opening = self.opening();
        let middle = (opening + self.messages.len()) / 2;
        let cut = (opening + 1..self.messages.len())
            .filter(|index| self.is_cut(*index))
            .min_by_key(|index| (index.abs_diff(middle), *index))?;
        self.messages.drain(opening..cut);
        Some(self.messages[opening].sequence.message())
    }

    /// Drop the history between the opening exchange and `from`, a cut `trim` may make;
    /// false when `from` is no such cut.
    pub(crate) fn trim_from(&mut self, from: MessageSeq) -> bool {
        let from = RecordSeq::from(from);
        let opening = self.opening();
        let cut = self
            .messages
            .iter()
            .position(|projected| projected.sequence == from);
        match cut.filter(|cut| *cut > opening && self.is_cut(*cut)) {
            Some(cut) => {
                self.messages.drain(opening..cut);
                true
            }
            None => false,
        }
    }

    /// Where the history after the opening exchange begins.
    fn opening(&self) -> usize {
        let results = self.messages.iter().skip(1);
        let results = results.take_while(|projected| matches!(projected.message, Message::Tool(_)));
        (1 + results.count()).min(self.messages.len())
    }

    fn is_cut(&self, index: usize) -> bool {
        matches!(
            self.messages[index].message,
            Message::User(_) | Message::Assistant(_)
        )
    }

    /// Fold one of the agent's records of a kind its history folds, after its
    /// checkpoint's frontier; `requested` carries its latest request and the state that
    /// ended its tail, which that request's response was sent with.
    pub(crate) fn apply(&mut self, requested: &mut Requested, record: &EventRecord) {
        match &record.event {
            SessionEvent::ModelRequested { tail, .. } => {
                let sent = match tail.last() {
                    Some(Message::State(state)) => Some(state.clone()),
                    _ => None,
                };
                *requested = Some((record.sequence.request(), sent));
            }
            SessionEvent::MessageCommitted { message } => self.messages.push(Projected {
                sequence: record.sequence,
                message: message.clone(),
                sent: None,
            }),
            SessionEvent::ResponseCompleted {
                attempt, message, ..
            } => {
                if let Some((request, Some(state))) = requested
                    && *request == attempt.request
                    && let Some(response) = (self.messages.iter_mut().rev())
                        .find(|projected| projected.sequence == RecordSeq::from(*message))
                {
                    response.sent = Some(state.clone());
                }
            }
            SessionEvent::ModeChanged { .. } => self.unbind(),
            _ => {}
        }
    }

    /// A changed conversation invalidates bound reasoning, and the state sent for it.
    pub(crate) fn unbind(&mut self) {
        for projected in &mut self.messages {
            projected.message.unbind();
            projected.sent = None;
        }
    }

    /// The request context: the history, then `tail` as the request's own state.
    #[must_use]
    pub fn context<'a>(&self, tail: impl IntoIterator<Item = &'a Message>) -> Vec<Entry> {
        let mut context = render_history(self.history());
        let current = tail
            .into_iter()
            .map(|message| message.entry(StateLifetime::Current));
        context.extend(current);
        context
    }
}

/// History as a provider receives it: each message's sent state superseded before it.
/// Tool results are committed one message per call as each call finishes; providers
/// receive one tool message per exchange, ordered like the calls that produced it.
#[must_use]
pub fn render_history<'a>(
    messages: impl IntoIterator<Item = (&'a Message, Option<&'a RuntimeState>)>,
) -> Vec<Entry> {
    use crate::provider::protocol::Message;
    let mut merged: Vec<Entry> = Vec::new();
    let mut calls: Vec<String> = Vec::new();
    for (message, sent) in messages {
        merged.extend(sent.map(|state| {
            Entry::State(State {
                text: state.to_string(),
                lifetime: StateLifetime::Superseded,
            })
        }));
        match message.entry(StateLifetime::Persisted) {
            Entry::Message(Message::Tool(results)) => {
                if let Some(Entry::Message(Message::Tool(previous))) = merged.last_mut() {
                    previous.extend(results);
                } else {
                    merged.push(Entry::Message(Message::Tool(results)));
                }
                if let Some(Entry::Message(Message::Tool(results))) = merged.last_mut() {
                    results.sort_by_key(|result| {
                        calls
                            .iter()
                            .position(|call| call == &result.call_id)
                            .unwrap_or(usize::MAX)
                    });
                }
            }
            entry => {
                if let Entry::Message(Message::Assistant(items)) = &entry {
                    calls = items
                        .iter()
                        .filter_map(|item| item.call())
                        .map(|call| call.id().to_owned())
                        .collect();
                }
                merged.push(entry);
            }
        }
    }
    merged
}

/// What a history fold carries between records: the agent's latest request, and the
/// state ending its tail.
pub(crate) type Requested = Option<(RequestSeq, Option<RuntimeState>)>;

/// What an agent's journal holds as its context opens: its history through `through`,
/// the newest record applied then, and a feed of every record of the agent its history
/// folds, and of its checkpoints, applied after it.
pub(crate) struct OpenedHistory {
    pub(crate) projection: Projection,
    pub(crate) through: RecordSeq,
    pub(crate) feed: std::sync::mpsc::Receiver<Arc<EventRecord>>,
}

/// A request of an agent that completed: the model context it was sent under, the last
/// message its history held, and what its completing attempt reported.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CompletedRequest {
    pub(crate) context: RecordSeq,
    pub(crate) through: Option<MessageSeq>,
    pub(crate) usage: Option<Usage>,
}

/// What compaction reads of an agent's journal to retain: its requests completed since
/// its checkpoint, and the exchanges its origins open.
#[derive(Default)]
pub(crate) struct Retention {
    pub(crate) requests: Vec<CompletedRequest>,
    pub(crate) exchanges: Vec<(MessageSeq, Message)>,
}

impl SessionStore {
    /// Open `agent`'s history as its journal holds it now, feeding every record of the
    /// agent its history folds, and its checkpoints, applied afterwards.
    pub(crate) async fn open_history(
        &self,
        agent: &AgentId,
    ) -> Result<OpenedHistory, SessionError> {
        let kinds = |kind: EntryKind| kind.history_folded() || kind == EntryKind::Compaction;
        let (through, feed) = self.inner.shared.observed().subscribe(agent, kinds);
        let projection = self.history(agent, through).await?;
        Ok(OpenedHistory {
            projection,
            through,
            feed,
        })
    }

    /// The newest record applied to the observation: an open history's feed has taken
    /// every record of its agent through it that the agent's history folds.
    pub fn applied(&self) -> RecordSeq {
        self.inner.shared.observed().applied()
    }

    /// What `agent`'s last completed response reported, which its next request
    /// compacts for, as the journal leaves it now.
    pub(crate) fn reported(&self, agent: &AgentId) -> Option<Usage> {
        self.read(|state| {
            state
                .agent_states
                .get(agent)?
                .reported
                .map(|(_, usage)| usage)
        })
    }

    /// `agent`'s history through `through`, which the journal has applied.
    pub(crate) async fn history(
        &self,
        agent: &AgentId,
        through: RecordSeq,
    ) -> Result<Projection, SessionError> {
        let (session, opening) = (self.id(), db::history::Opening::Latest);
        let read = move |db: &db::Db, row| db::history::project(db, session, row, opening, through);
        self.read_agent(agent, Projection::default(), read).await
    }

    /// What compaction retains from: `agent`'s requests after `checkpoint` that completed
    /// by `frontier`, and the exchanges `origins` open by then, unless one of them is not
    /// a message the agent committed.
    pub(crate) async fn retention(
        &self,
        agent: &AgentId,
        checkpoint: RecordSeq,
        frontier: RecordSeq,
        origins: Vec<MessageSeq>,
    ) -> Result<Result<Retention, CheckpointError>, SessionError> {
        let unstarted = if origins.is_empty() {
            Ok(Retention::default())
        } else {
            Err(CheckpointError::RetainedSource)
        };
        let read = move |db: &db::Db, row| {
            let mut exchanges = Vec::new();
            for origin in origins {
                match db::history::exchange(db, row, origin, frontier)? {
                    Some(exchange) => exchanges.extend(exchange),
                    None => return Ok(Err(CheckpointError::RetainedSource)),
                }
            }
            let requests = db::history::completed_requests(db, row, checkpoint, frontier)?;
            Ok(Ok(Retention {
                requests,
                exchanges,
            }))
        };
        self.read_agent(agent, unstarted, read).await
    }

    /// Run `read` for `agent`'s row in one snapshot off the async workers, or return
    /// `unstarted` when the agent has no row yet.
    pub(super) async fn read_agent<T: Send + 'static>(
        &self,
        agent: &AgentId,
        unstarted: T,
        read: impl FnOnce(&db::Db, i64) -> Result<T, db::DbError> + Send + 'static,
    ) -> Result<T, SessionError> {
        let agent = agent.clone();
        self.snapshot(move |db| match db::agent_row(db, &agent)? {
            Some(row) => Ok(read(db, row)?),
            None => Ok(unstarted),
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        provider::codec::common::tests::envelope,
        provider::protocol::{
            AssistantItem, Binding, HistoryLifetime, ModelRequest, ReplayFormat, ToolCall,
            ToolResult,
        },
        session::{
            AttemptRef, CompactionCheckpoint, CompactionTrigger, ModelPurpose, ReplayError,
            SessionEvent,
            tests::{
                admitted, committed, context, projected, requested_history, sequences, text_message,
            },
        },
        tests::TestRuntime,
    };
    use serde_json::json;

    /// Sequences the projection fixture journals, by role.
    struct Journaled {
        plan: RecordSeq,
        research: RecordSeq,
        summarizes: RecordSeq,
        first_summary: RecordSeq,
        steering: RecordSeq,
        first: RecordSeq,
        continued: RecordSeq,
        second_summary: RecordSeq,
        second: RecordSeq,
    }

    async fn append(runtime: &TestRuntime, event: SessionEvent) -> RecordSeq {
        let append = runtime.store.append(runtime.agent.clone(), event);
        append.await.unwrap().sequence
    }

    /// A plan opening the history, research, and two checkpoints: the first retains
    /// the plan while steering arrives during its summary, the second retains both.
    async fn projection_fixture(plan: Message) -> (TestRuntime, Journaled) {
        let runtime = TestRuntime::new().await;
        let plan = append(&runtime, committed(plan)).await;
        let research = append(&runtime, committed(text_message("research"))).await;
        append(
            &runtime,
            context(ModelPurpose::Agent, "provider", "model", "system"),
        )
        .await;
        let summarizes = context(ModelPurpose::Compaction, "provider", "model", "system");
        let summarizes = append(&runtime, summarizes).await;
        let summary = |checkpoint: Option<RecordSeq>, history: &[RecordSeq], tail| {
            let detached = HistoryLifetime::Detached;
            requested_history(summarizes, checkpoint, history, tail, detached)
        };
        let attempt = |request: RecordSeq| AttemptRef {
            request: request.request(),
            attempt: 1,
        };
        let first_summary = append(&runtime, summary(None, &[plan, research], "summarize")).await;
        let started = SessionEvent::ModelAttemptStarted(attempt(first_summary));
        append(&runtime, started).await;
        let steering = append(&runtime, committed(text_message("concurrent steering"))).await;
        let checkpoint = CompactionCheckpoint {
            todos: Vec::new(),
            frontier: research,
            message: text_message("first summary"),
            retained: vec![plan.message()],
            attempt: attempt(first_summary),
            trigger: CompactionTrigger::Occupancy(100),
        };
        let compaction = SessionEvent::Compaction {
            checkpoint: checkpoint.clone(),
        };
        let first = append(&runtime, compaction).await;
        let continued = append(&runtime, committed(text_message("continued work"))).await;
        let history = [plan, steering, continued];
        let second_summary = summary(Some(first), &history, "state at request time");
        let second_summary = append(&runtime, second_summary).await;
        let started = SessionEvent::ModelAttemptStarted(attempt(second_summary));
        append(&runtime, started).await;
        let second = SessionEvent::Compaction {
            checkpoint: CompactionCheckpoint {
                frontier: continued,
                message: text_message("second summary with inherited plan"),
                retained: vec![plan.message(), steering.message()],
                attempt: attempt(second_summary),
                ..checkpoint
            },
        };
        let second = append(&runtime, second).await;
        let journaled = Journaled {
            plan,
            research,
            summarizes,
            first_summary,
            steering,
            first,
            continued,
            second_summary,
            second,
        };
        (runtime, journaled)
    }

    /// A switch to a mode first defined by it.
    fn switch_mode() -> SessionEvent {
        let definition = crate::tool::policy::Mode {
            capabilities: Vec::new(),
            instructions: None,
            hint: None,
        };
        let mode = crate::session::ModeSelection {
            name: "plan".parse().unwrap(),
            definition: Some(definition),
        };
        let capabilities = Vec::new();
        SessionEvent::ModeChanged { mode, capabilities }
    }

    fn get(sequences: &[RecordSeq]) -> Vec<u64> {
        sequences.iter().map(|sequence| sequence.get()).collect()
    }

    #[tokio::test]
    async fn projection_preserves_concurrent_messages_and_repeated_compaction() {
        let (runtime, journaled) = projection_fixture(text_message("verbatim plan")).await;
        let Journaled {
            plan,
            research,
            summarizes,
            steering,
            first,
            continued,
            second,
            ..
        } = journaled;
        let (store, agent) = (&runtime.store, &runtime.agent);
        let early = store.history(agent, steering).await.unwrap();
        assert_eq!(
            (early.checkpoint, sequences(&early)),
            (None, get(&[plan, research, steering]))
        );
        assert_eq!(early.through(), Some(steering.message()));
        let projected_first = store.history(agent, continued).await.unwrap();
        assert_eq!(
            (projected_first.checkpoint, sequences(&projected_first)),
            (Some(first), get(&[first, plan, steering, continued]))
        );
        assert_eq!(projected_first.through(), Some(continued.message()));
        assert_eq!(
            (
                &projected_first.messages[1].message,
                &projected_first.messages[2].message
            ),
            (
                &text_message("verbatim plan"),
                &text_message("concurrent steering")
            )
        );
        let projected_second = projected(store, agent).await;
        assert_eq!(
            (projected_second.checkpoint, sequences(&projected_second)),
            (Some(second), get(&[second, plan, steering]))
        );
        assert_eq!(
            projected_second.messages[1].message,
            text_message("verbatim plan")
        );
        assert!(projected(store, &agent.child(1)).await.messages.is_empty());
        // A request names a checkpoint of its own agent's and a bound its history
        // under that checkpoint holds.
        use ReplayError::{Checkpoint, Through};
        for (checkpoint, through, expected) in [
            (summarizes, continued, Some(Checkpoint)),
            (journaled.second_summary, continued, Some(Checkpoint)),
            (first, plan, None),
            (first, research, Some(Through)),
            (first, summarizes, Some(Through)),
            (first, second.next(), Some(Through)),
        ] {
            let request = SessionEvent::ModelRequested {
                context: summarizes,
                checkpoint: Some(checkpoint),
                through: Some(through.message()),
                from: None,
                tail: Vec::new(),
                history_lifetime: HistoryLifetime::Detached,
            };
            let validated = admitted(store, agent, request);
            match expected {
                None => assert!(validated.is_ok(), "{validated:?}"),
                Some(expected) => assert!(matches!(
                    validated,
                    Err(SessionError::ModelRequestReplay { reason, .. }) if reason == expected
                )),
            }
        }
    }

    /// A response carries the state its request sent, whatever its outcome, alike in
    /// projection and reconstruction: only the state, never the rest of that tail;
    /// nothing for a request no response answered or whose state something followed,
    /// and nothing before a mode switch.
    #[tokio::test]
    async fn responses_carry_the_state_their_request_sent() {
        let runtime = TestRuntime::new().await;
        let (store, agent) = (&runtime.store, &runtime.agent);
        let answer = Message::Assistant(vec![AssistantItem::text("said", 0, "answer")]);
        let state = |date: &str| crate::session::RuntimeState {
            date: date.into(),
            jobs: Vec::new(),
            todos: Vec::new(),
            location: crate::execution::ExecutionLocation::root("/".into()),
        };
        // The runtime ends a request with its state; a state followed by anything was
        // never sent.
        let tail = |date: &str| {
            let state = Message::State(state(date));
            match date {
                "stale" => vec![state, text_message("request only")],
                _ => vec![text_message("request only"), state],
            }
        };
        let hello = append(&runtime, committed(text_message("hello"))).await;
        let agent_context = context(ModelPurpose::Agent, "provider", "model", "system");
        let agent_context = append(&runtime, agent_context).await;
        let requested = |through: RecordSeq, date: &str| SessionEvent::ModelRequested {
            context: agent_context,
            checkpoint: None,
            through: Some(through.message()),
            from: None,
            tail: tail(date),
            history_lifetime: HistoryLifetime::Extends,
        };
        let attempt = |request: RecordSeq| AttemptRef {
            request: request.request(),
            attempt: 1,
        };
        let respond = async |request: RecordSeq, outcome| {
            append(
                &runtime,
                SessionEvent::ModelAttemptStarted(attempt(request)),
            )
            .await;
            let message = append(&runtime, committed(answer.clone())).await;
            let completed = SessionEvent::ResponseCompleted {
                attempt: attempt(request),
                message: message.message(),
                outcome,
            };
            append(&runtime, completed).await;
            message
        };
        let one = append(&runtime, requested(hello, "one")).await;
        let answered = respond(one, crate::session::CompletedOutcome::Answer).await;
        let more = append(&runtime, committed(text_message("more"))).await;
        append(&runtime, requested(more, "unanswered")).await;
        let stale = append(&runtime, requested(more, "stale")).await;
        let cut = crate::session::CompletedOutcome::Cut(crate::session::Truncation::MaxTokens);
        let cut = respond(stale, cut).await;
        let three = append(&runtime, requested(cut, "three")).await;
        let projection = projected(store, agent).await;
        let sent: Vec<_> = (projection.messages.iter())
            .map(|projected| projected.sent.as_ref().map(|state| state.date.as_str()))
            .collect();
        assert_eq!(sent, [None, Some("one"), None, None]);
        assert_eq!(get(&[hello, answered, more, cut]), sequences(&projection));
        let request = store.reconstruct_model_request(three.request());
        let (_, request) = request.await.unwrap();
        assert_eq!(request.context, projection.context(&tail("three")));
        let lifetimes: Vec<_> = request.states().map(|state| state.lifetime).collect();
        use crate::provider::protocol::StateLifetime::*;
        assert_eq!(lifetimes, [Superseded, Current]);
        let only = serde_json::to_string(&request.context).unwrap();
        assert_eq!(only.matches("request only").count(), 1);
        // After a mode switch only later responses carry their request's state.
        append(&runtime, switch_mode()).await;
        let four = append(&runtime, requested(cut, "four")).await;
        respond(four, crate::session::CompletedOutcome::Answer).await;
        let switched = projected(store, agent).await;
        let sent: Vec<_> = switched.history().filter_map(|(_, sent)| sent).collect();
        assert_eq!(sent, [&state("four")]);
    }

    /// The trim keeps the opening exchange and resumes at the user or assistant message
    /// nearest the middle of the rest, never at tool results, and finds no cut when one
    /// exchange holds everything after the opening. Reconstruction accepts only its cuts.
    #[test]
    fn trimming_keeps_the_opening_exchange_and_cuts_nearest_the_middle() {
        let call = |id: &str| {
            let call = ToolCall::new(id, "read", json!({})).unwrap();
            Message::Assistant(vec![AssistantItem::tool_call(id, 0, call)])
        };
        let result = |id: &str| {
            Message::Tool(ToolResult {
                call_id: id.into(),
                name: "read".into(),
                result: json!("ok"),
                images: vec![],
                is_error: false,
            })
        };
        let projection = |messages: &[Message]| Projection {
            checkpoint: None,
            messages: (1u64..)
                .zip(messages.iter().cloned())
                .map(|(sequence, message)| Projected::unbound(sequence.into(), message))
                .collect(),
        };
        let trimmed = |messages: &[Message]| {
            let mut projection = projection(messages);
            let from = projection.trim().map(|from| RecordSeq::from(from).get());
            let kept: Vec<_> = (projection.messages.iter())
                .map(|kept| kept.sequence.get())
                .collect();
            (from, kept)
        };
        // The older side holds only a distant cut; the nearer one is newer.
        let mut long = vec![text_message("prompt"), text_message("early"), call("a")];
        long.extend(std::iter::repeat_with(|| result("a")).take(8));
        long.extend([call("b"), result("b"), text_message("late")]);
        assert_eq!(trimmed(&long), (Some(12), vec![1, 12, 13, 14]));
        // A call that opens the history keeps its results.
        let opened = [
            call("a"),
            result("a"),
            text_message("next"),
            text_message("last"),
        ];
        assert_eq!(trimmed(&opened), (Some(4), vec![1, 2, 4]));
        // One exchange after the opening message cannot be split.
        let single = [text_message("prompt"), call("a"), result("a"), result("a")];
        assert_eq!(trimmed(&single), (None, vec![1, 2, 3, 4]));
        // Reconstruction resumes only where a trim could have cut.
        for from in [2u64, 3] {
            assert!(!projection(&opened).trim_from(RecordSeq::from(from).message()));
        }
        assert!(projection(&opened).trim_from(RecordSeq::from(4).message()));
    }

    #[tokio::test]
    async fn bound_reasoning_does_not_survive_compaction() {
        let replay = |binding| {
            envelope(
                ReplayFormat::Messages,
                "model",
                json!({"signature":"opaque"}),
                binding,
            )
        };
        let (portable, bound) = (replay(Binding::Free), replay(Binding::Conversation));
        let message = |replay| {
            Message::Assistant(vec![
                AssistantItem::reasoning("bound", 0, "visible", replay),
                AssistantItem::reasoning("portable", 1, "kept", Some(portable.clone())),
                AssistantItem::text("said", 2, "answer"),
            ])
        };
        let (signed, stripped) = (message(Some(bound)), message(None));
        let (runtime, journaled) = projection_fixture(signed.clone()).await;
        let (store, agent) = (&runtime.store, &runtime.agent);
        let replayed = async |request: RecordSeq| {
            let replayed = store.reconstruct_model_request(request.request());
            replayed.await.unwrap().1
        };
        // Retained after each checkpoint, and sent without it by both summary requests.
        assert_eq!(projected(store, agent).await.messages[1].message, stripped);
        for (request, index) in [(journaled.first_summary, 0), (journaled.second_summary, 1)] {
            assert_eq!(
                replayed(request).await.context[index],
                stripped.entry(StateLifetime::Persisted)
            );
        }
        // Bound reasoning after the latest checkpoint is kept; replay matches projection.
        let latest = append(&runtime, committed(signed.clone())).await;
        let agent_context = context(ModelPurpose::Agent, "provider", "model", "system");
        let agent_context = append(&runtime, agent_context).await;
        let lifetime = HistoryLifetime::Extends;
        let (plan, steering) = (journaled.plan, journaled.steering);
        let state = |sources: &[RecordSeq]| {
            requested_history(
                agent_context,
                Some(journaled.second),
                sources,
                "state",
                lifetime,
            )
        };
        let before = append(&runtime, state(&[plan, steering, latest])).await;
        let projection = projected(store, agent).await;
        assert_eq!(projection.messages[3].message, signed);
        let replays = |request: &ModelRequest, projection: &Projection| {
            (request.context).starts_with(&render_history(projection.history()))
        };
        assert!(replays(&replayed(before).await, &projection));
        // A mode switch changes the conversation too: bound reasoning before it is
        // dropped from later requests, while the earlier request replays as sent.
        append(&runtime, switch_mode()).await;
        let after = append(&runtime, committed(signed.clone())).await;
        let switched_request = state(&[plan, steering, latest, after]);
        let switched_request = append(&runtime, switched_request).await;
        let switched = projected(store, agent).await;
        assert_eq!(
            (&switched.messages[3].message, &switched.messages[4].message),
            (&stripped, &signed)
        );
        assert!(replays(&replayed(switched_request).await, &switched));
        assert!(replays(&replayed(before).await, &projection));
    }
}
