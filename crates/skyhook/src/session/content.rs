//! The durable conversation: what the session journals and projects. Providers
//! see it as context entries: runtime state as state, job events as runtime text,
//! so the renderers here are part of the session format.

use std::{fmt, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    agent::TodoItem,
    execution::ExecutionLocation,
    identity::JobId,
    job::{AgentMessage, AgentProgress, JobState, JobView},
    media::AttachmentRef,
    provider::protocol::{self, AssistantItem, Entry, StateLifetime, ToolResult},
    target::TargetRef,
    tool::registry::JobName,
};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "role", content = "content", rename_all = "snake_case")]
pub enum Message {
    User(Vec<UserPart>),
    Assistant(Vec<AssistantItem>),
    Tool(ToolResult),
    /// The agent's state at a request: the harness speaking, never conversation.
    State(RuntimeState),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserPart {
    Text {
        text: String,
    },
    Attachment {
        attachment: AttachmentRef,
    },
    /// Child replies and job views delivered since the last request.
    JobEvents {
        events: Vec<JobEvent>,
    },
    ParentInput {
        text: String,
    },
    Compaction {
        text: String,
    },
}

impl Message {
    /// An assistant message with neither nonblank text nor a tool call. Reasoning
    /// accompanies an action but is never one, so such a turn is refused before it
    /// is committed: a committed one would make every later request unencodable.
    pub fn is_content_free(&self) -> bool {
        match self {
            Self::Assistant(items) => items.iter().all(AssistantItem::is_content_free),
            Self::User(_) | Self::Tool(_) | Self::State(_) => false,
        }
    }

    /// An assistant message that makes no call, which answers its turn.
    pub(crate) fn answers(&self) -> bool {
        match self {
            Self::Assistant(items) => !items.iter().any(|item| item.call().is_some()),
            Self::User(_) | Self::Tool(_) | Self::State(_) => false,
        }
    }

    /// How far the host event it delivers went, if it delivers one.
    pub(crate) fn host_cursor(&self) -> Option<HostCursor> {
        let Self::User(parts) = self else {
            return None;
        };
        (parts.iter())
            .flat_map(|part| match part {
                UserPart::JobEvents { events } => events.as_slice(),
                _ => &[],
            })
            .find_map(|event| match event {
                JobEvent::Host(host) => Some(host.through),
                _ => None,
            })
    }

    /// Drop replay bound to the conversation that produced it, keeping display text. Changing
    /// that conversation, as compaction or a mode switch does, invalidates such replay; other
    /// replay is kept.
    #[must_use]
    pub fn without_bound_reasoning(mut self) -> Self {
        self.unbind();
        self
    }

    pub fn unbind(&mut self) {
        if let Self::Assistant(items) = self {
            items.iter_mut().for_each(AssistantItem::unbind);
        }
    }

    /// The context entry the message projects: its state with `lifetime`, or
    /// conversation, job events as runtime text and parent input and compaction
    /// summaries as user text.
    #[must_use]
    pub fn entry(&self, lifetime: StateLifetime) -> Entry {
        Entry::Message(match self {
            Self::User(parts) => {
                protocol::Message::User(parts.iter().map(UserPart::content).collect())
            }
            Self::Assistant(items) => protocol::Message::Assistant(items.clone()),
            Self::Tool(result) => protocol::Message::Tool(vec![result.clone()]),
            Self::State(state) => {
                let text = state.to_string();
                return Entry::State(protocol::State { text, lifetime });
            }
        })
    }
}

impl UserPart {
    pub fn is_image(&self) -> bool {
        matches!(
            self,
            Self::Attachment {
                attachment: AttachmentRef::Image(_)
            }
        )
    }

    fn content(&self) -> protocol::UserContent {
        match self {
            Self::Attachment { attachment } => protocol::UserContent::Attachment {
                attachment: attachment.clone(),
            },
            Self::Text { text } | Self::ParentInput { text } | Self::Compaction { text } => {
                protocol::UserContent::Text { text: text.clone() }
            }
            Self::JobEvents { events } => protocol::UserContent::Runtime {
                text: job_events_text(events),
            },
        }
    }
}

/// The agent's current state at a request.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct RuntimeState {
    pub date: String,
    pub jobs: Vec<StateJob>,
    pub todos: Vec<TodoItem>,
    /// The agent's own location; a job shows a target or workspace only where it differs.
    pub location: ExecutionLocation,
    /// The host's section, rendered by the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<crate::Prose>,
}

/// A live job in the state, with the active agent jobs it owns beneath it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct StateJob {
    pub job: JobId,
    pub kind: StateJobKind,
    pub name: Option<JobName>,
    pub state: JobState,
    /// None when the agent may not see targets.
    pub target: Option<TargetRef>,
    pub workspace: PathBuf,
    pub age_seconds: u64,
    pub children: Vec<StateJob>,
}

/// What a live job runs: a child agent, which reports its progress, or a tool.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StateJobKind {
    Agent { progress: AgentProgress },
    Tool { tool: String },
}

impl StateJobKind {
    pub fn tool(&self) -> &str {
        match self {
            Self::Agent { .. } => crate::tool::builtins::names::AGENT,
            Self::Tool { tool } => tool,
        }
    }
}

/// One entry of a job-events notification: a child's reply, a job's presented view, or
/// what the host had for the agent as its request was made.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobEvent {
    Message(AgentMessage),
    Job(Box<JobView>),
    Host(HostEvent),
}

impl JobEvent {
    /// The job it reports on.
    pub fn job(&self) -> Option<JobId> {
        match self {
            Self::Message(message) => Some(message.id),
            Self::Job(view) => view.id,
            Self::Host(_) => None,
        }
    }
}

/// A position in a host's own ordered log, opaque to the harness: a non-negative integer, which
/// the journal stores as it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(try_from = "i64", into = "i64")]
pub struct HostCursor(i64);

impl HostCursor {
    pub const fn get(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("host cursor {0} is negative")]
pub struct NegativeCursor(pub i64);

impl TryFrom<i64> for HostCursor {
    type Error = NegativeCursor;

    fn try_from(position: i64) -> Result<Self, NegativeCursor> {
        match position {
            0.. => Ok(Self(position)),
            _ => Err(NegativeCursor(position)),
        }
    }
}

impl From<HostCursor> for i64 {
    fn from(cursor: HostCursor) -> Self {
        cursor.0
    }
}

/// What a host's log held for an agent through `through`, as the document the model reads.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct HostEvent {
    pub through: HostCursor,
    pub view: HostView,
}

/// A host event's document: an object sent beside `"kind": "host"`, so it names no `kind` of
/// its own.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(try_from = "serde_json::Map<String, serde_json::Value>")]
pub struct HostView(serde_json::Map<String, serde_json::Value>);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a host event's view names a `kind` of its own")]
pub struct ViewNamesKind;

impl TryFrom<serde_json::Map<String, serde_json::Value>> for HostView {
    type Error = ViewNamesKind;

    fn try_from(view: serde_json::Map<String, serde_json::Value>) -> Result<Self, ViewNamesKind> {
        match view.contains_key("kind") {
            true => Err(ViewNamesKind),
            false => Ok(Self(view)),
        }
    }
}

impl HostView {
    pub fn fields(&self) -> &serde_json::Map<String, serde_json::Value> {
        &self.0
    }
}

/// An event as the model reads it: a host event is its view alone.
struct Sent<'a>(&'a JobEvent);

impl Serialize for Sent<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let JobEvent::Host(host) = self.0 else {
            return self.0.serialize(serializer);
        };
        let fields = host.view.fields();
        let mut map = serializer.serialize_map(Some(fields.len() + 1))?;
        map.serialize_entry("kind", "host")?;
        for (key, value) in fields {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

/// The text a job-events notification sends the model.
fn job_events_text(events: &[JobEvent]) -> String {
    let sent: Vec<_> = events.iter().map(Sent).collect();
    let json = serde_json::to_string(&sent).expect("job events serialize");
    format!("<skyhook_job_events>\n{json}\n</skyhook_job_events>")
}

/// This presentation is independent of the JSON used by tools and the journal.
impl fmt::Display for RuntimeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<skyhook_state>\ndate:{}\n", self.date)?;
        if !self.jobs.is_empty() {
            f.write_str("jobs: job parent tool name state age_s turns tool_calls\n")?;
            render_jobs(f, &self.jobs, None, &self.location)?;
        }
        if !self.todos.is_empty() {
            f.write_str("todos:\n")?;
            let mut previous = None;
            for item in &self.todos {
                if previous != Some(item.status) {
                    writeln!(f, "{}:", item.status)?;
                    previous = Some(item.status);
                }
                writeln!(f, "  {}", quoted(item.text.as_str()))?;
            }
        }
        if let Some(host) = &self.host {
            writeln!(f, "host:\n{}", host.as_str())?;
        }
        f.write_str("</skyhook_state>")
    }
}

fn render_jobs(
    f: &mut fmt::Formatter<'_>,
    jobs: &[StateJob],
    parent: Option<JobId>,
    location: &ExecutionLocation,
) -> fmt::Result {
    for job in jobs {
        write!(
            f,
            "{} {} {} {} {} {}",
            job.job,
            parent.map_or_else(|| "-".to_owned(), |id| id.to_string()),
            cell(job.kind.tool()),
            job.name.as_ref().map_or("-", JobName::as_str),
            job.state,
            job.age_seconds,
        )?;
        match &job.kind {
            StateJobKind::Agent { progress } => {
                write!(f, " {} {}", progress.turns, progress.tool_calls)?;
            }
            StateJobKind::Tool { .. } => f.write_str(" - -")?,
        }
        // Every row compares against the snapshot context, never its parent row.
        if let Some(target) = &job.target
            && target != &location.target
        {
            write!(f, " target={}", quoted(target.as_str()))?;
        }
        if job.workspace != location.workspace {
            write!(f, " workspace={}", quoted(&job.workspace.to_string_lossy()))?;
        }
        f.write_str("\n")?;
        render_jobs(f, &job.children, Some(job.job), location)?;
    }
    Ok(())
}

fn quoted(value: &str) -> String {
    serde_json::to_string(value).expect("strings are serializable")
}

/// Keep common tool/job names bare, but escape anything that could alter a row.
fn cell(value: &str) -> String {
    if !value.is_empty()
        && value != "-"
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-./".contains(&byte))
    {
        value.to_owned()
    } else {
        quoted(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::TodoStatus;

    /// A host event's view naming its own `kind` is refused where it is built and where the
    /// journal reads it back.
    #[test]
    fn a_host_view_names_no_kind() {
        let named = serde_json::json!({"kind": "notice", "changes": []});
        let serde_json::Value::Object(map) = named.clone() else {
            panic!("an object")
        };
        assert_eq!(HostView::try_from(map), Err(ViewNamesKind));
        assert!(serde_json::from_value::<HostView>(named).is_err());
    }

    fn location() -> ExecutionLocation {
        ExecutionLocation::root("/project".into())
    }

    fn state(jobs: Vec<StateJob>, todos: Vec<TodoItem>, location: ExecutionLocation) -> String {
        RuntimeState {
            date: "2026-09-10".into(),
            jobs,
            todos,
            location,
            host: None,
        }
        .to_string()
    }

    fn job(id: u64, tool: &str, workspace: &str) -> StateJob {
        StateJob {
            job: JobId::new(id).unwrap(),
            kind: if tool == crate::tool::builtins::names::AGENT {
                StateJobKind::Agent {
                    progress: AgentProgress::default(),
                }
            } else {
                StateJobKind::Tool {
                    tool: tool.to_owned(),
                }
            },
            name: None,
            state: JobState::Running,
            target: Some(TargetRef::Root),
            workspace: workspace.into(),
            age_seconds: 12,
            children: Vec::new(),
        }
    }

    #[test]
    fn todos_group_only_consecutive_statuses_and_escape_text() {
        assert_eq!(
            state(vec![], vec![], location()),
            "<skyhook_state>\ndate:2026-09-10\n</skyhook_state>"
        );
        let todos = [
            (TodoStatus::InProgress, "Design"),
            (TodoStatus::InProgress, "Measure"),
            (TodoStatus::Pending, "Implement"),
            (TodoStatus::Pending, "Test\n\"quoted\" \\ Unicode: λ"),
            (TodoStatus::Completed, "Inspect"),
            (TodoStatus::Pending, "Document"),
        ]
        .map(|(status, text)| TodoItem {
            status,
            text: text.parse().unwrap(),
        });
        assert_eq!(
            state(vec![], todos.to_vec(), location()),
            concat!(
                "<skyhook_state>\ndate:2026-09-10\ntodos:\n",
                "in_progress:\n  \"Design\"\n  \"Measure\"\n",
                "pending:\n  \"Implement\"\n  \"Test\\n\\\"quoted\\\" \\\\ Unicode: λ\"\n",
                "completed:\n  \"Inspect\"\npending:\n  \"Document\"\n",
                "</skyhook_state>"
            )
        );
    }

    #[test]
    fn job_rows_keep_snapshot_locations_exact_counters_and_row_structure() {
        // Nested jobs use their own snapshot location, not the parent's.
        let mut parent = job(7, "agent", "/other");
        parent.name = Some("runtime-review".parse().unwrap());
        parent.target = Some("remote".parse().unwrap());
        parent.kind = StateJobKind::Agent {
            progress: AgentProgress {
                turns: 3,
                tool_calls: 8,
            },
        };
        let mut child = job(9, "agent", "/project");
        child.state = JobState::WaitingInput;
        let mut grandchild = job(12, "agent", "/other");
        grandchild.target = Some("remote".parse().unwrap());
        child.children.push(grandchild);
        parent.children.push(child);
        // Target and workspace overrides are independent; hidden targets stay hidden.
        let mut target_only = job(1, "agent", "/project");
        target_only.target = Some("remote".parse().unwrap());
        let mut hidden_target = job(3, "exec", "/project");
        hidden_target.target = None;
        // A remote snapshot uses its own context as the default.
        let mut same = job(1, "agent", "/remote-project");
        same.target = Some("remote".parse().unwrap());
        let remote = ExecutionLocation::named("remote".parse().unwrap(), "/remote-project".into());
        // Arbitrary strings cannot change row structure.
        let item = job(1, "custom\n\"tool\"", "/other\n\"dir\"");
        let cases = [
            (
                location(),
                vec![parent, job(18, "exec", "/project")],
                concat!(
                    "7 - agent runtime-review running 12 3 8 target=\"remote\" workspace=\"/other\"\n",
                    "9 7 agent - waiting_input 12 0 0\n",
                    "12 9 agent - running 12 0 0 target=\"remote\" workspace=\"/other\"\n",
                    "18 - exec - running 12 - -\n",
                ),
            ),
            (
                location(),
                vec![target_only, job(2, "exec", "/other"), hidden_target],
                concat!(
                    "1 - agent - running 12 0 0 target=\"remote\"\n",
                    "2 - exec - running 12 - - workspace=\"/other\"\n",
                    "3 - exec - running 12 - -\n",
                ),
            ),
            (
                remote,
                vec![same, job(2, "exec", "/project")],
                concat!(
                    "1 - agent - running 12 0 0\n",
                    "2 - exec - running 12 - - target=\"root\" workspace=\"/project\"\n",
                ),
            ),
            (
                location(),
                vec![item],
                "1 - \"custom\\n\\\"tool\\\"\" - running 12 - - workspace=\"/other\\n\\\"dir\\\"\"\n",
            ),
        ];
        for (context, jobs, rows) in cases {
            let header = "jobs: job parent tool name state age_s turns tool_calls";
            let expected =
                format!("<skyhook_state>\ndate:2026-09-10\n{header}\n{rows}</skyhook_state>");
            assert_eq!(state(jobs, vec![], context), expected);
        }
        assert_eq!(cell("run-tests"), "run-tests");
        assert_eq!(cell("-"), "\"-\"");
        assert_eq!(cell(""), "\"\"");
        assert_eq!(cell("two words"), "\"two words\"");
        assert_eq!(cell("λ"), "\"λ\"");
    }

    use crate::provider::codec::common::tests::envelope;
    use crate::provider::protocol::{Binding, ItemId, Position, Replay, ReplayFormat};
    use serde_json::json;

    fn replay(binding: Binding) -> Replay {
        envelope(ReplayFormat::Messages, "model", json!({}), binding)
    }

    fn id(id: &str) -> ItemId {
        ItemId::try_from(id.to_owned()).unwrap()
    }

    /// Items whose blocks were all dropped, as a malformed decode would leave them.
    fn blockless_text() -> AssistantItem {
        AssistantItem::Text {
            id: id("text"),
            position: Position::try_from(0_usize).unwrap(),
            blocks: Vec::new(),
        }
    }

    fn blockless_reasoning(replay: Option<Replay>) -> AssistantItem {
        AssistantItem::Reasoning {
            id: id("reasoning"),
            position: Position::try_from(0_usize).unwrap(),
            blocks: Vec::new(),
            replay,
        }
    }

    /// Only nonblank text or a call makes an assistant turn; reasoning, with or
    /// without replay, and blank text ride along.
    #[test]
    fn content_free_means_no_nonblank_text_and_no_call() {
        assert!(Message::Assistant(Vec::new()).is_content_free());
        assert!(Message::Assistant(vec![blockless_text()]).is_content_free());
        assert!(Message::Assistant(vec![blockless_reasoning(None)]).is_content_free());
        let replayed = blockless_reasoning(Some(replay(Binding::Conversation)));
        assert!(Message::Assistant(vec![replayed]).is_content_free());
        let reasoning = AssistantItem::reasoning("r", 0, "thought", None);
        assert!(Message::Assistant(vec![reasoning]).is_content_free());
        for text in ["", " ", " \n", "\n\t "] {
            let item = AssistantItem::text("answer", 0, text);
            assert!(Message::Assistant(vec![item]).is_content_free(), "{text:?}");
        }
        let visible = AssistantItem::text("answer", 0, "hello");
        assert!(!Message::Assistant(vec![visible]).is_content_free());
        // Only assistant messages can be content-free.
        assert!(!Message::User(Vec::new()).is_content_free());
    }

    #[test]
    fn unbinding_drops_only_conversation_bound_replay() {
        let free = replay(Binding::Free);
        let message = Message::Assistant(vec![
            AssistantItem::reasoning("bound", 0, "visible", Some(replay(Binding::Conversation))),
            AssistantItem::reasoning("free", 1, "kept", Some(free.clone())),
            AssistantItem::text("said", 2, "answer"),
        ]);
        let Message::Assistant(items) = message.without_bound_reasoning() else {
            unreachable!()
        };
        assert_eq!(items[0].replay(), None);
        assert_eq!(items[1].replay(), Some(&free));
    }
}
