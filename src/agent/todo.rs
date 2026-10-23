//! Durable per-agent advisory checklists.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    Prose,
    identity::{AgentId, JobId},
    named_enum::named_enum,
    session::{
        Command, EventRecord, MessageSeq, Plan, RecordSeq, SessionError, SessionEvent, SessionStore,
    },
    tool::{
        ToolError,
        diagnostic::{Operation, Subject},
    },
};

named_enum! {
    #[derive(Clone, Copy, Debug, Serialize, JsonSchema, PartialEq, Eq)]
    pub enum TodoStatus {
        Pending = "pending",
        InProgress = "in_progress",
        Completed = "completed",
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TodoItem<Text = Prose> {
    /// Instruction or step to carry out.
    #[schemars(with = "String")]
    pub text: Text,
    pub status: TodoStatus,
}

/// One agent's list as the core folds it.
#[derive(Default)]
pub(crate) struct AgentTodos {
    owner_job: Option<JobId>,
    items: Vec<TodoItem>,
    /// The latest checkpoint's frontier: its summary reconciled every response up to it.
    reconciled: RecordSeq,
    /// The record that last set the list.
    set: RecordSeq,
}

impl AgentTodos {
    /// The empty list of an agent `owner_job` owns, whose latest checkpoint reconciled
    /// every response up to `reconciled`, to fold the record that last set it into.
    pub(crate) fn opened(owner_job: Option<JobId>, reconciled: RecordSeq) -> Self {
        Self {
            owner_job,
            reconciled,
            ..Self::default()
        }
    }
}

/// Fold one committed record into every agent's list.
pub(crate) fn apply(agents: &mut BTreeMap<AgentId, AgentTodos>, record: &EventRecord) {
    let agent = record.agent.clone();
    match &record.event {
        SessionEvent::AgentStarted { owner_job, .. } => {
            agents.entry(agent).or_default().owner_job = *owner_job;
        }
        SessionEvent::TodosReplaced { items } => {
            let state = agents.entry(agent).or_default();
            state.items.clone_from(items);
            state.set = record.sequence;
        }
        SessionEvent::Compaction { checkpoint } => {
            let state = agents.entry(agent).or_default();
            state.items.clone_from(&checkpoint.todos);
            state.reconciled = checkpoint.frontier;
            state.set = record.sequence;
        }
        _ => {}
    }
}

/// Whether `agent`'s list changed after `frontier`, which makes the list a summary up
/// to it reconciled stale.
pub(crate) fn changed_after(
    agents: &BTreeMap<AgentId, AgentTodos>,
    agent: &AgentId,
    frontier: RecordSeq,
) -> bool {
    agents.get(agent).is_some_and(|state| state.set > frontier)
}

/// Why a replacement was not applied.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ReplaceError {
    /// The summary that installed the current list already reconciled this replacement.
    #[error(
        "superseded: compaction reconciled the todo list after the response that issued this call; \
         the current list already accounts for it, so do not retry"
    )]
    Superseded,
    #[error(transparent)]
    Session(#[from] SessionError),
}

impl SessionStore {
    /// Replace `agent`'s list on behalf of the response `issued_by`, if a model call
    /// issued it. A checkpoint that reconciled that response supersedes it.
    pub(crate) async fn replace_todos(
        &self,
        agent: &AgentId,
        items: Vec<TodoItem>,
        issued_by: Option<MessageSeq>,
    ) -> Result<(), ReplaceError> {
        let agent = agent.clone();
        self.commit(Command::Journal, move |state| {
            let reconciled = state.todos.get(&agent).map(|state| state.reconciled);
            let reconciled = reconciled.unwrap_or_default();
            if issued_by.is_some_and(|message| RecordSeq::from(message) <= reconciled) {
                return Err(ReplaceError::Superseded);
            }
            let replaced = SessionEvent::TodosReplaced { items };
            Ok(Plan::journal(vec![(agent, replaced)]))
        })
        .await
    }

    /// The caller's own list, or with `job` the list of the descendant it launched.
    pub(crate) fn inspect_todos(
        &self,
        caller: &AgentId,
        job: Option<JobId>,
    ) -> Result<Vec<TodoItem>, ToolError> {
        self.read(|state| {
            let agents = &state.todos;
            let agent = if let Some(job) = job {
                let agent = agents
                    .iter()
                    .find_map(|(agent, state)| (state.owner_job == Some(job)).then_some(agent))
                    .ok_or_else(|| {
                        ToolError::invalid_arguments(
                            "job does not identify an initialized child agent",
                        )
                        .operation(Operation::Lookup, Subject::Job(job))
                    })?;
                if agent == caller || !agent.is_within(caller) {
                    return Err(ToolError::invalid_arguments(
                        "todo inspection is limited to descendants",
                    )
                    .operation(Operation::Inspect, Subject::Job(job)));
                }
                agent
            } else {
                caller
            };
            Ok(agents
                .get(agent)
                .map_or_else(Vec::new, |state| state.items.clone()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejections_identify_the_rejected_job() {
        let fixture = crate::tests::TestRuntime::new().await;
        let (todos, jobs) = (&fixture.store, &fixture.jobs);
        let spec = crate::job::JobSpec {
            role: crate::job::JobRole::Agent,
            ..crate::job::JobSpec::test(fixture.agent.clone(), "agent")
        };
        let job = jobs.create(spec).await.unwrap().into_test_id();
        let missing = todos.inspect_todos(&fixture.agent, Some(job)).unwrap_err();
        let workspace = fixture.root.path();
        crate::session::tests::start_child(todos, &fixture.agent, 1, Some(job), workspace).await;
        let sibling = fixture.agent.child(2);
        let forbidden = todos.inspect_todos(&sibling, Some(job)).unwrap_err();
        for (error, operation) in [
            (missing, Operation::Lookup),
            (forbidden, Operation::Inspect),
        ] {
            let context = error.diagnostic().context;
            assert_eq!(context.operation, operation);
            assert_eq!(context.subject, Subject::Job(job));
        }
    }
}
