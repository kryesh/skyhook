//! Durable per-agent advisory checklists.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::identity::AgentId;
use crate::session::{
    Command, EventRecord, MessageSeq, Plan, RecordSeq, SessionError, SessionEvent, SessionStore,
};
use skyhook_base::{enums::NamedEnum, newtype::Prose};

#[derive(Clone, Copy, Debug, Serialize, JsonSchema, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TodoItem {
    /// Instruction or step to carry out
    pub text: Prose,
    pub status: TodoStatus,
}

/// One agent's list as the core folds it.
#[derive(Default)]
pub(crate) struct AgentTodos {
    items: Vec<TodoItem>,
    /// The latest checkpoint's frontier: its summary reconciled every response up to it.
    reconciled: RecordSeq,
    /// The record that last set the list.
    set: RecordSeq,
}

impl AgentTodos {
    /// The empty list of an agent whose latest checkpoint reconciled every response up
    /// to `reconciled`, to fold the record that last set it into.
    pub(crate) fn opened(reconciled: RecordSeq) -> Self {
        Self {
            reconciled,
            ..Self::default()
        }
    }
}

/// Fold one committed record into every agent's list.
pub(crate) fn apply(agents: &mut BTreeMap<AgentId, AgentTodos>, record: &EventRecord) {
    let agent = record.agent.clone();
    match &record.event {
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

/// A new list, or new statuses for the current one's items in order.
pub(crate) enum TodoUpdate {
    Items(Vec<TodoItem>),
    Statuses(Vec<TodoStatus>),
}

impl From<Vec<TodoItem>> for TodoUpdate {
    fn from(items: Vec<TodoItem>) -> Self {
        Self::Items(items)
    }
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
    #[error(
        "status gives {given} statuses for a list of {listed} items: give one per item, in order"
    )]
    Statuses { given: usize, listed: usize },
    #[error(transparent)]
    Session(#[from] SessionError),
}

impl SessionStore {
    /// Replace `agent`'s list on behalf of the response `issued_by`, if a model call
    /// issued it. A checkpoint that reconciled that response supersedes it, so new
    /// statuses always apply to the list the call's issuer saw.
    pub(crate) async fn replace_todos(
        &self,
        agent: &AgentId,
        update: impl Into<TodoUpdate>,
        issued_by: Option<MessageSeq>,
    ) -> Result<(), ReplaceError> {
        let (agent, update) = (agent.clone(), update.into());
        self.commit(Command::Journal, move |state| {
            let current = state.todos.get(&agent);
            let reconciled = current.map(|state| state.reconciled);
            let reconciled = reconciled.unwrap_or_default();
            if issued_by.is_some_and(|message| RecordSeq::from(message) <= reconciled) {
                return Err(ReplaceError::Superseded);
            }
            let items = match update {
                TodoUpdate::Items(items) => items,
                TodoUpdate::Statuses(statuses) => {
                    let listed = current.map_or(&[][..], |state| &state.items);
                    if statuses.len() != listed.len() {
                        return Err(ReplaceError::Statuses {
                            given: statuses.len(),
                            listed: listed.len(),
                        });
                    }
                    let restatus = |(item, status): (&TodoItem, TodoStatus)| TodoItem {
                        text: item.text.clone(),
                        status,
                    };
                    listed.iter().zip(statuses).map(restatus).collect()
                }
            };
            let replaced = SessionEvent::TodosReplaced { items };
            Ok(Plan::journal(vec![(agent, replaced)]))
        })
        .await
    }

    /// `agent`'s list.
    pub(crate) fn inspect_todos(&self, agent: &AgentId) -> Vec<TodoItem> {
        self.read(|state| {
            (state.todos.get(agent)).map_or_else(Vec::new, |state| state.items.clone())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn statuses_restatus_the_list_in_order_and_must_cover_it() {
        let fixture = crate::tests::TestRuntime::new().await;
        let (store, agent) = (&fixture.store, &fixture.agent);
        let item = |text: &str, status| TodoItem {
            text: text.parse().unwrap(),
            status,
        };
        let listed = vec![
            item("Inspect", TodoStatus::InProgress),
            item("Change", TodoStatus::Pending),
        ];
        store
            .replace_todos(agent, listed.clone(), None)
            .await
            .unwrap();
        let short = TodoUpdate::Statuses(vec![TodoStatus::Completed]);
        let short = store.replace_todos(agent, short, None).await.unwrap_err();
        assert!(matches!(
            short,
            ReplaceError::Statuses {
                given: 1,
                listed: 2
            }
        ));
        assert_eq!(store.inspect_todos(agent), listed);
        let statuses = TodoUpdate::Statuses(vec![TodoStatus::Completed, TodoStatus::InProgress]);
        store.replace_todos(agent, statuses, None).await.unwrap();
        assert_eq!(
            store.inspect_todos(agent),
            [
                item("Inspect", TodoStatus::Completed),
                item("Change", TodoStatus::InProgress)
            ]
        );
    }
}
