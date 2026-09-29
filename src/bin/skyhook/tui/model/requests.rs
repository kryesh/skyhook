//! Metadata-only request rows and request timing.

use super::{Entry, Timing, clean, number};
use crate::tui::format::{Precision, local_time};
use chrono::{Local, NaiveDate};
use skyhook::provider::protocol::Usage;
use skyhook::session::{ModelPurpose, RequestPhase, RequestRecord, RequestSeq};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestStatus {
    Failed,
    Running,
    Retrying,
    Completed,
    Interrupted,
}

impl RequestStatus {
    /// Explicit UI text; variant names never leak into rendered rows.
    pub fn label(self) -> &'static str {
        match self {
            Self::Failed => "Failed",
            Self::Running => "Running",
            Self::Retrying => "Retrying",
            Self::Completed => "Completed",
            Self::Interrupted => "Interrupted",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct RequestRow {
    pub sequence: RequestSeq,
    pub purpose: ModelPurpose,
    pub model: String,
    pub status: RequestStatus,
    /// None until an attempt reports usage.
    pub usage: Option<Usage>,
    /// When the request was sent, on the local clock.
    pub sent: String,
    pub timing: Timing,
}

impl RequestRow {
    pub fn metadata(&self) -> [String; 5] {
        [
            format!("Request #{}", self.sequence),
            self.sent.clone(),
            match self.purpose {
                ModelPurpose::Agent => "Agent",
                ModelPurpose::Compaction => "Compaction",
            }
            .into(),
            self.model.clone(),
            self.status.label().into(),
        ]
    }

    pub fn statistics(&self) -> [String; 4] {
        let tokens = |value: fn(Usage) -> String| self.usage.map_or_else(|| "—".into(), value);
        [
            tokens(|usage| number(usage.output_tokens)),
            tokens(|usage| number(usage.input_tokens)),
            tokens(|usage| number(usage.cached_input_tokens)),
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
        purpose: record.purpose,
        model: clean(record.profile.profile.model.as_str()).replace('\n', " "),
        status: request_status(&record.phase),
        usage: reported(record.usage),
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

fn request_status(phase: &RequestPhase) -> RequestStatus {
    match phase {
        RequestPhase::Requested | RequestPhase::Open { .. } => RequestStatus::Running,
        RequestPhase::Retrying { .. } => RequestStatus::Retrying,
        RequestPhase::Failed { .. } => RequestStatus::Failed,
        RequestPhase::Interrupted { .. } => RequestStatus::Interrupted,
        RequestPhase::Completed { .. } => RequestStatus::Completed,
    }
}

/// The runtime journals usage only once an attempt observed some.
fn reported(usage: Usage) -> Option<Usage> {
    (usage != Usage::default()).then_some(usage)
}
