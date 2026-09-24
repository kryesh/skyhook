//! The durable conversation: what the session journals and projects. Providers
//! see it rendered: runtime state and job events become text at that boundary,
//! so the renderers here are part of the session format.

use std::{borrow::Cow, fmt, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    agent::TodoItem,
    execution::ExecutionLocation,
    identity::JobId,
    job::{AgentMessage, AgentProgress, JobState, JobView},
    media::AttachmentRef,
    provider::protocol::{self, AssistantItem, ToolResult},
    target::TargetRef,
};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "role", content = "content", rename_all = "snake_case")]
pub enum Message {
    User(Vec<UserPart>),
    Assistant(Vec<AssistantItem>),
    Tool(Vec<ToolResult>),
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
    /// The agent's state at a request.
    State {
        state: RuntimeState,
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
    /// An assistant message that yields no content blocks at all, and so cannot be
    /// encoded into a later request: Anthropic rejects it outright, and Chat/Responses
    /// drop it silently. It must never reach the append-only journal, because a
    /// committed one makes every subsequent request fail.
    ///
    /// Emptiness here is structural, never a judgement about text. An empty or
    /// whitespace-only text block is content: providers legitimately emit one
    /// alongside tool calls on a non-final turn, and it replays without complaint.
    /// Reasoning is content whenever it carries replay state, or a block that an
    /// encoder may render.
    #[must_use]
    pub fn is_content_free(&self) -> bool {
        match self {
            Self::Assistant(items) => items.iter().all(AssistantItem::is_content_free),
            Self::User(_) | Self::Tool(_) => false,
        }
    }

    /// Drop replay bound to the conversation that produced it, keeping display text. Changing
    /// that conversation, as compaction or a mode switch does, invalidates such replay; other
    /// replay is kept. A message left content-free cannot be encoded and is dropped whole.
    #[must_use]
    pub fn without_bound_reasoning(mut self) -> Option<Self> {
        if let Self::Assistant(items) = &mut self {
            items.iter_mut().for_each(AssistantItem::unbind);
        }
        (!self.is_content_free()).then_some(self)
    }

    /// The message as a provider receives it: runtime state and job events as
    /// runtime text, parent input and compaction summaries as user text.
    #[must_use]
    pub fn render(&self) -> protocol::Message {
        match self {
            Self::User(parts) => {
                protocol::Message::User(parts.iter().map(UserPart::render).collect())
            }
            Self::Assistant(items) => protocol::Message::Assistant(items.clone()),
            Self::Tool(results) => protocol::Message::Tool(results.clone()),
        }
    }
}

impl UserPart {
    #[must_use]
    pub fn is_image(&self) -> bool {
        matches!(
            self,
            Self::Attachment {
                attachment: AttachmentRef::Image(_)
            }
        )
    }

    /// The text the model reads, or the attachment whose content is loaded separately.
    pub fn text(&self) -> Result<Cow<'_, str>, &AttachmentRef> {
        match self {
            Self::Text { text } | Self::ParentInput { text } | Self::Compaction { text } => {
                Ok(Cow::Borrowed(text))
            }
            Self::State { state } => Ok(Cow::Owned(state.to_string())),
            Self::JobEvents { events } => Ok(Cow::Owned(job_events_text(events))),
            Self::Attachment { attachment } => Err(attachment),
        }
    }

    fn render(&self) -> protocol::UserContent {
        match self {
            Self::Attachment { attachment } => protocol::UserContent::Attachment {
                attachment: attachment.clone(),
            },
            Self::Text { text } | Self::ParentInput { text } | Self::Compaction { text } => {
                protocol::UserContent::Text { text: text.clone() }
            }
            Self::State { state } => protocol::UserContent::Runtime {
                text: state.to_string(),
            },
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
}

/// A live job in the state, with the active agent jobs it owns beneath it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct StateJob {
    pub job: JobId,
    pub kind: StateJobKind,
    pub name: Option<String>,
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
    /// The tool name the state shows; every child agent runs the `agent` tool.
    pub const AGENT: &'static str = "agent";

    #[must_use]
    pub fn tool(&self) -> &str {
        match self {
            Self::Agent { .. } => Self::AGENT,
            Self::Tool { tool } => tool,
        }
    }
}

/// One entry of a job-events notification: a child's reply or a job's presented view.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobEvent {
    Message(AgentMessage),
    Job(Box<JobView>),
}

/// The text a job-events notification sends the model.
fn job_events_text(events: &[JobEvent]) -> String {
    let json = serde_json::to_string(events).expect("job events serialize");
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
                writeln!(f, "  {}", quoted(&item.text))?;
            }
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
            job.name.as_deref().map_or_else(|| "-".to_owned(), cell),
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

    fn location() -> ExecutionLocation {
        ExecutionLocation::root("/project".into())
    }

    fn state(jobs: Vec<StateJob>, todos: Vec<TodoItem>, location: ExecutionLocation) -> String {
        RuntimeState {
            date: "2026-09-10".into(),
            jobs,
            todos,
            location,
        }
        .to_string()
    }

    fn job(id: u64, tool: &str, workspace: &str) -> StateJob {
        StateJob {
            job: JobId::new(id).unwrap(),
            kind: if tool == StateJobKind::AGENT {
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
    fn empty_state_contains_only_date() {
        assert_eq!(
            state(vec![], vec![], location()),
            "<skyhook_state>\ndate:2026-09-10\n</skyhook_state>"
        );
    }

    #[test]
    fn todos_group_only_consecutive_statuses_and_escape_text() {
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
            text: text.to_owned(),
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
        parent.name = Some("runtime-review".to_owned());
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
        let mut item = job(1, "custom\n\"tool\"", "/other\n\"dir\"");
        item.name = Some("-".to_owned());
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
                "1 - \"custom\\n\\\"tool\\\"\" \"-\" running 12 - - workspace=\"/other\\n\\\"dir\\\"\"\n",
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
}

#[cfg(test)]
mod content_free_tests {
    use super::*;
    use crate::provider::protocol::{
        Binding, ItemId, Position, Provenance, Replay, Scope, ToolCall,
    };
    use serde_json::json;

    fn envelope() -> Replay {
        Replay {
            provenance: Provenance {
                protocol: "anthropic".into(),
                model: "model".into(),
                scope: Scope::try_from("scope".to_owned()).unwrap(),
            },
            payload: json!({"type":"thinking","thinking":"private","signature":"signed"}),
            binding: Binding::Conversation,
        }
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

    #[test]
    fn content_free_means_no_blocks_at_all_not_empty_text() {
        // The only unencodable shapes: no items, or items carrying no blocks and
        // no replay state. A tool call is always content.
        assert!(Message::Assistant(Vec::new()).is_content_free());
        assert!(Message::Assistant(vec![blockless_text()]).is_content_free());
        assert!(Message::Assistant(vec![blockless_reasoning(None)]).is_content_free());
        // Only assistant messages can be content-free.
        assert!(!Message::User(Vec::new()).is_content_free());
        assert!(!Message::Tool(Vec::new()).is_content_free());
    }

    #[test]
    fn empty_and_whitespace_text_blocks_remain_content() {
        // Providers emit an empty or blank text block alongside tool calls on a
        // non-final turn. Such a block encodes and replays, so it is not a failure.
        for text in ["", " ", "\n\t "] {
            let item = AssistantItem::text("answer", 0, text);
            assert!(!Message::Assistant(vec![item]).is_content_free());
        }
        let call = ToolCall::new("call", "shell", json!({})).unwrap();
        let non_final = vec![
            AssistantItem::text("answer", 0, ""),
            AssistantItem::tool_call("tool-1", 1, call),
        ];
        assert!(!Message::Assistant(non_final).is_content_free());
        // Reasoning is content through a rendered block or through replay state.
        let blank_prose = AssistantItem::reasoning("thought", 0, "   ", None);
        assert!(!Message::Assistant(vec![blank_prose]).is_content_free());
        let signed = blockless_reasoning(Some(envelope()));
        assert!(!Message::Assistant(vec![signed]).is_content_free());
        let visible = AssistantItem::text("answer", 0, "hello");
        assert!(!Message::Assistant(vec![visible]).is_content_free());
    }

    #[test]
    fn unbinding_drops_only_conversation_bound_replay_and_empties_fall_away() {
        let free = Replay {
            binding: Binding::Free,
            ..envelope()
        };
        let message = Message::Assistant(vec![
            AssistantItem::reasoning("bound", 0, "visible", Some(envelope())),
            AssistantItem::reasoning("free", 1, "kept", Some(free.clone())),
        ]);
        let Some(Message::Assistant(items)) = message.without_bound_reasoning() else {
            panic!("readable text keeps the message")
        };
        assert_eq!(items[0].replay(), None);
        assert_eq!(items[1].replay(), Some(&free));
        let signed_only = Message::Assistant(vec![blockless_reasoning(Some(envelope()))]);
        assert_eq!(signed_only.without_bound_reasoning(), None);
    }
}
