//! The one capability-aware renderer of failure facts, and the sanitizing its text takes.

use super::{Cause, Diagnostic, Effects, FailureSite, RequestEffects, Subject};
use crate::target::{ExecutionLocation, TargetRef};
use skyhook_worker::diagnostic::{PathFact, PathRole};
use skyhook_worker::op::TargetPath;
use skyhook_worker::target::Os;

/// Whether a viewer may learn which target something ran on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetView {
    Named,
    Hidden,
}

/// Rendering facts belong to the viewer, not the operation that produced the error.
#[derive(Clone, Copy)]
pub enum DiagnosticViewer<'a> {
    /// The host, which sees every detail.
    Host(TargetView),
    /// An agent, at its location.
    Agent(TargetView, &'a ExecutionLocation),
}

impl<'a> DiagnosticViewer<'a> {
    pub fn new(targets: TargetView, location: &'a ExecutionLocation) -> Self {
        Self::Agent(targets, location)
    }

    pub fn targets(self) -> TargetView {
        match self {
            Self::Host(targets) | Self::Agent(targets, _) => targets,
        }
    }

    /// The viewing agent's location; a host viewer has none.
    pub fn location(self) -> Option<&'a ExecutionLocation> {
        match self {
            Self::Host(_) => None,
            Self::Agent(_, location) => Some(location),
        }
    }
}

/// Inspection without a location is a host view; agent presentation supplies its location.
impl From<TargetView> for DiagnosticViewer<'_> {
    fn from(targets: TargetView) -> Self {
        Self::Host(targets)
    }
}

impl Diagnostic {
    /// Render for a host viewer. Agent-facing presentation also supplies its target.
    pub fn render(&self, targets: TargetView) -> String {
        self.render_for(targets.into())
    }

    pub fn render_for(&self, viewer: DiagnosticViewer<'_>) -> String {
        let mut text = self.context.operation.as_str().replace('_', " ");
        text.push_str(&match &self.context.subject {
            Subject::None => String::new(),
            Subject::Path(path) => format!(" path {}", quoted(path.as_str())),
            Subject::WorkingDirectory(path) => {
                format!(" working directory {}", quoted(path.as_str()))
            }
            Subject::StagingFile(path) => format!(" staging file for {}", quoted(path.as_str())),
            Subject::ParentDirectory(path) => {
                format!(" parent directory {}", quoted(path.as_str()))
            }
            Subject::DirectoryEntry(path) => format!(" directory entry {}", quoted(path.as_str())),
            Subject::Argument(root) if root.is_empty() => " arguments".into(),
            Subject::Argument(path) => format!(" argument {}", quoted(path)),
            Subject::Tool(tool) => format!(" tool {}", quoted(tool)),
            Subject::Job(job) => format!(" job {job}"),
            Subject::Process => " process".into(),
            Subject::Label(label) => format!(" {}", safe_text(label)),
        });
        // A failure that names nothing of its own is placed by the call's paths: where
        // they led, or what was requested when none resolved.
        let subject = &self.context.subject;
        let unnamed = matches!(subject, Subject::None | Subject::Tool(_));
        let resolved = |fact: &PathFact| fact.role == PathRole::Resolved;
        let role = match self.context.paths.iter().any(resolved) {
            true => PathRole::Resolved,
            false => PathRole::Requested,
        };
        let shown = (self.context.paths.iter()).filter(|fact| unnamed && fact.role == role);
        let mut relative = subject.target_path().is_some_and(is_relative);
        for fact in shown {
            relative |= is_relative(&fact.path);
            text.push_str(&format!(
                " ({} {})",
                fact.role.as_str(),
                quoted(fact.path.as_str())
            ));
        }
        text.push_str(" failed");
        match &self.context.site {
            FailureSite::Invocation => {}
            FailureSite::Host => {
                if viewer
                    .location()
                    .is_some_and(|location| location.target != TargetRef::Root)
                {
                    text.push_str(" on session host");
                }
            }
            // An agent already knows its own location; elsewhere, the workspace says
            // more than the target only for a path read from it.
            FailureSite::Execution(location) => {
                let elsewhere = viewer.location() != Some(location);
                let target = elsewhere && viewer.targets() == TargetView::Named;
                let workspace = viewer.location().is_none() || (elsewhere && relative);
                let workspace = workspace.then(|| quoted(location.workspace.as_str()));
                if target {
                    text.push_str(&format!(" on target {}", quoted(location.target.as_str())));
                }
                match workspace {
                    Some(workspace) if target => {
                        text.push_str(&format!(" (workspace {workspace})"))
                    }
                    Some(workspace) => text.push_str(&format!(" in workspace {workspace}")),
                    None => {}
                }
            }
        }
        text.push_str(": ");
        // A cause names its tool or job only where the subject has not.
        let named = |tool: &String| matches!(subject, Subject::Tool(named) if named == tool);
        let unless = |named: bool, name: String| if named { String::new() } else { name };
        match &self.cause {
            Cause::Io(error) => {
                text.push_str(&error.classified());
                if let Some(detail) = &error.detail {
                    text.push_str(&format!(": {}", safe_text(detail)));
                }
            }
            Cause::InvalidArguments(message)
            | Cause::Message(message)
            | Cause::Connection {
                detail: message, ..
            } => text.push_str(&safe_text(message)),
            Cause::Denied(reason) => {
                text.push_str("permission denied");
                if !reason.is_empty() {
                    text.push_str(&format!(" ({})", safe_text(reason)));
                }
            }
            Cause::Cancelled => text.push_str("operation cancelled"),
            Cause::Interrupted => text.push_str("operation interrupted; completion is unknown"),
            Cause::InputClosed => text.push_str("tool input channel is closed"),
            Cause::Json => text.push_str("invalid JSON data"),
            Cause::Unavailable { tool } => text.push_str(&safe_text(&format!(
                "{}unavailable in this context",
                unless(named(tool), format!("tool `{tool}` is "))
            ))),
            Cause::UnknownTool { tool } => text.push_str(&safe_text(&format!(
                "unknown tool{}",
                unless(named(tool), format!(" `{tool}`"))
            ))),
            Cause::ScriptUnavailable { tool } => text.push_str(&safe_text(&format!(
                "{}not available in scripts",
                unless(named(tool), format!("tool `{tool}` is "))
            ))),
            Cause::UnknownJob { job } => text.push_str(&format!(
                "unknown job{}",
                unless(*subject == Subject::Job(*job), format!(" {job}"))
            )),
            Cause::JobAlreadyTerminal { job } => text.push_str(&format!(
                "{}already has a terminal result",
                unless(*subject == Subject::Job(*job), format!("job {job} "))
            )),
            Cause::Unspelled { native } => text.push_str(&format!(
                "resolved path is not text (native bytes {})",
                escaped_bytes(native)
            )),
        }
        text.push_str(match self.context.request {
            RequestEffects::Unknown => "",
            RequestEffects::MayHaveExecuted => "; request may have executed",
        });
        text.push_str(match self.context.effects {
            Effects::Unknown | Effects::ReadOnly => "",
            Effects::NotStarted => "; operation did not start",
            Effects::Unchanged => "; destination unchanged",
            Effects::DestinationReplaced => "; destination replaced; durability is uncertain",
            Effects::PartialChange => "; partial changes may remain",
            Effects::OutputIncomplete => "; output incomplete",
        });
        text
    }
}

/// Show control characters as escapes, so untrusted names and messages can
/// neither drive a terminal nor fake line structure.
pub fn escape_controls(value: impl std::fmt::Display) -> String {
    let mut output = String::new();
    for character in value.to_string().chars() {
        if character.is_control() {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    output
}

/// Bound externally opaque text and remove terminal controls and URL credentials.
/// This is presentation sanitization, never failure classification.
pub(crate) fn safe_text(value: &str) -> String {
    let mut text = String::new();
    for word in value.split_whitespace() {
        if !text.is_empty() {
            text.push(' ');
        }
        let mut word = word
            .chars()
            .filter(|ch| !ch.is_control())
            .collect::<String>();
        if let Some(scheme) = word.find("://") {
            let authority = scheme + 3;
            let end = word[authority..]
                .find(['/', '?', '#'])
                .map_or(word.len(), |i| authority + i);
            if let Some(at) = word[authority..end].rfind('@') {
                word.replace_range(authority..authority + at + 1, "[redacted]@");
            }
        }
        text.push_str(&word);
        if text.chars().count() > 512 {
            text = text.chars().take(512).collect();
            text.push('…');
            break;
        }
    }
    text
}

/// Whether no target reads `path` from its root.
fn is_relative(path: &TargetPath) -> bool {
    !Os::ALL.iter().any(|os| path.is_absolute(*os))
}

/// Opaque bytes as escaped ASCII, never decoded.
fn escaped_bytes(bytes: &[u8]) -> String {
    let escaped = bytes
        .iter()
        .take(512)
        .flat_map(|byte| byte.escape_ascii())
        .map(char::from)
        .collect::<String>();
    format!(
        "`{}`{}",
        escaped.replace('`', "\\`"),
        if bytes.len() > 512 { "…" } else { "" }
    )
}

pub(crate) fn quoted(value: &str) -> String {
    let escaped = value
        .chars()
        .take(512)
        .flat_map(char::escape_debug)
        .collect::<String>();
    let suffix = if value.chars().count() > 512 {
        "…"
    } else {
        ""
    };
    format!("`{}`{suffix}", escaped.replace('`', "\\`"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::PartialContext;
    use skyhook_worker::diagnostic::Operation;

    #[test]
    fn rendering_preserves_paths_but_hides_targets_and_opaque_credentials() {
        let diagnostic = Diagnostic::new(
            PartialContext::new(Operation::Read, Subject::path("two  spaces\n\u{1b}[2J"))
                .at(FailureSite::Execution(ExecutionLocation::named(
                    "bastion".parse().unwrap(),
                    "/work".into(),
                )))
                .resolve(),
            Cause::Message("failed https://user:password@example.test/path\n\u{1b}oops".into()),
        );
        let text = diagnostic.render(TargetView::Hidden);
        assert!(text.contains("two  spaces\\n\\u{1b}[2J"));
        for secret in ["bastion", "password", "\u{1b}"] {
            assert!(!text.contains(secret));
        }
        let mut host = diagnostic.clone();
        host.context.site = FailureSite::Host;
        let local = ExecutionLocation::root("/local".into());
        let remote = ExecutionLocation::named("worker".parse().unwrap(), "/remote".into());
        assert!(
            !host
                .render_for(DiagnosticViewer::new(TargetView::Hidden, &local))
                .contains("on session host")
        );
        assert!(
            host.render_for(DiagnosticViewer::new(TargetView::Hidden, &remote))
                .contains("on session host")
        );
    }

    #[test]
    fn agents_are_told_only_what_their_location_and_the_subject_leave_unsaid() {
        let site = ExecutionLocation::named("bastion".parse().unwrap(), "/work".into());
        let render = |subject: Subject, requested: &str, viewer: &ExecutionLocation| {
            let context = PartialContext::new(Operation::Resolve, subject)
                .at(FailureSite::Execution(site.clone()))
                .path(PathRole::Requested, requested);
            let cause = Cause::io(&std::io::Error::from_raw_os_error(2));
            Diagnostic::new(context.resolve(), cause)
                .render_for(DiagnosticViewer::new(TargetView::Named, viewer))
        };
        let parent = || Subject::ParentDirectory("/nonexistent".into());
        let elsewhere = ExecutionLocation::root("/local".into());
        assert_eq!(
            render(parent(), "/nonexistent/missing", &site),
            "resolve parent directory `/nonexistent` failed: not found"
        );
        assert_eq!(
            render(parent(), "/nonexistent/missing", &elsewhere),
            "resolve parent directory `/nonexistent` failed on target `bastion`: not found"
        );
        assert_eq!(
            render(Subject::path("missing"), "missing", &elsewhere),
            "resolve path `missing` failed on target `bastion` (workspace `/work`): not found"
        );
        assert_eq!(
            render(Subject::Tool("read".into()), "missing", &elsewhere),
            "resolve tool `read` (requested `missing`) failed on target `bastion` \
             (workspace `/work`): not found"
        );
    }
}
