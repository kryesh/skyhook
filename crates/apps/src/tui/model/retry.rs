//! One journal-derived status block per failed, retrying or interrupted request,
//! not per attempt, and per failed turn no such block reports.
use super::entries::Inputs;
use super::{Entry, EntryKey, Surface, View};
use crate::tui::commands::Command;
use crate::tui::format::Timing;
use skyhook::identity::AgentId;
use skyhook::provider::protocol::ItemKind;
use skyhook::provider::{ProviderError, ProviderErrorKind};
use skyhook::session::Failure;
use skyhook::session::{FailureRun, RequestFailure, RequestPhase, RequestSeq};
use std::borrow::Cow;
use std::fmt::Write as _;

/// Refusals are deterministic for a given request, so a plain retry repeats it.
pub(super) fn refusal_hint() -> String {
    let (model, retry) = (Command::Model, Command::Retry);
    format!("Choose another model with {model}, then continue with {retry}.")
}

/// A failure as its card shows it: the provider class in the title, and the
/// message in the body, then what resolves it.
#[derive(Clone)]
struct Report {
    kind: Option<ProviderErrorKind>,
    message: String,
    hint: Option<String>,
}

impl From<&Failure> for Report {
    fn from(failure: &Failure) -> Self {
        let (kind, message) = match failure {
            Failure::Provider(message, kind) => (Some(*kind), message.clone()),
            failure => (None, failure.to_string()),
        };
        let hint = matches!(failure, Failure::Refused(_)).then(refusal_hint);
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

/// Attempts `first` through `last` of a request, which failed as `report` says.
struct Run {
    first: u64,
    last: u64,
    report: Report,
}

impl From<&FailureRun> for Run {
    fn from(run: &FailureRun) -> Self {
        Self {
            first: run.first,
            last: run.last,
            report: (&run.failure).into(),
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

    /// The message, or each of `runs` once more than one attempt failed, then
    /// `between` and the hint, each when present.
    fn body(&self, runs: &[Run], between: &[&str]) -> String {
        let failures = match runs {
            [] => Cow::Borrowed(self.message.as_str()),
            [run] if run.first == run.last => Cow::Borrowed(self.message.as_str()),
            runs => Cow::Owned(listing(runs)),
        };
        let lines = std::iter::once(&*failures)
            .chain(between.iter().copied())
            .chain(self.hint.as_deref());
        let lines: Vec<_> = lines.filter(|line| !line.is_empty()).collect();
        lines.join("\n")
    }
}

/// Each run, oldest first: its attempts, then its class and its message, whose lines
/// align past the widest label.
fn listing(runs: &[Run]) -> String {
    let label = |run: &Run| match run.first == run.last {
        true => format!("attempt {}", run.first),
        false => format!("attempts {}–{}", run.first, run.last),
    };
    let labels: Vec<_> = runs.iter().map(label).collect();
    let width = labels.iter().map(|label| label.chars().count()).max();
    let width = width.unwrap_or_default() + 2;
    let mut listed = Vec::new();
    for (label, run) in labels.iter().zip(runs) {
        let mut lines = run.report.message.lines();
        let first = lines.next().unwrap_or_default();
        listed.push(match run.report.kind {
            Some(kind) => format!("{label:width$}{kind}: {first}"),
            None => format!("{label:width$}{first}"),
        });
        listed.extend(lines.map(|line| format!("{:width$}{line}", "")));
    }
    listed.join("\n")
}

/// A failed turn no request's card reports.
pub(super) fn failure_entry(key: EntryKey, failure: &Failure, view: &View) -> Entry {
    let report = Report::from(failure);
    let open = view.is_expanded(&key, true);
    let title = report.title("Turn failed", None);
    Entry::disclosed(key, title, open, Surface::Status, || report.body(&[], &[]))
}

pub(super) fn retry_entry(
    inputs: Inputs<'_>,
    agent: &AgentId,
    request: RequestSeq,
) -> Option<Entry> {
    let record = inputs.request(request)?;
    let key = EntryKey::Retry(request);
    let view = inputs.presentation.view;
    let mut timing = Timing::Untimed;
    // `settled` is the attempt that failed for good, after those retried.
    let (title, report, settled, open, surface) = match &record.phase {
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
            let open = view.is_expanded(&key, true);
            (title, report, *attempt, open, Surface::Error)
        }
        RequestPhase::Failed {
            attempt, failure, ..
        } => {
            let report = Report::from(failure);
            let title = report.title("Request failed", *attempt);
            let open = view.is_expanded(&key, true);
            (title, report, *attempt, open, Surface::Status)
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
            (title, report, None, open, Surface::Status)
        }
    };
    let body = || {
        let response = inputs.live.views.responses.get(&(agent.clone(), request));
        let partial = response.into_iter().flat_map(|response| response.blocks());
        let partial: Vec<_> = partial
            .filter(|block| block.kind == ItemKind::Text && !block.text.trim().is_empty())
            .map(|block| block.text.as_str())
            .collect();
        let mut runs: Vec<_> = record.retried.iter().map(Run::from).collect();
        runs.extend(settled.map(|attempt| Run {
            first: attempt,
            last: attempt,
            report: report.clone(),
        }));
        // The hint follows any partial response, so it is not buried.
        report.body(&runs, &partial)
    };
    let mut entry = Entry::disclosed(key, title, open, surface, body);
    entry.timing = timing;
    Some(entry)
}
