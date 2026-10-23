//! The viewed agent's transcript as far as it is loaded: one run of consecutive entries,
//! paged by ordinal as the lists are, and what presenting them reads, as their pages and
//! the updates since leave it.

use super::entries::{Dep, presents};
use super::window::{List, PAGE};
use skyhook::identity::JobId;
use skyhook::session::{
    EventRecord, Fact, JobEvent, Message, Page, PageFacts, RecordSeq, RequestRecord, RequestSeq,
    SessionEvent, TranscriptPage, UserPart,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

#[derive(Default)]
pub struct Transcript {
    /// The ordinals read, and those kept: the records the conversation can show.
    pub run: List<u64>,
    records: HashMap<u64, Arc<EventRecord>>,
    /// What presenting the loaded records reads; their requests are the window's.
    pub facts: PageFacts,
}

impl Transcript {
    /// The loaded record at `ordinal`.
    pub fn record(&self, ordinal: u64) -> Option<&Arc<EventRecord>> {
        self.records.get(&ordinal)
    }

    /// The loaded records in ordinal order.
    pub fn records(&self) -> impl Iterator<Item = (u64, &Arc<EventRecord>)> + '_ {
        (self.run.kept().iter()).map(|ordinal| (*ordinal, &self.records[ordinal]))
    }

    /// Add what `page` read where it adjoins what is loaded, and what presenting it
    /// reads; returns the requests it read.
    pub(super) fn place(
        &mut self,
        page: Page<u64>,
        read: TranscriptPage,
    ) -> BTreeMap<RequestSeq, RequestRecord> {
        let TranscriptPage {
            first,
            records,
            facts,
        } = read;
        // What the updates since the read changed stays.
        let PageFacts {
            requests,
            answers,
            results,
            admitted,
            reported,
        } = facts;
        for (message, answer) in answers {
            self.facts.answers.entry(message).or_insert(answer);
        }
        for (call, result) in results {
            self.facts.results.entry(call).or_insert(result);
        }
        self.facts.admitted.extend(admitted);
        self.facts.reported.extend(reported);
        let full = records.len() == PAGE;
        let read: Vec<_> = (first..).zip(records).collect();
        let kept = read
            .iter()
            .map(|(ordinal, record)| (*ordinal, presents(&record.event)));
        if self.run.insert(page, kept, full) {
            // A page from an end reads the transcript afresh.
            if matches!(page, Page::First | Page::Last) {
                self.records.clear();
            }
            for (ordinal, record) in read {
                if presents(&record.event) {
                    self.records.entry(ordinal).or_insert(record);
                }
            }
        }
        requests
    }

    /// Fold the agent's entry `ordinal` from the updates, which the live slice decided
    /// `fact` of, its open turn having begun at `turn`: loaded when it follows the loaded
    /// end, and kept for what presenting the loaded records reads, or every record when
    /// `reading` a page that may hold them. Returns whether it was loaded, and the call
    /// it answered or admitted.
    pub(super) fn observe(
        &mut self,
        ordinal: u64,
        record: &Arc<EventRecord>,
        (fact, turn): (Option<Fact>, Option<i64>),
        reading: bool,
    ) -> (bool, Option<Dep>) {
        let placed = self.run.push(ordinal, presents(&record.event));
        if placed {
            self.records.insert(ordinal, record.clone());
        }
        let (run, records) = (self.run.kept(), &self.records);
        let holds = |sequence| reading || held(run, records, sequence);
        let call = self.facts.observe(record, fact, turn, holds);
        (placed, call.map(|(message, call)| Dep::Call(message, call)))
    }

    /// Let go of the loaded records beyond those kept around the view's `shown` ones.
    pub(super) fn trim(&mut self, shown: std::ops::Range<usize>) -> bool {
        if !self.run.trim(shown) {
            return false;
        }
        let run = self.run.kept();
        (self.records).retain(|ordinal, _| run.binary_search(ordinal).is_ok());
        true
    }

    /// Let go of what presenting records not loaded read, kept while a page that may
    /// hold them was in flight.
    pub(super) fn retain_facts(&mut self) {
        let (run, records, facts) = (self.run.kept(), &self.records, &mut self.facts);
        let held = |sequence| held(run, records, sequence);
        let message = |message: &skyhook::session::MessageSeq| held((*message).into());
        facts.answers.retain(|answer, _| message(answer));
        facts.results.retain(|(call, _), _| message(call));
        facts.admitted.retain(|(call, _)| message(call));
        facts.reported.retain(|failed| held(*failed));
    }

    /// The jobs the loaded records show.
    pub(super) fn shown_jobs(&self) -> HashSet<JobId> {
        self.records
            .values()
            .flat_map(|record| shown(record))
            .collect()
    }

    /// The requests the loaded records make or answer.
    pub(super) fn shown_requests(&self) -> HashSet<RequestSeq> {
        let made = self
            .records
            .values()
            .filter_map(|record| match record.event {
                SessionEvent::ModelRequested { .. } => Some(record.sequence.request()),
                _ => None,
            });
        let answered = self.facts.answers.values().map(|answer| answer.request);
        made.chain(answered).collect()
    }
}

/// Whether `records`, loaded at the ordinals `run`, hold the record at `sequence`: an
/// agent's records are in sequence order, as their ordinals are.
fn held(run: &[u64], records: &HashMap<u64, Arc<EventRecord>>, sequence: RecordSeq) -> bool {
    (run.binary_search_by_key(&sequence, |ordinal| records[ordinal].sequence)).is_ok()
}

/// The jobs `record` shows: one it creates, and those its notifications name.
pub fn shown(record: &EventRecord) -> Vec<JobId> {
    match &record.event {
        SessionEvent::JobCreated { job, .. } => vec![*job],
        SessionEvent::MessageCommitted {
            message: Message::User(parts),
        } => {
            let events = parts.iter().flat_map(|part| match part {
                UserPart::JobEvents { events } => events.as_slice(),
                _ => &[],
            });
            events.filter_map(JobEvent::job).collect()
        }
        _ => Vec::new(),
    }
}
