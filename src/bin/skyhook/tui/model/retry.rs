//! One journal-derived status block per failed, retrying or interrupted request,
//! not per attempt, and per failed turn no such block reports.
use super::entries::Inputs;
use super::{Entry, EntryKey, Surface, Timing, Title, View, number};
use skyhook::agent::Failure;
use skyhook::identity::AgentId;
use skyhook::provider::protocol::ItemKind;
use skyhook::provider::{ProviderError, ProviderErrorKind};
use skyhook::session::{RequestFailure, RequestPhase, RequestSeq};
use std::borrow::Cow;
use std::fmt::Write as _;

/// Refusals are deterministic for a given request, so a plain retry repeats it.
pub(super) const REFUSAL_HINT: &str =
    "Choose another model with /model, then continue with /retry.";

/// The most of a failure's message a card lays out, in characters and in lines; copy
/// and export read all of it.
const SHOWN_CHARS: usize = 800;
const SHOWN_LINES: usize = 8;

/// A failure as its card shows it: the provider class in the title, and the
/// message in the body, then what resolves it.
struct Report {
    kind: Option<ProviderErrorKind>,
    message: String,
    hint: Option<&'static str>,
}

impl From<&Failure> for Report {
    fn from(failure: &Failure) -> Self {
        let (kind, message) = match failure {
            Failure::Provider(message, kind) => (Some(*kind), message.clone()),
            failure => (None, failure.to_string()),
        };
        let hint = matches!(failure, Failure::Refused(_)).then_some(REFUSAL_HINT);
        Self {
            kind,
            message,
            hint,
        }
    }
}

impl From<&RequestFailure> for Report {
    fn from(failure: &RequestFailure) -> Self {
        match failure {
            RequestFailure::Model(failure) => failure.into(),
            RequestFailure::Compaction(fault) => Self {
                kind: None,
                message: fault.to_string(),
                hint: None,
            },
        }
    }
}

impl From<&ProviderError> for Report {
    fn from(error: &ProviderError) -> Self {
        Self {
            kind: Some(error.kind()),
            message: error.message.clone(),
            hint: None,
        }
    }
}

impl Report {
    /// `headline` · attempt · provider class.
    fn title(&self, headline: &str, attempt: Option<u64>) -> String {
        let mut title = headline.to_owned();
        if let Some(attempt) = attempt {
            let _ = write!(title, " · attempt {attempt}");
        }
        if let Some(kind) = self.kind {
            let _ = write!(title, " · {kind}");
        }
        title
    }

    /// The message, `between` it and the hint, and the hint, each when present: as
    /// the card lays it out, the message abridged, and whole.
    fn body<'a>(&'a self, between: impl Iterator<Item = &'a str>) -> (String, String) {
        let between: Vec<_> = between.collect();
        let join = |message: &str| {
            let lines = std::iter::once(message)
                .chain(between.iter().copied())
                .chain(self.hint);
            let lines: Vec<_> = lines.filter(|line| !line.is_empty()).collect();
            lines.join("\n")
        };
        (join(&abridge(&self.message)), join(&self.message))
    }
}

/// What a card lays out of `message`: its first lines and characters, then how much
/// more there is.
fn abridge(message: &str) -> Cow<'_, str> {
    let chars = message.char_indices().nth(SHOWN_CHARS).map(|(at, _)| at);
    let lines = message.match_indices('\n').nth(SHOWN_LINES - 1);
    let end = [chars, lines.map(|(at, _)| at)].into_iter().flatten().min();
    let Some(end) = end else {
        return Cow::Borrowed(message);
    };
    let more = message[end..].chars().count() as u64;
    let shown = message[..end].trim_end();
    Cow::Owned(format!("{shown}\n… {} more characters", number(more)))
}

/// A collapsible card, its body, which abridges the whole, built only while open.
fn card(
    key: EntryKey,
    label: String,
    open: bool,
    surface: Surface,
    body: impl FnOnce() -> (String, String),
) -> Entry {
    let title = Title::disclosed(label, open);
    if !open {
        return Entry::titled(key, title, String::new(), surface);
    }
    let (shown, whole) = body();
    Entry::abridged(key, title, shown, whole, surface)
}

/// A failed turn no request's card reports.
pub(super) fn failure_entry(key: EntryKey, failure: &Failure, view: &View) -> Entry {
    let report = Report::from(failure);
    let open = view.is_expanded(&key, true);
    let title = report.title("Turn failed", None);
    card(key, title, open, Surface::Status, || {
        report.body(std::iter::empty())
    })
}

pub(super) fn retry_entry(
    inputs: Inputs<'_>,
    agent: &AgentId,
    request: RequestSeq,
) -> Option<Entry> {
    let phase = &inputs.request(request)?.phase;
    let key = EntryKey::Retry(request);
    let view = inputs.presentation.view;
    let mut timing = Timing::Untimed;
    let (title, report, open, surface) = match phase {
        RequestPhase::Requested
        | RequestPhase::Open { .. }
        | RequestPhase::Completed { .. }
        | RequestPhase::Interrupted { attempt: None, .. } => return None,
        // An interrupted attempt's partial response renders at its journal position.
        RequestPhase::Interrupted {
            attempt: Some(attempt),
            ..
        } => {
            let text = format!("Interrupted · attempt {attempt}");
            return Some(Entry::new(key, text, Surface::Status));
        }
        // The headline names the refusal; the body is the model's account of it.
        RequestPhase::Failed {
            attempt,
            failure: RequestFailure::Model(failure @ Failure::Refused(detail)),
            ..
        } => {
            let report = Report {
                message: detail.clone(),
                ..Report::from(failure)
            };
            let title = report.title("Model declined to respond", *attempt);
            (title, report, view.is_expanded(&key, true), Surface::Error)
        }
        RequestPhase::Failed {
            attempt, failure, ..
        } => {
            let report = Report::from(failure);
            let title = report.title("Request failed", *attempt);
            (title, report, view.is_expanded(&key, true), Surface::Status)
        }
        RequestPhase::Retrying {
            attempt,
            failure,
            due,
        } => {
            // A scheduled retry counts down to its next attempt.
            timing = Timing::Until(*due);
            let report = Report::from(failure);
            let open = view.is_expanded(&key, inputs.presentation.all_details);
            let title = report.title("Retrying", Some(attempt + 1));
            (title, report, open, Surface::Status)
        }
    };
    let body = || {
        let response = inputs.live.views.responses.get(&(agent.clone(), request));
        let partial = response.into_iter().flat_map(|response| response.blocks());
        let partial = partial
            .filter(|block| block.kind == ItemKind::Text && !block.text.trim().is_empty())
            .map(|block| block.text.as_str());
        // The hint follows any partial response, so it is not buried.
        report.body(partial)
    };
    let mut entry = card(key, title, open, surface, body);
    entry.timing = timing;
    Some(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A long failure's card lays out its first lines, saying how much more there is,
    /// while its text keeps the whole message and the hint after it.
    #[test]
    fn a_long_failure_lays_out_an_abridged_message() {
        let message = format!("<html>\n{}</html>", "<p>bad gateway</p>\n".repeat(5000));
        let failure = Failure::Refused(message.clone());
        let entry = failure_entry(EntryKey::JournalFailed, &failure, &View::default());
        let shown = entry.body();
        assert!(shown.lines().count() <= SHOWN_LINES + 2, "{shown}");
        assert!(shown.contains("more characters"), "{shown}");
        assert!(shown.ends_with(REFUSAL_HINT), "{shown}");
        assert!(entry.text().contains(&message));
        assert!(entry.text().ends_with(REFUSAL_HINT));
        let short = Failure::Refused("declined".into());
        let entry = failure_entry(EntryKey::JournalFailed, &short, &View::default());
        let (_, whole) = entry.text().split_once('\n').unwrap();
        assert_eq!(entry.body(), whole);
    }
}
