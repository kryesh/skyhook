//! Live response cards, reasoning expansion, and stable native block identities.

use super::{Entry, Projection, Surface, View};
use skyhook::agent::{AgentActivity, LiveResponse, ObservationSnapshot};
use skyhook::identity::AgentId;
use skyhook::provider::protocol::BlockKind;

pub(super) fn working_label(
    snapshot: &ObservationSnapshot,
    projection: &Projection,
    agent: &AgentId,
    running: bool,
) -> Option<String> {
    if running {
        return None;
    }
    // Attempt-aware requests carry status in their original journal position,
    // including the short transition between failure and scheduled recovery.
    if projection.active_request.get(agent).is_some_and(|request| {
        projection.requests.get(request).is_some_and(|info| {
            info.retry
                .as_ref()
                .is_some_and(super::retry::RetryState::has_error)
        })
    }) {
        return None;
    }
    let label = match snapshot.activity.get(agent) {
        Some(AgentActivity::Working)
            if !projection
                .active_request
                .get(agent)
                .is_some_and(|request| projection.response_committed(*request)) =>
        {
            "Working".into()
        }
        Some(AgentActivity::Reconnecting {
            attempt,
            max_attempts,
        }) => match max_attempts {
            Some(max) => format!("Reconnecting · attempt {attempt} of {max}"),
            None => format!("Retrying · attempt {attempt}"),
        },
        Some(AgentActivity::Compacting) => "Compacting".into(),
        _ => return None,
    };
    Some(label)
}

pub(super) fn working_entry(agent: &AgentId, label: &str) -> Entry {
    let mut entry = Entry::new(
        format!("working-{agent}"),
        format!("  {label}"),
        Surface::Muted,
    );
    entry.running = true;
    entry
}

pub(super) fn reasoning_entry(
    key: String,
    text: &str,
    view: &View,
    default_open: bool,
    title: &str,
) -> Entry {
    // Source lines determine collapsibility; terminal wrapping must not change interaction.
    let text = text.trim_matches(['\r', '\n']);
    if text.lines().count() <= 1 {
        let mut entry = Entry::new(key, text.to_owned(), Surface::Reasoning);
        entry.default_open = default_open;
        return entry;
    }
    let open = view.is_expanded(&key, default_open);
    let mut entry = Entry::new(
        key,
        if open {
            format!("▾ {title}\n{text}")
        } else {
            format!("▸ {title}")
        },
        Surface::Reasoning,
    );
    entry.expandable = true;
    entry.default_open = default_open;
    entry
}

/// Length-prefixed native IDs avoid collisions even when IDs contain separators.
pub(super) fn response_block_key(request: u64, item: &str, block: &str) -> String {
    format!(
        "response{request}/{}:{item}/{}:{block}",
        item.len(),
        block.len()
    )
}

pub(super) fn reasoning_key(request: u64, item: &str, block: &str) -> String {
    format!("reasoning-{}", response_block_key(request, item, block))
}

pub(super) fn response_entries(
    request: u64,
    response: &LiveResponse,
    view: &View,
    thinking: bool,
    agent_name: &str,
) -> Vec<Entry> {
    let mut entries = Vec::new();
    for item in response.snapshot().items {
        for block in item.blocks {
            match block.kind {
                BlockKind::Reasoning if !block.text.trim().is_empty() => {
                    // A block ends independently of its item and of answer text.
                    let running = !response.settled && !block.ended;
                    let mut entry = reasoning_entry(
                        reasoning_key(request, &item.id, &block.id),
                        &block.text,
                        view,
                        running || thinking,
                        if running {
                            "  Reasoning"
                        } else if response.error.is_some() {
                            "Reasoning · incomplete"
                        } else {
                            "Reasoning"
                        },
                    );
                    entry.running = running;
                    entry.default_open = running || thinking;
                    entries.push(entry);
                }
                BlockKind::Text if !block.text.trim().is_empty() => {
                    entries.push(Entry::new(
                        response_block_key(request, &item.id, &block.id),
                        format!(
                            "{}\n{}",
                            if response.error.is_some() {
                                "Incomplete response"
                            } else {
                                agent_name
                            },
                            block.text,
                        ),
                        if response.error.is_some() {
                            Surface::Error
                        } else {
                            Surface::Agent
                        },
                    ));
                }
                _ => {}
            }
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use skyhook::{
        agent::{ObservedEvent, RuntimeEvent},
        identity::SessionId,
        provider::protocol::{BlockContent, ItemKind, ResponseEvent},
    };

    fn response(text: &str) -> LiveResponse {
        let agent = AgentId::root(SessionId::from_bytes([1; 16]));
        let mut snapshot = ObservationSnapshot::default();
        for event in [
            ResponseEvent::ItemStarted {
                id: "text".into(),
                position: 0,
                kind: ItemKind::Text,
            },
            ResponseEvent::BlockStarted {
                item: "text".into(),
                id: "block".into(),
                position: 0,
                kind: BlockKind::Text,
            },
            ResponseEvent::BlockEnded {
                item: "text".into(),
                block: "block".into(),
                content: BlockContent::Text { text: text.into() },
            },
        ] {
            snapshot.apply(ObservedEvent {
                revision: snapshot.revision + 1,
                event: RuntimeEvent::ResponseEvent {
                    agent: agent.clone(),
                    request: 4,
                    event,
                },
            });
        }
        snapshot.responses.remove(&(agent, 4)).unwrap()
    }

    #[test]
    fn reasoning_expansion_respects_defaults_and_explicit_overrides() {
        let mut view = View::default();
        let key = reasoning_key(4, "item", "block");
        let active = reasoning_entry(
            key.clone(),
            "\nFirst\nSecond\r\n",
            &view,
            true,
            "  Reasoning",
        );
        assert_eq!(active.text, "▾   Reasoning\nFirst\nSecond");
        assert!(active.expandable && active.default_open);
        view.collapsed.insert(key.clone());
        assert_eq!(
            reasoning_entry(key.clone(), "First\nSecond", &view, true, "  Reasoning").text,
            "▸   Reasoning"
        );
        view.collapsed.clear();
        assert_eq!(
            reasoning_entry(key.clone(), "First\nSecond", &view, false, "Reasoning").text,
            "▸ Reasoning"
        );
        view.expanded.insert(key.clone());
        assert!(
            reasoning_entry(key.clone(), "First\nSecond", &view, false, "Reasoning")
                .text
                .contains("Second")
        );
        assert!(!reasoning_entry(key, "single line", &view, false, "Reasoning").expandable);
    }

    #[test]
    fn text_visibility_preserves_whitespace_and_failed_response_attribution() {
        for text in ["", "\n\n", "  "] {
            let live = response(text);
            assert!(response_entries(4, &live, &View::default(), false, "Agent").is_empty());
        }
        let mut live = response("\n\n  Actual answer.\n");
        let rows = response_entries(4, &live, &View::default(), false, "Agent");
        assert_eq!(rows[0].text, "Agent\n\n\n  Actual answer.\n");
        assert_eq!(rows[0].surface, Surface::Agent);
        live.error = Some("disconnected".into());
        let rows = response_entries(4, &live, &View::default(), false, "Agent");
        assert_eq!(rows[0].text, "Incomplete response\n\n\n  Actual answer.\n");
        assert_eq!(rows[0].surface, Surface::Error);
    }

    #[test]
    fn native_block_keys_cannot_collide_at_id_separators() {
        assert_ne!(
            response_block_key(4, "a/b", "c"),
            response_block_key(4, "a", "b/c")
        );
    }
}
