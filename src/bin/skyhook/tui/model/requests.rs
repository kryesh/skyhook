//! Metadata-only request rows and request timing.

use super::projection::RequestInfo;
use super::{Entry, Projection, Surface, clean, number};
use skyhook::agent::{AgentActivity, ObservationSnapshot};
use skyhook::identity::AgentId;
use skyhook::provider::protocol::Usage;
use skyhook::session::ModelPurpose;

#[derive(Clone, PartialEq, Eq)]
pub struct RequestRow {
    pub sequence: u64,
    pub purpose: ModelPurpose,
    pub model: String,
    pub status: &'static str,
    pub usage: Option<Usage>,
    pub elapsed_tenths: Option<u64>,
}

impl RequestRow {
    pub fn metadata(&self) -> [String; 4] {
        [
            format!("Request #{}", self.sequence),
            format!("{:?}", self.purpose),
            self.model.clone(),
            self.status.into(),
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
    let status = if info.failed {
        "Failed"
    } else if running {
        "Running"
    } else if info.response.is_some() || info.usage.is_some() {
        "Completed"
    } else {
        "Interrupted"
    };
    let row = RequestRow {
        sequence,
        purpose: *purpose,
        model: clean(info.model.as_deref().unwrap_or("Unknown model")).replace('\n', " "),
        status,
        usage: info.usage,
        elapsed_tenths: request_elapsed(info, running),
    };
    let mut e = Entry::new(
        format!("r{sequence}"),
        row.metadata().join(" · "),
        Surface::Tool,
    );
    e.request = Some(row);
    e.running = running;
    e
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
    let start = info.started_millis?;
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
    use super::*;
    use skyhook::identity::SessionId;

    #[test]
    fn request_rows_stay_metadata_only_across_lifecycle_states() {
        let mut info = RequestInfo {
            model: Some("model\nname".into()),
            started_millis: Some(1000),
            ..Default::default()
        };
        for (running, failed, response, status) in [
            (true, false, None, "Running"),
            (false, false, None, "Interrupted"),
            (false, false, Some(9), "Completed"),
            (false, true, Some(9), "Failed"),
        ] {
            info.failed = failed;
            info.response = response;
            let row = request_entry(4, &ModelPurpose::Agent, &info, running);
            assert!(!row.expandable);
            assert!(row.document.is_none());
            assert_eq!(row.running, running);
            let metadata = row.request.unwrap();
            assert_eq!(
                metadata.metadata(),
                ["Request #4", "Agent", "model name", status].map(String::from)
            );
            assert_eq!(&metadata.statistics()[..3], &["—", "—", "—"]);
        }
        info.failed = false;
        info.response = None;
        info.usage = Some(Usage {
            input_tokens: 2000,
            cached_input_tokens: 300,
            output_tokens: 40,
        });
        info.finished_millis = Some(2345);
        let row = request_entry(4, &ModelPurpose::Agent, &info, false)
            .request
            .unwrap();
        assert_eq!(row.status, "Completed");
        assert_eq!(
            row.statistics(),
            ["40", "2k", "300", "1.3s"].map(String::from)
        );
        info.finished_millis = Some(0);
        assert_eq!(request_elapsed(&info, false), Some(0));
    }

    #[test]
    fn only_the_current_unfinished_working_request_is_running() {
        let agent = AgentId::root(SessionId::from_bytes([1; 16]));
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
}
