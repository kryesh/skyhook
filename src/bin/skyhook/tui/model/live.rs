//! Live response cards, reasoning expansion, and stable native block identities.

use super::{AgentDisplayState, Entry, EntryKey, Projection, Surface, View};
use skyhook::agent::{AgentActivity, ObservationSnapshot, ObservedResponse};
use skyhook::identity::AgentId;
use skyhook::provider::protocol::ItemKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReasoningStatus {
    Running,
    Complete,
    Incomplete,
}

pub(super) fn working_entry(
    snapshot: &ObservationSnapshot,
    projection: &Projection,
    agent: &AgentId,
    running: bool,
) -> Option<Entry> {
    if running {
        return None;
    }
    // Attempt-aware requests carry status in their original journal position,
    // including the short transition between failure and scheduled recovery.
    let active = projection.active_request.get(agent);
    if active.is_some_and(|request| projection.retry_failed(*request)) {
        return None;
    }
    let state = match snapshot.activity.get(agent) {
        Some(AgentActivity::Working)
            if !projection
                .active_request
                .get(agent)
                .is_some_and(|request| projection.response_committed(*request)) =>
        {
            AgentDisplayState::Working
        }
        Some(AgentActivity::Reconnecting { attempt }) => {
            AgentDisplayState::Reconnecting { attempt: *attempt }
        }
        Some(AgentActivity::Compacting) => AgentDisplayState::Compacting,
        _ => return None,
    };

    let mut entry = Entry::new(
        EntryKey::Working(agent.clone()),
        format!("  {}", state.label()),
        Surface::Muted,
    );
    entry.running = true;
    Some(entry)
}

pub(super) fn reasoning_entry(
    key: EntryKey,
    text: &str,
    view: &View,
    status: ReasoningStatus,
) -> Entry {
    let title = match status {
        ReasoningStatus::Running => "  Reasoning",
        ReasoningStatus::Complete => "Reasoning",
        ReasoningStatus::Incomplete => "Reasoning · incomplete",
    };
    let running = status == ReasoningStatus::Running;
    let default_open = running;
    // Source lines determine collapsibility; terminal wrapping must not change interaction.
    let text = text.trim_matches(['\r', '\n']);
    if text.lines().count() <= 1 {
        let mut entry = Entry::new(key, text.to_owned(), Surface::Reasoning);
        entry.default_open = default_open;
        entry.running = running;
        return entry;
    }
    let open = view.is_expanded(&key, default_open);
    let mut entry = Entry::expandable_text(
        key,
        if open {
            format!("▾ {title}\n{text}")
        } else {
            format!("▸ {title}")
        },
        Surface::Reasoning,
    );
    entry.default_open = default_open;
    entry.running = running;
    entry
}

/// Native block identity remains stable from live response through journal commit.
pub(super) fn response_block_key(request: u64, item: &str, block: &str) -> EntryKey {
    EntryKey::ResponseBlock {
        request,
        item: item.to_owned(),
        block: block.to_owned(),
    }
}

pub(super) fn reasoning_key(request: u64, item: &str, block: &str) -> EntryKey {
    EntryKey::ReasoningBlock {
        request,
        item: item.to_owned(),
        block: block.to_owned(),
    }
}

/// Complete live-tail eligibility shared by fresh, reset, and dirty-tail paths.
/// This deliberately does not broaden Projection::live_response's narrower API.
pub(super) fn live_tail_response<'a>(
    snapshot: &'a ObservationSnapshot,
    projection: &Projection,
    agent: &AgentId,
    request: u64,
) -> Option<&'a ObservedResponse> {
    snapshot
        .responses
        .get(&(agent.clone(), request))
        .filter(|response| {
            projection.live_response(request, response) && !projection.retry_failed(request)
        })
}

pub(super) fn live_tail_responses<'a>(
    snapshot: &'a ObservationSnapshot,
    projection: &Projection,
    agent: &AgentId,
) -> Vec<(u64, &'a ObservedResponse)> {
    let mut responses: Vec<_> = snapshot
        .responses
        .keys()
        .filter(|(owner, _)| owner == agent)
        .filter_map(|(_, request)| {
            live_tail_response(snapshot, projection, agent, *request)
                .map(|response| (*request, response))
        })
        .collect();
    responses.sort_by_key(|(request, _)| *request);
    responses
}

pub(super) fn response_entries(
    request: u64,
    response: &ObservedResponse,
    view: &View,
    agent_name: &str,
) -> Vec<Entry> {
    let mut entries = Vec::new();
    for block in response.blocks() {
        let (item, id) = (block.block.item.as_str(), block.block.block.as_str());
        match block.kind {
            ItemKind::Reasoning if !block.text.trim().is_empty() => {
                // Reasoning keeps streaming until a later block takes over or the
                // response ends.
                let entry = reasoning_entry(
                    reasoning_key(request, item, id),
                    &block.text,
                    view,
                    if response.streaming(block) {
                        ReasoningStatus::Running
                    } else if response.error.is_some() {
                        ReasoningStatus::Incomplete
                    } else {
                        ReasoningStatus::Complete
                    },
                );
                entries.push(entry);
            }
            ItemKind::Text if !block.text.trim().is_empty() => {
                entries.push(Entry::new(
                    response_block_key(request, item, id),
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
    entries
}

#[cfg(test)]
mod tests {
    use super::super::tests::{response as apply, root};
    use super::*;
    use skyhook::provider::protocol::{AssistantItem, Completion, ResponseEvent};

    fn response(text: &str) -> ObservedResponse {
        let agent = root(1);
        let mut snapshot = ObservationSnapshot::default();
        let ended = Completion::answer(vec![AssistantItem::text("text", 0, text)]).unwrap();
        apply(&mut snapshot, &agent, 4, ResponseEvent::End(ended));
        snapshot.responses.remove(&(agent, 4)).unwrap()
    }

    #[test]
    fn reasoning_expansion_respects_defaults_and_explicit_overrides() {
        use ReasoningStatus::{Complete, Running};
        let mut view = View::default();
        let key = reasoning_key(4, "item", "block");
        let entry =
            |view: &View, text: &str, status| reasoning_entry(key.clone(), text, view, status);
        let active = entry(&view, "\nFirst\nSecond\r\n", Running);
        assert_eq!(active.text(), "▾   Reasoning\nFirst\nSecond");
        assert!(active.expandable() && active.default_open);
        view.set_expanded(key.clone(), false);
        assert_eq!(
            entry(&view, "First\nSecond", Running).text(),
            "▸   Reasoning"
        );
        view.clear_collapsed();
        assert_eq!(
            entry(&view, "First\nSecond", Complete).text(),
            "▸ Reasoning"
        );
        view.set_expanded(key.clone(), true);
        assert!(
            entry(&view, "First\nSecond", Complete)
                .text()
                .contains("Second")
        );
        assert!(!entry(&view, "single line", Complete).expandable());
    }

    #[test]
    fn text_visibility_preserves_whitespace_and_failed_response_attribution() {
        let rows = |live: &ObservedResponse| response_entries(4, live, &View::default(), "Agent");
        for text in ["", "\n\n", "  "] {
            assert!(rows(&response(text)).is_empty());
        }
        let mut live = response("\n\n  Actual answer.\n");
        for (error, label, surface) in [
            (None, "Agent", Surface::Agent),
            (Some("disconnected"), "Incomplete response", Surface::Error),
        ] {
            live.error = error.map(Into::into);
            let rows = rows(&live);
            assert_eq!(rows[0].text(), format!("{label}\n\n\n  Actual answer.\n"));
            assert_eq!(rows[0].surface, surface);
        }
    }
}
