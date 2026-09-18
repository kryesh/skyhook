//! Durable per-agent advisory checklists.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
    identity::{AgentId, JobId},
    session::{CompactionCheckpoint, EventRecord, SessionError, SessionEvent, SessionStore},
    tool::ToolError,
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
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
    revision: u64,
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
        if items.iter().any(|item| item.text.trim().is_empty()) {
            return Err(ToolError::InvalidArguments(
                "todo text cannot be blank".to_owned(),
            ));
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
                    ToolError::InvalidArguments(
                        "job does not identify an initialized child agent".to_owned(),
                    )
                })?;
            if agent.session() != caller.session()
                || agent.depth() <= caller.depth()
                || !agent.path().starts_with(caller.path())
            {
                return Err(ToolError::InvalidArguments(
                    "todo inspection is limited to descendants".to_owned(),
                ));
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

    pub async fn snapshots(&self) -> Vec<TodoSnapshot> {
        self.agents
            .lock()
            .await
            .iter()
            .map(|(agent, state)| TodoSnapshot {
                agent: agent.clone(),
                items: state.items.clone(),
            })
            .collect()
    }
}
