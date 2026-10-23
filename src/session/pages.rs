//! A session's history read a page at a time, each page in one snapshot of the
//! journal. A host subscribes first ([`SessionStore::observe`]), then reads pages: a
//! page sees at least what the observation applied, so a host that places updates by
//! their ordinals (counted from the observation's `entries`) skips those a page holds
//! and misses none.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use super::{
    EventRecord, Fact, Message, MessageSeq, RecordSeq, RequestRecord, RequestSeq, SessionError,
    SessionEvent, SessionStore, db,
};
use crate::identity::{AgentId, JobId};
use crate::provider::protocol::ToolResult;

/// Where a page of an ordered list starts. Keys bound a page and are never on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page<K> {
    First,
    After(K),
    Before(K),
    Last,
}

/// Consecutive entries of one agent's transcript, in order.
#[derive(Clone, Debug)]
pub struct TranscriptPage {
    /// The ordinal of the first entry; where the page would start when it is empty.
    pub first: u64,
    pub records: Vec<Arc<EventRecord>>,
    /// What presenting the records reads of the journal beyond them.
    pub facts: PageFacts,
}

/// What presenting a page's records reads beyond them, keyed by the records it
/// concerns, as the journal leaves it when the page is read. The updates after the
/// read extend it through [`PageFacts::observe`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageFacts {
    /// The requests its records make or answer, as their rows hold them.
    pub requests: BTreeMap<RequestSeq, RequestRecord>,
    /// Each assistant message's request, and when the turn it answers began.
    pub answers: HashMap<MessageSeq, Answer>,
    /// The result that answered each call its assistant messages made, by message and
    /// call id.
    pub results: HashMap<(MessageSeq, String), ToolResult>,
    /// The calls its assistant messages made that started a job, whose card shows them.
    pub admitted: HashSet<(MessageSeq, String)>,
    /// Its failed turns that ended on a request's failure, whose card reports them.
    pub reported: HashSet<RecordSeq>,
}

/// The request an assistant message committed under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Answer {
    pub request: RequestSeq,
    /// When the turn it answers began (epoch milliseconds), if one was open.
    pub turn: Option<i64>,
}

impl PageFacts {
    /// Fold an update's record, which the live slice decided `fact` of (see
    /// [`Applied`](crate::agent::Applied)), its agent's open turn having begun at
    /// `turn`, into what presenting the records `holds` holds reads. Returns the call
    /// it answered or admitted.
    pub fn observe(
        &mut self,
        record: &EventRecord,
        fact: Option<Fact>,
        turn: Option<i64>,
        holds: impl Fn(RecordSeq) -> bool,
    ) -> Option<(MessageSeq, String)> {
        let at = record.sequence;
        match (&record.event, fact) {
            (SessionEvent::MessageCommitted { .. }, Some(Fact::Answers(request))) if holds(at) => {
                self.answers.insert(at.message(), Answer { request, turn });
                None
            }
            (
                SessionEvent::MessageCommitted {
                    message: Message::Tool(result),
                },
                Some(Fact::Answered(message)),
            ) if holds(message.into()) => {
                let call = (message, result.call_id.clone());
                self.results.insert(call.clone(), result.clone());
                Some(call)
            }
            (
                SessionEvent::JobCreated {
                    origin: Some(origin),
                    ..
                },
                _,
            ) if holds(origin.message.into()) => {
                let call = (origin.message, origin.call_id.clone());
                self.admitted.insert(call.clone());
                Some(call)
            }
            (SessionEvent::AgentFailed { .. }, Some(Fact::EndedOn)) if holds(at) => {
                self.reported.insert(at);
                None
            }
            _ => None,
        }
    }
}

/// Every agent the session started, each as the records that say what a host shows
/// of it, read through `through`.
#[derive(Clone, Debug, Default)]
pub struct AgentsPage {
    pub through: RecordSeq,
    pub agents: Vec<AgentSummary>,
}

/// One agent: its start, its latest model and mode changes, its latest todos (a
/// replacement or a checkpoint) and the latest record that ended a turn, in order.
/// Folding the records shows the agent as the whole journal would.
#[derive(Clone, Debug)]
pub struct AgentSummary {
    pub agent: AgentId,
    pub records: Vec<Arc<EventRecord>>,
    pub owner_job: Option<JobId>,
    /// When its latest finished turn began and ended (epoch milliseconds).
    pub finished: Option<(i64, i64)>,
    /// When its open turn began, if one is.
    pub turn: Option<i64>,
}

impl SessionStore {
    /// Up to `limit` consecutive entries of `agent`'s transcript at `page`, by ordinal.
    pub async fn transcript(
        &self,
        agent: &AgentId,
        page: Page<u64>,
        limit: usize,
    ) -> Result<TranscriptPage, SessionError> {
        let (session, agent) = (self.id(), agent.clone());
        let read = move |db: &db::Db| db::transcript(db, session, &agent, page, limit);
        self.snapshot(move |db| Ok(read(db)?)).await
    }

    /// Every agent the session started.
    pub async fn agents(&self) -> Result<AgentsPage, SessionError> {
        let session = self.id();
        self.snapshot(move |db| Ok(db::agent_summaries(db, session)?))
            .await
    }

    /// Whether `agent` journaled a failed turn after `after`.
    pub async fn failed_after(
        &self,
        agent: &AgentId,
        after: RecordSeq,
    ) -> Result<bool, SessionError> {
        let agent = agent.clone();
        self.snapshot(move |db| Ok(db::failed_after(db, &agent, after)?))
            .await
    }

    /// Up to `limit` of `agent`'s requests at `page`, in order, each as the journal
    /// leaves it.
    pub async fn requests(
        &self,
        agent: &AgentId,
        page: Page<RequestSeq>,
        limit: usize,
    ) -> Result<Vec<(RequestSeq, RequestRecord)>, SessionError> {
        let agent = agent.clone();
        self.snapshot(move |db| Ok(db::requests(db, &agent, page, limit)?))
            .await
    }
}

impl super::Journal {
    /// Up to `limit` of the jobs `owner` created that do not nest under a parent.
    pub(crate) fn root_jobs(
        &self,
        owner: &AgentId,
        page: Page<JobId>,
        limit: usize,
    ) -> Result<Vec<JobId>, SessionError> {
        Ok(self
            .reads
            .read(|db| db::root_jobs(db, owner, page, limit))?)
    }

    /// Up to `limit` of `parent`'s children.
    pub(crate) fn child_jobs(
        &self,
        parent: JobId,
        page: Page<JobId>,
        limit: usize,
    ) -> Result<Vec<JobId>, SessionError> {
        Ok(self
            .reads
            .read(|db| db::child_jobs(db, parent, page, limit))?)
    }

    /// The newest `limit` jobs `owner` created but those `except` names, in id
    /// order, and how many older ones there are.
    pub(crate) fn newest_jobs(
        &self,
        owner: &AgentId,
        except: &[JobId],
        limit: usize,
    ) -> Result<(Vec<JobId>, usize), SessionError> {
        Ok(self
            .reads
            .read(|db| db::newest_jobs(db, owner, except, limit))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agent::RuntimeEvent, session::SessionEvent, tests::TestRuntime};

    /// A page read after observing holds every entry the observation counted, and the
    /// updates after it carry each later one: a host placing updates by ordinal, counted
    /// from `entries`, skips those the page holds and misses none.
    #[tokio::test]
    async fn a_page_read_after_observing_holds_the_updates_it_skips() {
        let TestRuntime {
            root: _root,
            store,
            agent,
            ..
        } = TestRuntime::on_disk().await;
        let status = async |text: &str| {
            let message = format!("{text}\n{text}");
            let event = SessionEvent::Status { message };
            store.append(agent.clone(), event).await.unwrap()
        };
        status("before").await;
        let observation = store.observe();
        let mut ordinal = observation.snapshot.views.entries[&agent];
        let mut updates = observation.updates;
        status("observed").await;
        let page = store.transcript(&agent, Page::First, 64).await.unwrap();
        let last = status("read").await;
        let journal: Vec<_> = (store.records().into_iter())
            .filter(|record| record.agent == agent)
            .collect();
        let mut placed: std::collections::BTreeMap<_, _> = (page.first..)
            .zip(page.records.iter().map(|record| record.sequence))
            .collect();
        let mut skipped = 0;
        while let Ok(update) = updates.try_recv() {
            if let RuntimeEvent::Record(record) = update.event
                && record.agent == agent
            {
                assert!(record.sequence > observation.snapshot.applied);
                skipped += usize::from(placed.contains_key(&ordinal));
                let held = *placed.entry(ordinal).or_insert(record.sequence);
                assert_eq!(held, record.sequence);
                ordinal += 1;
            }
        }
        assert!(skipped > 0);
        let journaled = journal.iter().map(|record| record.sequence);
        assert!(placed.into_values().eq(journaled));
        let ordinal = journal.len() as u64 - 1;
        let tail = store.transcript(&agent, Page::Last, 2);
        let tail = tail.await.unwrap();
        assert_eq!((tail.first, tail.records.len()), (ordinal - 1, 2));
        assert_eq!(tail.records[1].sequence, last.sequence);
    }
}
