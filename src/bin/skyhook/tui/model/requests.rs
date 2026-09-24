//! Metadata-only request rows and request timing.

use super::{Entry, EntryBody, clean, number};
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

    /// The request is still in progress, so its row animates and its elapsed ticks.
    pub fn running(self) -> bool {
        matches!(self, Self::Running | Self::Retrying)
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
    pub elapsed_tenths: Option<u64>,
}

impl RequestRow {
    pub fn metadata(&self) -> [String; 4] {
        [
            format!("Request #{}", self.sequence),
            format!("{:?}", self.purpose),
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
            self.elapsed_tenths.map_or_else(
                || "—".into(),
                |tenths| format!("{}.{}s", tenths / 10, tenths % 10),
            ),
        ]
    }
}

pub(super) fn request_entry(sequence: RequestSeq, record: &RequestRecord) -> Entry {
    let status = request_status(&record.phase);
    let row = RequestRow {
        sequence,
        purpose: record.purpose,
        model: clean(&record.profile.profile.model).replace('\n', " "),
        status,
        usage: reported(record.usage),
        elapsed_tenths: request_elapsed(record),
    };
    let mut e = Entry::request_entry(row);
    e.running = status.running();
    e
}

fn request_status(phase: &RequestPhase) -> RequestStatus {
    match phase {
        RequestPhase::Requested | RequestPhase::Open { .. } => RequestStatus::Running,
        RequestPhase::Retrying { .. } => RequestStatus::Retrying,
        RequestPhase::Failed { .. } | RequestPhase::Refused { .. } => RequestStatus::Failed,
        RequestPhase::Interrupted { .. } => RequestStatus::Interrupted,
        RequestPhase::Completed { .. } => RequestStatus::Completed,
    }
}

/// The runtime journals usage only once an attempt observed some.
fn reported(usage: Usage) -> Option<Usage> {
    (usage != Usage::default()).then_some(usage)
}

/// Update a request row's time-dependent fields in place; the result is
/// identical to rebuilding it with `request_entry`. Returns whether it changed.
pub(super) fn refresh_request_entry(entry: &mut Entry, record: &RequestRecord) -> bool {
    let EntryBody::Request { row, text } = &mut entry.body else {
        return false;
    };
    let status = request_status(&record.phase);
    let usage = reported(record.usage);
    let elapsed_tenths = request_elapsed(record);
    if entry.running == status.running()
        && row.status == status
        && row.usage == usage
        && row.elapsed_tenths == elapsed_tenths
    {
        return false;
    }
    entry.running = status.running();
    row.usage = usage;
    row.elapsed_tenths = elapsed_tenths;
    if row.status != status {
        row.status = status;
        *text = row.metadata().join(" · ");
    }
    true
}

/// Tenths of a second from the request until it settled, or until now while it
/// is still pending.
pub(super) fn request_elapsed(record: &RequestRecord) -> Option<u64> {
    let start = record.requested_millis;
    let end = if record.phase.pending() {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(start, |duration| {
                duration.as_millis().min(i64::MAX as u128) as i64
            })
    } else {
        record.finished_millis?
    };
    Some(end.saturating_sub(start).max(0) as u64 / 100)
}
