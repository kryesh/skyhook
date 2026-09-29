//! Each agent's turn as the journal records it, shared by statistics and hosts.
use std::collections::HashMap;

use crate::{
    identity::AgentId,
    session::{CompletedOutcome, EventRecord, ModelPurpose, RequestLedger, SessionEvent},
};

/// Agents inside a turn, with when it began (epoch milliseconds): from the first
/// model request after the last turn ended, until a response ends it without
/// calling tools or the agent stops.
#[derive(Clone, Debug, Default)]
pub struct Turns(HashMap<AgentId, i64>);

impl Turns {
    /// Fold one record after `ledger` has, returning when the turn it ended began.
    pub fn observe(&mut self, record: &EventRecord, ledger: &RequestLedger) -> Option<i64> {
        let agent = &record.agent;
        match &record.event {
            SessionEvent::ModelRequested { .. } => {
                let request = ledger.get(record.sequence.request());
                if request.is_some_and(|request| request.purpose == ModelPurpose::Agent) {
                    self.0
                        .entry(agent.clone())
                        .or_insert(record.timestamp_millis);
                }
                None
            }
            SessionEvent::ResponseCompleted { outcome, .. }
                if *outcome != CompletedOutcome::ToolUse =>
            {
                self.0.remove(agent)
            }
            SessionEvent::AgentCompleted
            | SessionEvent::AgentInterrupted
            | SessionEvent::AgentFailed { .. } => self.0.remove(agent),
            _ => None,
        }
    }

    /// When the agent's open turn began.
    #[must_use]
    pub fn started(&self, agent: &AgentId) -> Option<i64> {
        self.0.get(agent).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Failure,
        identity::SessionId,
        session::{
            AttemptRef, ModelContext, RecordSeq, RequestSeq,
            tests::{profile, record, requested},
        },
    };

    /// One agent's records folded into a ledger and its turns, each stamped with
    /// its sequence.
    struct Fold {
        agent: AgentId,
        ledger: RequestLedger,
        turns: Turns,
        next: u64,
    }

    impl Fold {
        /// Fold `event`, returning its sequence and when the turn it ended began.
        fn observe(&mut self, event: SessionEvent) -> (RecordSeq, Option<i64>) {
            let mut record = record(&self.agent, self.next, event);
            record.timestamp_millis = self.next as i64;
            self.next += 1;
            self.ledger.observe(&record);
            (record.sequence, self.turns.observe(&record, &self.ledger))
        }

        /// A request for `purpose`, and when it was made.
        fn request(&mut self, purpose: ModelPurpose) -> (RequestSeq, i64) {
            let context = ModelContext::test(purpose, profile());
            let (context, _) = self.observe(SessionEvent::ModelContext { context });
            let (request, _) = self.observe(requested(context));
            (request.request(), request.get() as i64)
        }

        fn complete(&mut self, request: RequestSeq, outcome: CompletedOutcome) -> Option<i64> {
            let completed = SessionEvent::ResponseCompleted {
                attempt: AttemptRef {
                    request,
                    attempt: 1,
                },
                message: RecordSeq::from(self.next).message(),
                outcome,
            };
            self.observe(completed).1
        }

        fn started(&self) -> Option<i64> {
            self.turns.started(&self.agent)
        }
    }

    /// A turn starts at the first agent request after the last one ended, runs
    /// through tool use, and ends with an answer or when the agent stops.
    #[test]
    fn turns_run_from_an_agent_request_until_an_answer_or_a_stop() {
        let mut fold = Fold {
            agent: AgentId::root(SessionId::from_bytes([3; 16])),
            ledger: RequestLedger::default(),
            turns: Turns::default(),
            next: 1,
        };
        fold.request(ModelPurpose::Compaction);
        assert_eq!(fold.started(), None);
        let (first, start) = fold.request(ModelPurpose::Agent);
        assert_eq!(fold.complete(first, CompletedOutcome::ToolUse), None);
        let (second, _) = fold.request(ModelPurpose::Agent);
        assert_eq!(fold.started(), Some(start));
        assert_eq!(fold.complete(second, CompletedOutcome::Answer), Some(start));
        assert_eq!(fold.started(), None);
        let failed = SessionEvent::AgentFailed {
            failure: Failure::Other("boom".into()),
        };
        for stop in [SessionEvent::AgentInterrupted, failed] {
            let (_, start) = fold.request(ModelPurpose::Agent);
            assert_eq!(fold.observe(stop).1, Some(start));
            assert_eq!(fold.started(), None);
        }
    }
}
