//! Metadata-only request rows and request timing.

use super::projection::RequestInfo;
use super::{Entry, EntryBody, Projection, clean, number};
use skyhook::agent::{AgentActivity, ObservationSnapshot};
use skyhook::identity::AgentId;
use skyhook::provider::protocol::Usage;
use skyhook::session::ModelPurpose;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestStatus {
    Failed,
    Running,
    Completed,
    Interrupted,
}

impl RequestStatus {
    /// Explicit UI text; variant names never leak into rendered rows.
    pub fn label(self) -> &'static str {
        match self {
            Self::Failed => "Failed",
            Self::Running => "Running",
            Self::Completed => "Completed",
            Self::Interrupted => "Interrupted",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct RequestRow {
    pub sequence: u64,
    pub purpose: ModelPurpose,
    pub model: String,
    pub status: RequestStatus,
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

pub(super) fn request_entry(
    sequence: u64,
    purpose: &ModelPurpose,
    info: &RequestInfo,
    running: bool,
) -> Entry {
    let status = request_status(info, running);
    let row = RequestRow {
        sequence,
        purpose: info.start.as_ref().map_or(*purpose, |start| start.purpose),
        model: clean(
            info.start
                .as_ref()
                .and_then(|start| start.model.as_deref())
                .unwrap_or("Unknown model"),
        )
        .replace('\n', " "),
        status,
        usage: info.usage,
        elapsed_tenths: request_elapsed(info, running),
    };
    let mut e = Entry::request_entry(row);
    e.running = running;
    e
}

fn request_status(info: &RequestInfo, running: bool) -> RequestStatus {
    if info.failed {
        RequestStatus::Failed
    } else if running {
        RequestStatus::Running
    } else if info.response.is_some() || info.usage.is_some() {
        RequestStatus::Completed
    } else {
        RequestStatus::Interrupted
    }
}

/// Update a request row's time-dependent fields in place; the result is
/// identical to rebuilding it with `request_entry`. Returns whether it changed.
pub(super) fn refresh_request_entry(entry: &mut Entry, info: &RequestInfo, running: bool) -> bool {
    let EntryBody::Request { row, text } = &mut entry.body else {
        return false;
    };
    let status = request_status(info, running);
    let elapsed_tenths = request_elapsed(info, running);
    if entry.running == running
        && row.status == status
        && row.usage == info.usage
        && row.elapsed_tenths == elapsed_tenths
    {
        return false;
    }
    entry.running = running;
    row.usage = info.usage;
    row.elapsed_tenths = elapsed_tenths;
    if row.status != status {
        row.status = status;
        *text = row.metadata().join(" · ");
    }
    true
}

pub(super) fn request_running(
    info: &RequestInfo,
    snapshot: &ObservationSnapshot,
    projection: &Projection,
    agent: &AgentId,
    request: u64,
) -> bool {
    info.finished_millis.is_none()
        && projection.active_request.get(agent) == Some(&request)
        && snapshot
            .responses
            .get(&(agent.clone(), request))
            .is_none_or(|response| !response.settled)
        && matches!(snapshot.activity.get(agent), Some(AgentActivity::Working))
}

pub(super) fn request_elapsed(info: &RequestInfo, running: bool) -> Option<u64> {
    let start = info.start.as_ref()?.timestamp_millis;
    let end = info.finished_millis.or_else(|| {
        running.then(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(start, |duration| {
                    duration.as_millis().min(i64::MAX as u128) as i64
                })
        })
    })?;
    Some(end.saturating_sub(start).max(0) as u64 / 100)
}

#[cfg(test)]
mod tests {
    use super::super::projection::RequestStart;
    use super::super::tests::{record, root};
    use super::super::{EntryKey, Surface};
    use super::*;
    use skyhook::session::SessionEvent;

    fn start(model: Option<&str>, purpose: ModelPurpose) -> Option<RequestStart> {
        let model = model.map(Into::into);
        Some(RequestStart {
            model,
            timestamp_millis: 1000,
            purpose,
        })
    }

    #[test]
    fn request_rows_stay_metadata_only_across_lifecycle_states() {
        let start = start(Some("model\nname"), ModelPurpose::Agent);
        let mut info = RequestInfo {
            start,
            ..Default::default()
        };
        for (running, failed, response, status) in [
            (true, false, None, "Running"),
            (false, false, None, "Interrupted"),
            (false, false, Some(9), "Completed"),
            (false, true, Some(9), "Failed"),
        ] {
            (info.failed, info.response) = (failed, response);
            let row = request_entry(4, &ModelPurpose::Agent, &info, running);
            assert_eq!(
                (row.key(), row.expandable(), row.running),
                (&EntryKey::Request(4), false, running)
            );
            assert!(row.header().is_none() && row.document().is_none());
            let metadata = row.request().unwrap();
            assert_eq!(metadata.sequence, 4);
            let expected = ["Request #4", "Agent", "model name", status].map(String::from);
            assert_eq!(metadata.metadata(), expected);
            assert_eq!(row.text(), metadata.metadata().join(" · "));
            assert_eq!(&metadata.statistics()[..3], &["—", "—", "—"]);
        }
        (info.failed, info.response, info.finished_millis) = (false, None, Some(2345));
        info.usage = Some(Usage {
            input_tokens: 2000,
            cached_input_tokens: 300,
            output_tokens: 40,
        });
        let entry = request_entry(4, &ModelPurpose::Agent, &info, false);
        let row = entry.request().unwrap();
        assert_eq!(row.status, RequestStatus::Completed);
        assert_eq!(
            row.statistics(),
            ["40", "2k", "300", "1.3s"].map(String::from)
        );
        info.finished_millis = Some(0);
        assert_eq!(request_elapsed(&info, false), Some(0));
    }

    #[test]
    fn in_place_refresh_matches_a_rebuilt_row() {
        let mut info = RequestInfo {
            start: start(Some("model"), ModelPurpose::Agent),
            ..Default::default()
        };
        let mut entry = request_entry(4, &ModelPurpose::Agent, &info, true);
        assert!(!refresh_request_entry(&mut entry, &info, true));
        info.finished_millis = Some(2345);
        info.usage = Some(Usage::default());
        assert!(refresh_request_entry(&mut entry, &info, false));
        assert!(entry == request_entry(4, &ModelPurpose::Agent, &info, false));
        assert_eq!(entry.request().unwrap().status.label(), "Completed");
        assert!(!refresh_request_entry(&mut entry, &info, false));
        let mut text = Entry::new(EntryKey::Request(4), "text".into(), Surface::Tool);
        assert!(!refresh_request_entry(&mut text, &info, true));
    }

    #[test]
    fn only_the_current_unfinished_working_request_is_running() {
        let agent = root(1);
        let mut snapshot = ObservationSnapshot::default();
        let mut projection = Projection::default();
        let mut info = RequestInfo::default();
        projection.active_request.insert(agent.clone(), 4);
        assert!(!request_running(&info, &snapshot, &projection, &agent, 4));
        snapshot
            .activity
            .insert(agent.clone(), AgentActivity::Working);
        assert!(request_running(&info, &snapshot, &projection, &agent, 4));
        assert!(!request_running(&info, &snapshot, &projection, &agent, 3));
        info.finished_millis = Some(1);
        assert!(!request_running(&info, &snapshot, &projection, &agent, 4));
    }

    #[test]
    fn missing_start_and_unresolved_model_are_distinct() {
        let mut info = RequestInfo {
            failed: true,
            response: Some(9),
            usage: Some(Usage::default()),
            finished_millis: Some(2500),
            ..Default::default()
        };
        for (start, elapsed, purpose) in [
            (None, None, ModelPurpose::Compaction),
            (
                start(None, ModelPurpose::Agent),
                Some(15),
                ModelPurpose::Agent,
            ),
        ] {
            info.start = start;
            assert_eq!(request_elapsed(&info, false), elapsed);
            let entry = request_entry(4, &ModelPurpose::Compaction, &info, false);
            let row = entry.request().unwrap();
            assert_eq!(&*row.model, "Unknown model");
            assert_eq!(
                (&row.status, &row.purpose),
                (&RequestStatus::Failed, &purpose)
            );
        }
        // The same facts survive a journal rebuild: failure and usage before
        // any start, and a start whose context record is missing.
        let agent = root(1);
        let mut snapshot = ObservationSnapshot::default();
        for event in [
            SessionEvent::ModelFailed {
                request: 9,
                attempt: 1,
                error: "failed".into(),
            },
            SessionEvent::Usage {
                request: Some(9),
                usage: Usage::default(),
            },
            SessionEvent::ModelRequested {
                context: 99,
                history: Vec::new(),
                tail: Vec::new(),
                history_lifetime: Default::default(),
                purpose: ModelPurpose::Compaction,
            },
        ] {
            record(&mut snapshot, &agent, event);
        }
        let mut projection = Projection::default();
        projection.rebuild(&snapshot);
        let missing = &projection.requests[&9];
        assert!(missing.start.is_none() && missing.failed);
        assert!(missing.usage.is_some() && missing.finished_millis.is_some());
        let start = projection.requests[&3].start.as_ref().unwrap();
        assert_eq!(
            (start.timestamp_millis, &start.purpose),
            (3000, &ModelPurpose::Compaction)
        );
        assert!(start.model.is_none());
    }
}
