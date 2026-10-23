//! Metadata-only request rows and request timing.

use super::{Entry, Timing, clean, number};
use crate::tui::format::{Precision, local_time};
use chrono::{Local, NaiveDate};
use skyhook::provider::protocol::Usage;
use skyhook::session::{ModelPurpose, RequestNumber, RequestPhase, RequestRecord, RequestSeq};

#[derive(Clone, PartialEq, Eq)]
pub struct RequestRow {
    pub sequence: RequestSeq,
    pub number: RequestNumber,
    pub purpose: ModelPurpose,
    pub model: String,
    pub phase: RequestPhase,
    pub usage: Usage,
    /// When the request was sent, on the local clock.
    pub sent: String,
    pub timing: Timing,
}

impl RequestRow {
    pub fn metadata(&self) -> [String; 5] {
        [
            format!("Request #{}", self.number),
            self.sent.clone(),
            match self.purpose {
                ModelPurpose::Agent => "Agent",
                ModelPurpose::Compaction => "Compaction",
            }
            .into(),
            self.model.clone(),
            match self.phase {
                RequestPhase::Requested | RequestPhase::Open { .. } => "Running",
                RequestPhase::Retrying { .. } => "Retrying",
                RequestPhase::Failed { .. } => "Failed",
                RequestPhase::Interrupted { .. } => "Interrupted",
                RequestPhase::Completed { .. } => "Completed",
            }
            .into(),
        ]
    }

    pub fn statistics(&self) -> [String; 4] {
        let tokens = |count: Option<u64>| count.map_or_else(|| "—".into(), number);
        [
            tokens(self.usage.output_tokens),
            tokens(self.usage.input_tokens),
            tokens(self.usage.cached_input_tokens),
            self.timing.took().unwrap_or_default(),
        ]
    }
}

pub(super) fn request_entry(
    sequence: RequestSeq,
    record: &RequestRecord,
    today: NaiveDate,
) -> Entry {
    Entry::request_entry(RequestRow {
        sequence,
        number: record.number,
        purpose: record.purpose,
        model: clean(record.profile.profile.model.as_str()).replace('\n', " "),
        phase: record.phase.clone(),
        usage: record.usage,
        sent: local_time(record.requested_millis, today, &Local, Precision::Seconds),
        timing: request_timing(record),
    })
}

/// From the request until it settled, counting while it may still produce an outcome.
pub(super) fn request_timing(record: &RequestRecord) -> Timing {
    let start = record.requested_millis;
    match record.phase.settled_at() {
        Some(until) => Timing::Took {
            since: start,
            until,
        },
        None => Timing::Since(start),
    }
}
