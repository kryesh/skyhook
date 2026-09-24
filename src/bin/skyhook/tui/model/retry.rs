//! One journal-derived status block per failed, retrying or interrupted request,
//! not per attempt.
use super::{Entry, EntryKey, Projection, Surface};
use skyhook::agent::ObservationSnapshot;
use skyhook::identity::AgentId;
use skyhook::provider::protocol::ItemKind;
use skyhook::session::{RequestPhase, RequestSeq};

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
    request: RequestSeq,
) -> Option<Entry> {
    let phase = &projection.ledger.get(request)?.phase;
    let mut text = match phase {
        RequestPhase::Requested
        | RequestPhase::Open { .. }
        | RequestPhase::Completed { .. }
        | RequestPhase::Interrupted { attempt: None } => return None,
        RequestPhase::Failed { attempt, error, .. } => {
            let attempt = attempt.map_or(String::new(), |attempt| format!(" · attempt {attempt}"));
            format!("Request failed{attempt}\n{}", diagnostic(error))
        }
        RequestPhase::Refused { attempt, error } => {
            format!(
                "Model declined to respond · attempt {attempt}\n{}",
                diagnostic(error)
            )
        }
        RequestPhase::Retrying {
            attempt,
            delay,
            error,
        } => format!(
            "Retrying · attempt {} · retry delay {} ms\n{}",
            attempt + 1,
            delay.as_millis(),
            diagnostic(error)
        ),
        RequestPhase::Interrupted {
            attempt: Some(attempt),
        } => format!("Interrupted · attempt {attempt}"),
    };
    // A partial response that committed (an abort) or settled without a failure
    // (an interruption) renders at its journal position, not in the card.
    let partial = matches!(
        phase,
        RequestPhase::Failed { message: None, .. }
            | RequestPhase::Refused { .. }
            | RequestPhase::Retrying { .. }
    );
    if partial && let Some(response) = snapshot.responses.get(&(agent.clone(), request)) {
        for block in response.blocks() {
            if block.kind == ItemKind::Text && !block.text.trim().is_empty() {
                text.push('\n');
                text.push_str(&block.text);
            }
        }
    }
    let refused = matches!(phase, RequestPhase::Refused { .. });
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
    entry.running = matches!(phase, RequestPhase::Retrying { .. });
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
}
