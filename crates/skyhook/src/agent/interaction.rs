//! Host interaction contracts and streamed runtime events.

use std::sync::Arc;
use std::{collections::HashMap, future::Future, pin::Pin};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::session::RequestSeq;
use crate::{
    identity::{AgentId, JobId},
    session::EventRecord,
};

/// The most characters a question's header holds.
pub const MAX_HEADER: usize = 24;

/// Why a header is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum HeaderError {
    #[error("a question header must not be blank")]
    Blank,
    #[error("a question header holds at most {MAX_HEADER} characters")]
    Long,
}

crate::newtype::string_newtype! {
    /// A question's short title: its tab among the questions asked together, unique
    /// among them, and the key its answer is filed under.
    #[derive(PartialOrd, Ord)]
    pub struct QuestionHeader(HeaderError) = |header| match header {
        header if header.trim().is_empty() => Err(HeaderError::Blank),
        header if header.chars().count() > MAX_HEADER => Err(HeaderError::Long),
        _ => Ok(()),
    }, schema = schemars::json_schema!({
        "type": "string", "minLength": 1, "maxLength": MAX_HEADER, "pattern": "\\S",
    });
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QuestionOption {
    /// Short answer label, returned as the answer when chosen.
    pub label: crate::Prose,
    /// What choosing it means: its effect or tradeoff.
    pub description: crate::Prose,
    /// The option you recommend: at most one, unless the question takes several.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recommended: bool,
    /// Markdown shown beside this option while it is focused: a short concrete artifact the
    /// user compares across options, such as a code or config snippet (fenced, with a
    /// language), a data shape, or a small ASCII mockup. Show only what differs from the
    /// other options, in the same framing for each; keep it under about 20 lines and 60
    /// columns. Give every option a preview or none. The tradeoff belongs in `description`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<crate::Prose>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub header: QuestionHeader,
    /// The complete question as Markdown, shown to the user or owning parent agent. Put
    /// lists and separate points on their own lines.
    pub prompt: crate::Prose,
    /// Suggested answers with distinct labels; free-form input is always allowed. A chosen
    /// suggestion answers with its label, or {"answer": "label", "comment": "text"} with a
    /// comment.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<QuestionOption>,
    /// Take several suggestions: the answer is then a list of labels and free-form text,
    /// or {"answers": [...], "comment": "text"} with a comment.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub multiple: bool,
}

impl Question {
    /// Why the question cannot be asked: labels repeat, it recommends more than one
    /// option while taking one, or it takes several with none to choose from.
    pub fn refusal(&self) -> Option<&'static str> {
        let mut labels = std::collections::HashSet::new();
        if !self
            .options
            .iter()
            .all(|option| labels.insert(option.label.as_str()))
        {
            return Some("option labels must be distinct");
        }
        let recommended = self
            .options
            .iter()
            .filter(|option| option.recommended)
            .count();
        if !self.multiple && recommended > 1 {
            return Some("a question taking one answer recommends at most one option");
        }
        if self.multiple && self.options.is_empty() {
            return Some("a question taking several answers needs options");
        }
        None
    }
}

#[cfg(test)]
impl Question {
    /// A question titled `header` without options.
    pub(crate) fn test(header: &str) -> Self {
        Self {
            header: header.parse().unwrap(),
            prompt: "Question?".parse().unwrap(),
            options: Vec::new(),
            multiple: false,
        }
    }
}

/// A suspended child's question batch, returned in its agent job's output.
/// Presentation never shortens it.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
pub(crate) struct QuestionOutput {
    #[schemars(transform = crate::tool::output::complete)]
    pub questions: Vec<Question>,
}

/// One question's answer: a suggestion's label or free-form text, or, for a question
/// taking several, a list of them; either with a comment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum QuestionAnswer {
    Text(String),
    Commented {
        answer: String,
        comment: String,
    },
    Selected(Vec<String>),
    SelectedCommented {
        answers: Vec<String>,
        comment: String,
    },
}

impl QuestionAnswer {
    /// `value` as an answer to `question`, when its shape fits the question.
    pub fn parse(question: &Question, value: serde_json::Value) -> Option<Self> {
        let answer = serde_json::from_value::<Self>(value).ok()?;
        let several = matches!(answer, Self::Selected(_) | Self::SelectedCommented { .. });
        (several == question.multiple).then_some(answer)
    }
}

/// A host's answers to one batch, keyed by `Question::header`.
pub type QuestionReply = HashMap<QuestionHeader, QuestionAnswer>;

pub type QuestionFuture =
    Pin<Box<dyn Future<Output = Result<QuestionReply, QuestionError>> + Send + 'static>>;

pub trait QuestionHandler: Send + Sync {
    /// Present a runtime-merged question batch and answer each question by its id.
    ///
    /// `background` is true if any ask, or any enclosing tool job up to the owning
    /// agent, runs in the background; a mixed batch is a background batch. Hosts may
    /// dismiss and reopen a foreground batch by retaining its pending future, but
    /// cancelling a background batch must resolve the future with an error: the
    /// running agent receives the failure and it cannot be undone.
    fn ask(&self, agent: AgentId, questions: Vec<Question>, background: bool) -> QuestionFuture;
}

pub type HostStateFuture<'a> = Pin<Box<dyn Future<Output = Option<crate::Prose>> + Send + 'a>>;

pub type HostEventsFuture<'a> = Pin<
    Box<
        dyn Future<
                Output = Result<
                    Option<crate::session::HostEvent>,
                    Box<dyn std::error::Error + Send + Sync>,
                >,
            > + Send
            + 'a,
    >,
>;

/// The host's own section of each agent's runtime state, and the events it has for
/// each agent.
///
/// Rendered by the host, placed by the harness: the state joins every request's state
/// block and the compaction summary's, so what it says is never lost to compaction.
pub trait HostState: Send + Sync {
    /// The section for `agent`'s next request in a session with the host's tables,
    /// `extension`; `None` adds nothing to that request. An interrupt drops the
    /// future and ends the request it was for.
    fn state<'a>(
        &'a self,
        extension: &'a crate::session::ExtensionDb,
        agent: &'a AgentId,
    ) -> HostStateFuture<'a>;

    /// What the host's log holds for `agent` after `since`, how far the last event it
    /// was given went, or from its start when it was given none. Asked only as a
    /// request is about to be made: the event joins the job events delivered with it,
    /// in one commit with how far it went, so each is given once and none starts,
    /// reopens or keeps open the agent's work, or ends a wait. `None` gives nothing; a
    /// failure fails the request. An interrupt drops the future and commits nothing.
    fn events<'a>(
        &'a self,
        _extension: &'a crate::session::ExtensionDb,
        _agent: &'a AgentId,
        _since: Option<crate::session::HostCursor>,
    ) -> HostEventsFuture<'a> {
        Box::pin(std::future::ready(Ok(None)))
    }
}

pub type ResumeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send + 'a>>;

/// What this launch changes about the work of a session it reopens.
pub trait ResumeHook: Send + Sync {
    /// Called once a reopened session is about to take its first work in this
    /// harness: a prompt, queued prompts, a continue or a script, or
    /// [`SessionHandle::resume_work`](super::SessionHandle::resume_work). A session
    /// only viewed never reaches it. A failure refuses that work and the next work
    /// calls it again. An interrupt while it runs withdraws the work waiting on it
    /// and drops it unfinished, so the next work calls it again. Shutdown does the
    /// same, and returns only once it is dropped.
    fn resumed<'a>(&'a self, extension: &'a crate::session::ExtensionDb) -> ResumeFuture<'a>;
}

#[derive(Clone, Debug, Error)]
pub enum QuestionError {
    #[error("no host question handler is configured")]
    Unavailable,
    #[error("question failed: {0}")]
    Failed(String),
}

#[derive(Clone, Debug)]
pub enum RuntimeEvent {
    Record(Arc<EventRecord>),
    /// A job as the record that follows leaves it.
    Job(Box<crate::job::JobEnvelope>),
    /// Jobs the session no longer holds: what last moved them is their last view,
    /// until a record moves one again, which reports it anew.
    JobsUnheld(Vec<JobId>),
    /// `request`'s response streamed after it was last read: read it with
    /// [`SessionStore::response`](crate::session::SessionStore::response). No further
    /// one is posted for it until it is read again, or a record of its agent applies.
    ResponseChanged {
        agent: AgentId,
        request: RequestSeq,
    },
    ResponseSettled {
        agent: AgentId,
        request: RequestSeq,
        settlement: super::Settlement,
    },
    /// An agent's activity moved, as the session's state reads it; none once the
    /// agent no longer runs, as a parked child does not.
    Activity {
        agent: AgentId,
        activity: Option<super::ObservedActivity>,
    },
    TurnCompleted {
        agent: AgentId,
    },
    /// Run `invocation` of `job` captured output after its output was last read.
    /// No further one is posted for that run until its output is read again or
    /// the run finishes.
    OutputChanged {
        job: JobId,
        invocation: crate::job::Invocation,
    },
    /// The phase the session moved to.
    Phase(crate::session::SessionPhase),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn option(label: &str, recommended: bool) -> QuestionOption {
        QuestionOption {
            label: label.parse().unwrap(),
            description: "Why.".parse().unwrap(),
            recommended,
            preview: None,
        }
    }

    #[test]
    fn headers_are_short_and_not_blank() {
        assert_eq!("  ".parse::<QuestionHeader>(), Err(HeaderError::Blank));
        let longest = "é".repeat(MAX_HEADER);
        assert!(longest.parse::<QuestionHeader>().is_ok());
        let long = format!("{longest}x").parse::<QuestionHeader>();
        assert_eq!(long, Err(HeaderError::Long));
    }

    /// A question taking one answer recommends at most one option; one taking several
    /// may recommend any, but needs options; labels never repeat.
    #[test]
    fn questions_refuse_what_their_answers_cannot_take() {
        let with = |multiple, options| Question {
            multiple,
            options,
            ..Question::test("pick")
        };
        let both = || vec![option("a", true), option("b", true)];
        assert!(with(false, both()).refusal().is_some());
        assert!(with(true, both()).refusal().is_none());
        assert!(with(true, Vec::new()).refusal().is_some());
        assert!(
            with(false, vec![option("a", false), option("a", false)])
                .refusal()
                .is_some()
        );
    }

    #[test]
    fn answers_take_the_shape_their_question_does() {
        let one = Question::test("one");
        let several = Question {
            multiple: true,
            options: vec![option("a", false)],
            ..Question::test("several")
        };
        let commented = json!({"answer": "a", "comment": "why"});
        let listed = json!({"answers": ["a", "free"], "comment": "why"});
        for (question, fits, misfits) in [
            (
                &one,
                [json!("a"), commented.clone()],
                [json!(["a"]), listed.clone()],
            ),
            (
                &several,
                [json!(["a", "free"]), listed],
                [json!("a"), commented],
            ),
        ] {
            for answer in fits {
                let parsed = QuestionAnswer::parse(question, answer.clone()).unwrap();
                assert_eq!(serde_json::to_value(parsed).unwrap(), answer);
            }
            for answer in misfits {
                assert_eq!(QuestionAnswer::parse(question, answer), None);
            }
        }
        assert_eq!(QuestionAnswer::parse(&one, json!(1)), None);
    }
}
