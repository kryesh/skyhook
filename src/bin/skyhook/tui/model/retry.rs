//! One journal-derived error block per failed logical request, not per attempt.
use super::{Entry, EntryKey, Projection, Surface};
use skyhook::agent::{AgentActivity, ObservationSnapshot};
use skyhook::identity::AgentId;
use skyhook::provider::protocol::BlockKind;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RetryState {
    Started {
        attempt: u64,
    },
    Failed {
        attempt: u64,
        error: String,
    },
    /// The model declined to answer. Terminal for this request and never retried
    /// automatically, so it is presented as an error rather than a pending retry.
    Refused {
        attempt: u64,
        error: String,
    },
    Scheduled {
        attempt: u64,
        max_attempts: Option<u64>,
        delay_millis: u64,
        error: String,
    },
}

impl RetryState {
    pub(super) fn has_error(&self) -> bool {
        !matches!(self, Self::Started { .. })
    }
}

/// Refusals are deterministic for a given request, so a plain retry repeats it.
pub(super) const REFUSAL_HINT: &str =
    "Choose another model with /model, then continue with /retry.";

const DIAGNOSTIC_CHAR_LIMIT: usize = 240;
const DIAGNOSTIC_ELLIPSIS_BUDGET: usize = DIAGNOSTIC_CHAR_LIMIT - 1;

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
    if chars.len() > DIAGNOSTIC_CHAR_LIMIT {
        chars.truncate(DIAGNOSTIC_ELLIPSIS_BUDGET);
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
    // Started attempts have no diagnostic; scheduled retries always have one,
    // even when their retry budget is unlimited.
    let refused = matches!(state, RetryState::Refused { .. });
    let (attempt, max_attempts, delay_millis, error) = match state {
        RetryState::Started { .. } => return None,
        RetryState::Failed { attempt, error } | RetryState::Refused { attempt, error } => {
            (*attempt, None, None, error)
        }
        RetryState::Scheduled {
            attempt,
            max_attempts,
            delay_millis,
            error,
        } => (*attempt, *max_attempts, Some(*delay_millis), error),
    };
    let interrupted = projection.active_request.get(agent) == Some(&request)
        && matches!(
            snapshot.activity.get(agent),
            Some(AgentActivity::Interrupted)
        );
    let running = projection.active_request.get(agent) == Some(&request)
        && !interrupted
        && delay_millis.is_some();
    let label = if refused {
        "Model declined to respond"
    } else if interrupted {
        "Interrupted"
    } else if delay_millis.is_some() {
        "Retrying"
    } else {
        "Request failed"
    };
    let mut text = format!("{label} · attempt {attempt}");
    if let Some(max) = max_attempts {
        text.push_str(&format!(" of {max}"));
    }
    if let Some(delay) = delay_millis.filter(|_| !interrupted) {
        text.push_str(&format!(" · retry delay {delay} ms"));
    }
    text.push_str(&format!("\n{}", diagnostic(error)));
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
    if refused {
        // Last line, after any partial response above, so the one actionable
        // instruction is not buried. The same request refuses again unchanged.
        text.push_str(&format!("\n{REFUSAL_HINT}"));
    }
    let surface = if refused {
        Surface::Error
    } else {
        Surface::Status
    };
    let mut entry = Entry::new(EntryKey::Retry(request), text, surface);
    entry.running = running;
    Some(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_normalize_controls_and_apply_unicode_character_budget() {
        assert_eq!(diagnostic("one\n\t two\r three"), "one two three");
        let exact = "界".repeat(DIAGNOSTIC_CHAR_LIMIT);
        assert_eq!(diagnostic(&exact), exact);
        let long = diagnostic(&"界".repeat(DIAGNOSTIC_CHAR_LIMIT + 1));
        assert_eq!(long.chars().count(), DIAGNOSTIC_CHAR_LIMIT);
        assert!(long.ends_with('…'));
    }
    #[test]
    fn retry_phases_preserve_unlimited_bounded_and_interrupted_labels() {
        use skyhook::identity::SessionId;
        let agent = AgentId::root(SessionId::from_bytes([1; 16]));
        let mut projection = Projection::default();
        let mut snapshot = ObservationSnapshot::default();
        projection.active_request.insert(agent.clone(), 4);
        let scheduled = |attempt, max_attempts, delay_millis| RetryState::Scheduled {
            attempt,
            max_attempts,
            delay_millis,
            error: "failure".into(),
        };
        for (state, expected, running) in [
            (RetryState::Started { attempt: 1 }, None, false),
            (
                RetryState::Failed {
                    attempt: 1,
                    error: "failure".into(),
                },
                Some("Request failed · attempt 1\nfailure"),
                false,
            ),
            (
                scheduled(2, None, 0),
                Some("Retrying · attempt 2 · retry delay 0 ms\nfailure"),
                true,
            ),
            (
                scheduled(3, Some(4), 100),
                Some("Retrying · attempt 3 of 4 · retry delay 100 ms\nfailure"),
                true,
            ),
        ] {
            assert_eq!(state.has_error(), expected.is_some());
            projection.requests.entry(4).or_default().retry = Some(state);
            let entry = retry_entry(&snapshot, &projection, &agent, 4, false);
            assert_eq!(entry.as_ref().map(Entry::text), expected);
            assert_eq!(entry.as_ref().is_some_and(|entry| entry.running), running);
        }
        snapshot
            .activity
            .insert(agent.clone(), AgentActivity::Interrupted);
        let entry = retry_entry(&snapshot, &projection, &agent, 4, false).unwrap();
        assert_eq!(entry.text(), "Interrupted · attempt 3 of 4\nfailure");
        assert!(!entry.running);
        // Historical schedules retain their diagnostic but do not animate.
        projection.active_request.insert(agent.clone(), 5);
        let entry = retry_entry(&snapshot, &projection, &agent, 4, false).unwrap();
        assert_eq!(
            entry.text(),
            "Retrying · attempt 3 of 4 · retry delay 100 ms\nfailure"
        );
        assert!(!entry.running);
    }
}
