//! One agent's model-visible history: the journal's committed messages after its
//! latest checkpoint, each with the state its request sent.
use crate::{
    identity::AgentId,
    provider::protocol::{Entry, State, StateLifetime},
    session::{Message, RuntimeState},
};

use super::{EventRecord, RequestLedger, SessionEvent, request::record_at};
use crate::session::{MessageSeq, RecordSeq};

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

    /// A message committed at `sequence`, keeping what binds it to the conversation.
    pub(super) fn bound(
        ledger: &RequestLedger,
        records: &[EventRecord],
        sequence: RecordSeq,
        message: &Message,
    ) -> Self {
        Self {
            sequence,
            message: message.clone(),
            sent: sent_state(ledger, records, sequence),
        }
    }
}

impl Projection {
    /// The last committed message after the checkpoint, which bounds a request's history.
    pub fn through(&self) -> Option<MessageSeq> {
        let start = usize::from(self.checkpoint.is_some());
        let projected = self.messages[start..].last()?;
        Some(projected.sequence.message())
    }

    /// Each message with the state sent before it.
    pub fn history(&self) -> impl Iterator<Item = (&Message, Option<&RuntimeState>)> {
        (self.messages.iter()).map(|projected| (&projected.message, projected.sent.as_ref()))
    }

    /// Add what the journal committed for `agent` among `window`, the records after the
    /// last refresh; `records` is the whole journal and `ledger` its request fold. A
    /// mode switch unbinds what was committed before it.
    pub(crate) fn extend(
        &mut self,
        ledger: &RequestLedger,
        records: &[EventRecord],
        window: &[EventRecord],
        agent: &AgentId,
    ) {
        for record in window.iter().filter(|record| &record.agent == agent) {
            match &record.event {
                SessionEvent::MessageCommitted { message } => (self.messages)
                    .push(Projected::bound(ledger, records, record.sequence, message)),
                SessionEvent::ModeChanged { .. } => self.unbind(),
                _ => {}
            }
        }
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

/// The runtime state the request that produced the response committed at `message` sent,
/// attributed as the request ledger attributes the response. Only a state ending its
/// request was sent: anything after it in the tail would have made it stale.
fn sent_state(
    ledger: &RequestLedger,
    records: &[EventRecord],
    message: RecordSeq,
) -> Option<RuntimeState> {
    let requested = ledger
        .request_of(message.message())
        .and_then(|request| record_at(records, request.into()));
    let Some(SessionEvent::ModelRequested { tail, .. }) = requested.map(|record| &record.event)
    else {
        return None;
    };
    match tail.last()? {
        Message::State(state) => Some(state.clone()),
        Message::User(_) | Message::Assistant(_) | Message::Tool(_) => None,
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

/// Project the latest committed compaction and subsequent messages for one agent.
/// Checkpoints were validated when appended or loaded, so a journal always projects.
#[must_use]
pub fn project_history(records: &[EventRecord], agent: &AgentId) -> Projection {
    let checkpoint = records.iter().rposition(|record| {
        &record.agent == agent && matches!(record.event, SessionEvent::Compaction { .. })
    });
    project_from(records, agent, checkpoint)
}

/// Project the compaction at `checkpoint`, an index into `records`, and the messages
/// `agent` committed after its frontier.
pub(super) fn project_from(
    records: &[EventRecord],
    agent: &AgentId,
    checkpoint: Option<usize>,
) -> Projection {
    let mut result = Projection::default();
    let frontier = if let Some(index) = checkpoint {
        let record = &records[index];
        let SessionEvent::Compaction { checkpoint } = &record.event else {
            unreachable!()
        };
        result.checkpoint = Some(record.sequence);
        result.messages.push(Projected::unbound(
            record.sequence,
            checkpoint.message.clone(),
        ));
        for sequence in &checkpoint.retained {
            let source = record_at(&records[..index], (*sequence).into())
                .expect("retained sources were validated when the checkpoint was appended");
            let SessionEvent::MessageCommitted { message } = &source.event else {
                unreachable!()
            };
            result
                .messages
                .push(Projected::unbound((*sequence).into(), message.clone()));
        }
        checkpoint.frontier
    } else {
        RecordSeq::default()
    };
    let switched = mode_boundary(records, agent);
    let ledger = RequestLedger::of(records);
    for record in records {
        if &record.agent == agent
            && record.sequence > frontier
            && let SessionEvent::MessageCommitted { message } = &record.event
        {
            result.messages.push(if record.sequence < switched {
                Projected::unbound(record.sequence, message.clone())
            } else {
                Projected::bound(&ledger, records, record.sequence, message)
            });
        }
    }
    result
}

/// A mode switch replaces the system prompt and tools, which invalidates bound
/// reasoning before it exactly as compaction does.
fn mode_boundary(records: &[EventRecord], agent: &AgentId) -> RecordSeq {
    let switched = records.iter().rev().find(|record| {
        &record.agent == agent && matches!(record.event, SessionEvent::ModeChanged { .. })
    });
    switched.map_or(RecordSeq::default(), |record| record.sequence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        identity::AgentId,
        provider::codec::common::tests::envelope,
        provider::protocol::{
            AssistantItem, Binding, HistoryLifetime, ReplayFormat, ToolCall, ToolResult,
        },
        session::{
            AttemptRef, CompactionCheckpoint, CompactionTrigger, ModelPurpose, ReplayError,
            SessionError, reconstruct_model_request,
            tests::{committed, context, requested_history, sequences, text_message},
        },
    };
    use serde_json::json;

    fn projection_fixture() -> (AgentId, Vec<EventRecord>) {
        let agent = AgentId::root(crate::identity::SessionId::generate().unwrap());
        let checkpoint = CompactionCheckpoint {
            todos: Vec::new(),
            frontier: 2.into(),
            message: text_message("first summary"),
            retained: vec![1.into()],
            attempt: AttemptRef {
                request: 5.into(),
                attempt: 1,
            },
            trigger: CompactionTrigger::Occupancy(100),
        };
        let summary = |checkpoint: Option<u64>, history: &[u64], tail| {
            let history: Vec<RecordSeq> = history.iter().copied().map(Into::into).collect();
            let checkpoint = checkpoint.map(Into::into);
            requested_history(
                4.into(),
                checkpoint,
                &history,
                tail,
                HistoryLifetime::Detached,
            )
        };
        let events = vec![
            committed(text_message("verbatim plan")),
            committed(text_message("research")),
            context(ModelPurpose::Agent, "provider", "model", "system"),
            context(ModelPurpose::Compaction, "provider", "model", "system"),
            summary(None, &[1, 2], "summarize"),
            committed(text_message("concurrent steering")),
            SessionEvent::Compaction {
                checkpoint: checkpoint.clone(),
            },
            committed(text_message("continued work")),
            summary(Some(7), &[1, 6, 8], "state at request time"),
            SessionEvent::Compaction {
                checkpoint: CompactionCheckpoint {
                    frontier: 8.into(),
                    message: text_message("second summary with inherited plan"),
                    retained: vec![1.into(), 6.into()],
                    attempt: AttemptRef {
                        request: 9.into(),
                        attempt: 1,
                    },
                    ..checkpoint
                },
            },
        ];
        let records = events
            .into_iter()
            .enumerate()
            .map(|(index, event)| crate::session::tests::record(&agent, index as u64 + 1, event))
            .collect();
        (agent, records)
    }

    #[test]
    fn projection_preserves_concurrent_messages_and_repeated_compaction() {
        let (agent, records) = projection_fixture();
        let early = project_history(&records[..6], &agent);
        assert_eq!((early.checkpoint, sequences(&early)), (None, vec![1, 2, 6]));
        assert_eq!(early.through(), Some(RecordSeq::from(6).message()));
        let first = project_history(&records[..8], &agent);
        assert_eq!(
            (first.checkpoint, sequences(&first)),
            (Some(7.into()), vec![7, 1, 6, 8])
        );
        assert_eq!(first.through(), Some(RecordSeq::from(8).message()));
        assert_eq!(
            (&first.messages[1].message, &first.messages[2].message),
            (
                &text_message("verbatim plan"),
                &text_message("concurrent steering")
            )
        );
        let second = project_history(&records, &agent);
        assert_eq!(
            (second.checkpoint, sequences(&second)),
            (Some(10.into()), vec![10, 1, 6])
        );
        assert_eq!(second.messages[1].message, text_message("verbatim plan"));
        assert!(
            project_history(&records, &agent.child(1))
                .messages
                .is_empty()
        );
        // Reconstruction resolves the same checkpoint and history bound, and rejects a
        // checkpoint that is not the agent's, or a bound its history does not hold.
        use ReplayError::{Checkpoint, Through};
        for (checkpoint, through, expected) in [
            (3u64, 8u64, Some(Checkpoint)),
            (9, 8, Some(Checkpoint)),
            (7, 1, None),
            (7, 2, Some(Through)),
            (7, 4, Some(Through)),
            (7, 20, Some(Through)),
        ] {
            let mut invalid = records.clone();
            let SessionEvent::ModelRequested {
                checkpoint: named,
                through: bound,
                ..
            } = &mut invalid[8].event
            else {
                unreachable!()
            };
            *named = Some(checkpoint.into());
            *bound = Some(RecordSeq::from(through).message());
            let replayed = reconstruct_model_request(&invalid, 9.into());
            match expected {
                None => assert!(replayed.is_ok()),
                Some(expected) => assert!(matches!(
                    replayed,
                    Err(SessionError::ModelRequestReplay { reason, .. }) if reason == expected
                )),
            }
        }
    }

    /// A response carries the state its request sent, whatever its outcome, alike in
    /// projection and reconstruction: only the state, never the rest of that tail;
    /// nothing for a request no response answered or whose state something followed,
    /// and nothing before a mode switch.
    #[test]
    fn responses_carry_the_state_their_request_sent() {
        let agent = AgentId::root(crate::identity::SessionId::generate().unwrap());
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
        let requested = |through: &[u64], date: &str| SessionEvent::ModelRequested {
            context: 2.into(),
            checkpoint: None,
            through: through.last().map(|last| RecordSeq::from(*last).message()),
            from: None,
            tail: tail(date),
            history_lifetime: HistoryLifetime::Extends,
        };
        let attempt = |request: u64| {
            SessionEvent::ModelAttemptStarted(AttemptRef {
                request: RecordSeq::from(request).request(),
                attempt: 1,
            })
        };
        let completed = |request: u64, message: u64, outcome| SessionEvent::ResponseCompleted {
            attempt: AttemptRef {
                request: RecordSeq::from(request).request(),
                attempt: 1,
            },
            message: RecordSeq::from(message).message(),
            outcome,
        };
        let cut = crate::session::CompletedOutcome::Cut(crate::session::Truncation::MaxTokens);
        let mut records: Vec<_> = [
            committed(text_message("hello")),
            context(ModelPurpose::Agent, "provider", "model", "system"),
            requested(&[1], "one"),
            attempt(3),
            committed(answer.clone()),
            completed(3, 5, crate::session::CompletedOutcome::Answer),
            committed(text_message("more")),
            requested(&[1, 5, 7], "unanswered"),
            requested(&[1, 5, 7], "stale"),
            attempt(9),
            committed(answer.clone()),
            completed(9, 11, cut),
            requested(&[1, 5, 7, 11], "three"),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, event)| crate::session::tests::record(&agent, index as u64 + 1, event))
        .collect();
        let projected = project_history(&records, &agent);
        let sent: Vec<_> = (projected.messages.iter())
            .map(|projected| projected.sent.as_ref().map(|state| state.date.as_str()))
            .collect();
        assert_eq!(sent, [None, Some("one"), None, None]);
        let (_, request) = reconstruct_model_request(&records, 13.into()).unwrap();
        assert_eq!(request.context, projected.context(&tail("three")));
        let lifetimes: Vec<_> = request.states().map(|state| state.lifetime).collect();
        use crate::provider::protocol::StateLifetime::*;
        assert_eq!(lifetimes, [Superseded, Current]);
        let only = serde_json::to_string(&request.context).unwrap();
        assert_eq!(only.matches("request only").count(), 1);
        // After a mode switch only later responses carry their request's state.
        let mode = crate::session::ModeSelection {
            name: "plan".parse().unwrap(),
            definition: None,
        };
        let capabilities = Vec::new();
        let switch = SessionEvent::ModeChanged { mode, capabilities };
        for event in [
            switch,
            requested(&[1, 5, 7, 11], "four"),
            attempt(15),
            committed(answer),
        ] {
            let sequence = records.len() as u64 + 1;
            records.push(crate::session::tests::record(&agent, sequence, event));
        }
        let switched = project_history(&records, &agent);
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

    #[test]
    fn bound_reasoning_does_not_survive_compaction() {
        let (agent, mut records) = projection_fixture();
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
        records[0].event = committed(signed.clone());
        // Retained after each checkpoint, and sent without it by both summary requests.
        assert_eq!(
            project_history(&records, &agent).messages[1].message,
            stripped
        );
        for (request, index) in [(5, 0), (9, 1)] {
            let (_, request) = reconstruct_model_request(&records, request.into()).unwrap();
            assert_eq!(
                request.context[index],
                stripped.entry(StateLifetime::Persisted)
            );
        }
        // Bound reasoning after the latest checkpoint is kept; replay matches projection.
        let append = |records: &mut Vec<EventRecord>, sequence: u64, event: SessionEvent| {
            let mut record = records[8].clone();
            (record.sequence, record.event) = (sequence.into(), event);
            records.push(record);
        };
        append(&mut records, 11, committed(signed.clone()));
        let lifetime = HistoryLifetime::Extends;
        let state = |sources: &[u64]| {
            let sources: Vec<RecordSeq> = sources.iter().copied().map(Into::into).collect();
            requested_history(3.into(), Some(10.into()), &sources, "state", lifetime)
        };
        append(&mut records, 12, state(&[1, 6, 11]));
        let projected = project_history(&records, &agent);
        assert_eq!(projected.messages[3].message, signed);
        let replays_projection = |records: &[EventRecord], request: u64, projected: &Projection| {
            let (_, request) = reconstruct_model_request(records, request.into()).unwrap();
            assert!(
                request
                    .context
                    .starts_with(&render_history(projected.history()))
            );
        };
        replays_projection(&records, 12, &projected);
        // A mode switch changes the conversation too: bound reasoning before it is
        // dropped from later requests, while the earlier request replays as sent.
        let mode = crate::session::ModeSelection {
            name: "plan".parse().unwrap(),
            definition: None,
        };
        let capabilities = Vec::new();
        append(
            &mut records,
            13,
            SessionEvent::ModeChanged { mode, capabilities },
        );
        append(&mut records, 14, committed(signed.clone()));
        append(&mut records, 15, state(&[1, 6, 11, 14]));
        let switched = project_history(&records, &agent);
        assert_eq!(
            (&switched.messages[3].message, &switched.messages[4].message),
            (&stripped, &signed)
        );
        replays_projection(&records, 15, &switched);
        replays_projection(&records, 12, &projected);
    }
}
