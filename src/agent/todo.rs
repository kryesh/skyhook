//! Durable per-agent advisory checklists.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
    Prose,
    identity::{AgentId, JobId},
    named_enum::named_enum,
    session::{
        CompactionCheckpoint, EventRecord, MessageSeq, RecordSeq, SessionError, SessionEvent,
        SessionStore,
    },
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
    /// The latest checkpoint's frontier: its summary reconciled every response up to it.
    reconciled: RecordSeq,
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
                SessionEvent::Compaction { checkpoint } => {
                    agents.entry(record.agent.clone()).or_default().reconciled =
                        checkpoint.frontier;
                    &checkpoint.todos
                }
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

    /// Replace `agent`'s list on behalf of the response `issued_by`, if a model call
    /// issued it. A checkpoint that reconciled that response supersedes it.
    pub async fn replace(
        &self,
        agent: &AgentId,
        items: Vec<TodoItem>,
        issued_by: Option<MessageSeq>,
    ) -> Result<(), ReplaceError> {
        // Hold the lock across persistence so published snapshots and replay agree on order.
        let mut agents = self.agents.lock().await;
        let state = agents.entry(agent.clone()).or_default();
        if issued_by.is_some_and(|message| RecordSeq::from(message) <= state.reconciled) {
            return Err(ReplaceError::Superseded);
        }
        let replaced = SessionEvent::TodosReplaced {
            items: items.clone(),
        };
        self.store.append(agent.clone(), replaced).await?;
        state.items = items;
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
        let (items, reconciled) = (checkpoint.todos.clone(), checkpoint.frontier);
        self.store
            .append(agent.clone(), SessionEvent::Compaction { checkpoint })
            .await?;
        let state = agents.entry(agent.clone()).or_default();
        (state.items, state.reconciled) = (items, reconciled);
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

    /// A restored store still knows which responses its latest checkpoint reconciled.
    #[tokio::test]
    async fn replacements_from_reconciled_responses_are_superseded() {
        let fixture = crate::session::tests::MemorySession::new().await;
        let checkpoint = CompactionCheckpoint {
            frontier: RecordSeq::from(5),
            message: crate::session::Message::User(Vec::new()),
            todos: Vec::new(),
            retained: Vec::new(),
            attempt: crate::session::AttemptRef {
                request: RecordSeq::from(4).request(),
                attempt: 1,
            },
            before_tokens: 2,
            after_tokens: 1,
        };
        let compaction = SessionEvent::Compaction { checkpoint };
        let records = [crate::session::tests::record(&fixture.agent, 6, compaction)];
        let todos = TodoStore::restore(fixture.store, &records);
        let replace = |issued_by: Option<u64>| {
            let items = vec![TodoItem {
                text: "Next".parse().unwrap(),
                status: TodoStatus::Pending,
            }];
            todos.replace(&fixture.agent, items, issued_by.map(MessageSeq::from))
        };
        assert!(matches!(
            replace(Some(5)).await,
            Err(ReplaceError::Superseded)
        ));
        replace(Some(6)).await.unwrap();
        replace(None).await.unwrap();
    }

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
