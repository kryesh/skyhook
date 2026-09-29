//! One journal-derived status block per failed, retrying or interrupted request,
//! not per attempt.
use super::{Entry, EntryKey, Surface, Timing};
use crate::text::brief;
use skyhook::agent::{Failure, ObservationSnapshot};
use skyhook::identity::AgentId;
use skyhook::provider::protocol::ItemKind;
use skyhook::session::{RequestFailure, RequestPhase, RequestSeq};

/// Refusals are deterministic for a given request, so a plain retry repeats it.
pub(super) const REFUSAL_HINT: &str =
    "Choose another model with /model, then continue with /retry.";

const DIAGNOSTIC_CHARS: usize = 240;

/// Diagnostics are a bounded, single-line summary. Full safe error messages and
/// every attempt remain in the journal rather than growing history.
fn diagnostic(error: impl std::fmt::Display) -> String {
    let error = error.to_string().replace(char::is_control, " ");
    brief(&error, DIAGNOSTIC_CHARS)
}

pub(super) fn retry_entry(
    snapshot: &ObservationSnapshot,
    agent: &AgentId,
    request: RequestSeq,
) -> Option<Entry> {
    let phase = &snapshot.ledger.get(request)?.phase;
    let mut timing = Timing::Untimed;
    let mut text = match phase {
        RequestPhase::Requested
        | RequestPhase::Open { .. }
        | RequestPhase::Completed { .. }
        | RequestPhase::Interrupted { attempt: None, .. } => return None,
        RequestPhase::Failed {
            attempt: Some(attempt),
            failure: RequestFailure::Model(Failure::Refused(error)),
            ..
        } => {
            format!(
                "Model declined to respond · attempt {attempt}\n{}",
                diagnostic(error)
            )
        }
        RequestPhase::Failed {
            attempt, failure, ..
        } => {
            let attempt = attempt.map_or(String::new(), |attempt| format!(" · attempt {attempt}"));
            format!("Request failed{attempt}\n{}", diagnostic(failure))
        }
        RequestPhase::Retrying {
            attempt,
            failure,
            due,
        } => {
            // A scheduled retry counts down to its next attempt.
            timing = Timing::Until(*due);
            format!(
                "Retrying · attempt {}\n{}",
                attempt + 1,
                diagnostic(failure)
            )
        }
        RequestPhase::Interrupted {
            attempt: Some(attempt),
            ..
        } => format!("Interrupted · attempt {attempt}"),
    };
    // A partial response that committed (an abort) or settled without a failure
    // (an interruption) renders at its journal position, not in the card.
    let partial = phase.has_status_card() && !phase.settled_in_place();
    if partial && let Some(response) = snapshot.responses.get(&(agent.clone(), request)) {
        for block in response.blocks() {
            if block.kind == ItemKind::Text && !block.text.trim().is_empty() {
                text.push('\n');
                text.push_str(&block.text);
            }
        }
    }
    let refused = matches!(
        phase,
        RequestPhase::Failed {
            failure: RequestFailure::Model(Failure::Refused(_)),
            ..
        }
    );
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
    entry.timing = timing;
    Some(entry)
}
