//! Durable per-agent advisory checklists.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
    Prose,
    identity::{AgentId, JobId},
    named_enum::named_enum,
    session::{CompactionCheckpoint, EventRecord, SessionError, SessionEvent, SessionStore},
    tool::{
        ToolError,
        diagnostic::{Effects, Operation, Subject},
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

#[derive(Default)]
struct AgentTodos {
    owner_job: Option<JobId>,
    items: Vec<TodoItem>,
}

pub(super) struct TodoStore {
    store: SessionStore,
    agents: Mutex<BTreeMap<AgentId, AgentTodos>>,
}

impl TodoStore {
    pub fn restore(store: SessionStore, records: &[EventRecord]) -> Self {
        let mut agents = BTreeMap::<AgentId, AgentTodos>::new();
        for record in records {
            let items = match &record.event {
                SessionEvent::AgentStarted { owner_job, .. } => {
                    agents.entry(record.agent.clone()).or_default().owner_job = *owner_job;
                    continue;
                }
                SessionEvent::TodosReplaced { items } => items,
                SessionEvent::Compaction { checkpoint } => &checkpoint.todos,
                _ => continue,
            };
            let state = agents.entry(record.agent.clone()).or_default();
            state.items.clone_from(items);
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
        if let Some(items) = &seed {
            let seeded = SessionEvent::TodosReplaced {
                items: items.clone(),
            };
            self.store.append(agent.clone(), seeded).await?;
        }
        let state = agents.entry(agent).or_default();
        state.owner_job = owner_job;
        if let Some(items) = seed {
            state.items = items;
        }
        Ok(())
    }

    pub async fn replace(&self, agent: &AgentId, items: Vec<TodoItem>) -> Result<(), SessionError> {
        // Hold the lock across persistence so published snapshots and replay agree on order.
        let mut agents = self.agents.lock().await;
        let replaced = SessionEvent::TodosReplaced {
            items: items.clone(),
        };
        self.store.append(agent.clone(), replaced).await?;
        agents.entry(agent.clone()).or_default().items = items;
        Ok(())
    }

    /// Journal and publish history and todos together. The journal refuses a
    /// checkpoint that a newer todo mutation made stale; a fresh attempt reconciles it.
    pub(crate) async fn commit_compaction(
        &self,
        agent: &AgentId,
        checkpoint: CompactionCheckpoint,
    ) -> Result<(), SessionError> {
        let mut agents = self.agents.lock().await;
        let items = checkpoint.todos.clone();
        self.store
            .append(agent.clone(), SessionEvent::Compaction { checkpoint })
            .await?;
        agents.entry(agent.clone()).or_default().items = items;
        Ok(())
    }

    /// The caller's own list, or with `job` the list of the descendant it launched.
    pub async fn inspect(
        &self,
        caller: &AgentId,
        job: Option<JobId>,
    ) -> Result<Vec<TodoItem>, ToolError> {
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
        Ok(agents
            .get(agent)
            .map_or_else(Vec::new, |state| state.items.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejections_identify_the_rejected_job() {
        let fixture = crate::session::tests::MemorySession::new().await;
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
