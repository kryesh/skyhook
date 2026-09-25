//! Durable per-agent advisory checklists.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
    identity::{AgentId, JobId},
    named_enum::named_enum,
    session::{
        CompactionCheckpoint, EventRecord, RecordSeq, SessionError, SessionEvent, SessionStore,
    },
    tool::{
        ToolError,
        diagnostic::{Effects, Operation, Subject},
    },
};

named_enum! {
    #[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
    pub enum TodoStatus {
        Pending = "pending",
        InProgress = "in_progress",
        Completed = "completed",
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TodoItem {
    /// Instruction or step to carry out.
    pub text: String,
    pub status: TodoStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct TodoSnapshot {
    pub agent: AgentId,
    pub items: Vec<TodoItem>,
}

#[derive(Default)]
struct AgentTodos {
    owner_job: Option<JobId>,
    items: Vec<TodoItem>,
    revision: RecordSeq,
}

pub(super) struct TodoStore {
    store: SessionStore,
    agents: Mutex<BTreeMap<AgentId, AgentTodos>>,
}

impl TodoStore {
    pub fn restore(store: SessionStore, records: &[EventRecord]) -> Self {
        let mut agents = BTreeMap::<AgentId, AgentTodos>::new();
        for record in records {
            match &record.event {
                SessionEvent::AgentStarted { owner_job, .. } => {
                    agents.entry(record.agent.clone()).or_default().owner_job = *owner_job;
                }
                SessionEvent::TodosReplaced { items } => {
                    let state = agents.entry(record.agent.clone()).or_default();
                    state.items.clone_from(items);
                    state.revision = record.sequence;
                }
                SessionEvent::Compaction { checkpoint } => {
                    let state = agents.entry(record.agent.clone()).or_default();
                    state.items.clone_from(&checkpoint.todos);
                    state.revision = record.sequence;
                }
                _ => {}
            }
        }
        Self {
            store,
            agents: Mutex::new(agents),
        }
    }

    pub async fn register(
        &self,
        agent: AgentId,
        owner_job: Option<JobId>,
        seed: Option<Vec<TodoItem>>,
    ) -> Result<(), SessionError> {
        // Publish the job association and seed together so inspection cannot see an
        // empty list between registering the child and persisting its initial instructions.
        let mut agents = self.agents.lock().await;
        let revision = if let Some(items) = &seed {
            Some(
                self.store
                    .append(
                        agent.clone(),
                        SessionEvent::TodosReplaced {
                            items: items.clone(),
                        },
                    )
                    .await?
                    .sequence,
            )
        } else {
            None
        };
        let state = agents.entry(agent).or_default();
        state.owner_job = owner_job;
        if let Some(items) = seed {
            state.items = items;
            state.revision = revision.expect("seed was journaled");
        }
        Ok(())
    }

    pub fn validate(items: &[TodoItem]) -> Result<(), ToolError> {
        if let Some(index) = items.iter().position(|item| item.text.trim().is_empty()) {
            return Err(ToolError::invalid_arguments("todo text cannot be blank")
                .operation(
                    Operation::Validate,
                    Subject::Label(format!("todo item at index {index}")),
                )
                .effects(Effects::NotStarted));
        }
        Ok(())
    }

    pub async fn replace(
        &self,
        agent: &AgentId,
        items: Vec<TodoItem>,
    ) -> Result<TodoSnapshot, SessionError> {
        // Hold the lock across persistence so published snapshots and replay agree on order.
        let mut agents = self.agents.lock().await;
        let record = self
            .store
            .append(
                agent.clone(),
                SessionEvent::TodosReplaced {
                    items: items.clone(),
                },
            )
            .await?;
        let state = agents.entry(agent.clone()).or_default();
        state.items.clone_from(&items);
        state.revision = record.sequence;
        Ok(TodoSnapshot {
            agent: agent.clone(),
            items,
        })
    }

    /// Journal and publish history and todos together. A newer todo mutation
    /// invalidates the summary's snapshot and must be reconciled by a fresh attempt.
    pub(crate) async fn commit_compaction(
        &self,
        agent: &AgentId,
        checkpoint: CompactionCheckpoint,
    ) -> Result<bool, SessionError> {
        let mut agents = self.agents.lock().await;
        let state = agents.entry(agent.clone()).or_default();
        if state.revision > checkpoint.frontier {
            return Ok(false);
        }
        let items = checkpoint.todos.clone();
        let record = self
            .store
            .append(agent.clone(), SessionEvent::Compaction { checkpoint })
            .await?;
        state.items = items;
        state.revision = record.sequence;
        Ok(true)
    }

    pub async fn inspect(
        &self,
        caller: &AgentId,
        job: Option<JobId>,
    ) -> Result<TodoSnapshot, ToolError> {
        let agents = self.agents.lock().await;
        let agent = if let Some(job) = job {
            let agent = agents
                .iter()
                .find_map(|(agent, state)| (state.owner_job == Some(job)).then_some(agent))
                .ok_or_else(|| {
                    ToolError::invalid_arguments("job does not identify an initialized child agent")
                        .operation(Operation::Lookup, Subject::Job(job))
                        .effects(Effects::Unchanged)
                })?;
            if agent == caller || !agent.is_within(caller) {
                return Err(ToolError::invalid_arguments(
                    "todo inspection is limited to descendants",
                )
                .operation(Operation::Inspect, Subject::Job(job))
                .effects(Effects::Unchanged));
            }
            agent
        } else {
            caller
        };
        Ok(TodoSnapshot {
            agent: agent.clone(),
            items: agents
                .get(agent)
                .map_or_else(Vec::new, |state| state.items.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejections_identify_the_blank_item_and_the_rejected_job() {
        let item = |text: &str| TodoItem {
            text: text.into(),
            status: TodoStatus::Pending,
        };
        let blank = TodoStore::validate(&[item("task"), item("  ")]).unwrap_err();
        assert_eq!(
            blank.diagnostic().context.subject,
            Subject::Label("todo item at index 1".into())
        );

        let fixture = crate::session::fixture::MemorySession::new().await;
        let todos = TodoStore::restore(fixture.store, &[]);
        let job = JobId::new(17).unwrap();
        let missing = todos.inspect(&fixture.agent, Some(job)).await.unwrap_err();
        todos
            .register(fixture.agent.child(1), Some(job), None)
            .await
            .unwrap();
        let sibling = fixture.agent.child(2);
        let forbidden = todos.inspect(&sibling, Some(job)).await.unwrap_err();
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
