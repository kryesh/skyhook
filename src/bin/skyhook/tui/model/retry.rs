//! One journal-derived error block per failed logical request, not per attempt.
use super::{Entry, Projection, Surface};
use skyhook::agent::{AgentActivity, ObservationSnapshot};
use skyhook::identity::AgentId;
use skyhook::provider::protocol::BlockKind;

#[derive(Clone)]
pub(super) struct RetryState {
    attempt: u64,
    max_attempts: Option<u64>,
    delay_millis: Option<u64>,
    error: Option<String>,
}

impl RetryState {
    pub(super) fn has_error(&self) -> bool {
        self.error.is_some()
    }

    pub(super) fn started(attempt: u64) -> Self {
        Self {
            attempt,
            max_attempts: None,
            delay_millis: None,
            error: None,
        }
    }

    pub(super) fn failed(attempt: u64, error: &str) -> Self {
        Self {
            error: Some(error.to_owned()),
            ..Self::started(attempt)
        }
    }

    pub(super) fn scheduled(
        attempt: u64,
        max_attempts: Option<u64>,
        delay_millis: u64,
        error: &str,
    ) -> Self {
        Self {
            attempt,
            max_attempts,
            delay_millis: Some(delay_millis),
            error: Some(error.to_owned()),
        }
    }
}

/// Diagnostics are a bounded, single-line summary. Full safe error messages and
/// every attempt remain in the journal rather than growing history.
fn diagnostic(error: &str) -> String {
    let clean: String = error
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut chars = clean
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .collect::<Vec<_>>();
    if chars.len() > 240 {
        chars.truncate(239);
        chars.push('…');
    }
    chars.into_iter().collect()
}

pub(super) fn retry_entry(
    snapshot: &ObservationSnapshot,
    projection: &Projection,
    agent: &AgentId,
    request: u64,
    thinking: bool,
) -> Option<Entry> {
    let info = projection.requests.get(&request)?;
    let state = info.retry.as_ref()?;
    // Successful responses use their existing renderer, without attempt labels.
    if !state.has_error() {
        return None;
    }
    let interrupted = projection.active_request.get(agent) == Some(&request)
        && matches!(
            snapshot.activity.get(agent),
            Some(AgentActivity::Interrupted)
        );
    let running = projection.active_request.get(agent) == Some(&request)
        && !interrupted
        && state.delay_millis.is_some();
    let label = if interrupted {
        "Interrupted"
    } else if state.delay_millis.is_some() {
        "Retrying"
    } else {
        "Request failed"
    };
    let mut text = format!("{label} · attempt {}", state.attempt);
    if let Some(max) = state.max_attempts {
        text.push_str(&format!(" of {max}"));
    }
    if let Some(delay) = state.delay_millis.filter(|_| !interrupted) {
        text.push_str(&format!(" · retry delay {delay} ms"));
    }
    if let Some(error) = &state.error {
        text.push_str(&format!("\n{}", diagnostic(error)));
    }
    // Committed content is rendered by the normal message renderer, not twice.
    if info.response.is_none()
        && let Some(response) = snapshot.responses.get(&(agent.clone(), request))
    {
        for block in response
            .snapshot()
            .items
            .into_iter()
            .flat_map(|item| item.blocks)
        {
            match block.kind {
                BlockKind::Text if !block.text.trim().is_empty() => {
                    text.push('\n');
                    text.push_str(&block.text);
                }
                BlockKind::Reasoning if thinking && !block.text.trim().is_empty() => {
                    text.push_str("\nReasoning\n");
                    text.push_str(&block.text);
                }
                _ => {}
            }
        }
    }
    let mut entry = Entry::new(format!("failed{request}"), text, Surface::Status);
    entry.running = running;
    Some(entry)
}
