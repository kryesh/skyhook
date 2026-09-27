//! Live response cards, reasoning expansion, and stable native block identities.

use super::{AgentDisplayState, Entry, EntryKey, ResponseRef, Surface, Title, View};
use skyhook::agent::{AgentActivity, ObservationSnapshot, ObservedResponse};
use skyhook::identity::AgentId;
use skyhook::provider::protocol::{BlockRef, ItemKind};
use skyhook::session::{RequestPhase, RequestSeq};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReasoningStatus {
    Running,
    Complete,
    Incomplete,
}

pub(super) fn working_entry(
    snapshot: &ObservationSnapshot,
    agent: &AgentId,
    running: bool,
) -> Option<Entry> {
    if running {
        return None;
    }
    // A request's own status card carries failure and recovery at its journal
    // position, including the short transition between the two.
    let latest = snapshot
        .ledger
        .latest(agent)
        .and_then(|request| snapshot.ledger.get(request))
        .map(|record| &record.phase);
    if latest.is_some_and(RequestPhase::has_status_card) {
        return None;
    }
    let state = match snapshot.activity.get(agent) {
        // A committed answer needs no indicator under it while activity catches up.
        Some(AgentActivity::Working)
            if !matches!(
                latest,
                Some(
                    RequestPhase::Completed { .. }
                        | RequestPhase::Open {
                            message: Some(_),
                            ..
                        }
                )
            ) =>
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
        state.label(),
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
    let label = match status {
        ReasoningStatus::Running | ReasoningStatus::Complete => "Reasoning",
        ReasoningStatus::Incomplete => "Reasoning · incomplete",
    };
    let running = status == ReasoningStatus::Running;
    // Source lines determine collapsibility; terminal wrapping must not change interaction.
    let text = text.trim_matches(['\r', '\n']);
    if text.lines().count() <= 1 {
        let mut entry = Entry::new(key, text.to_owned(), Surface::Reasoning);
        entry.running = running;
        return entry;
    }
    let open = view.is_expanded(&key, running);
    let body = if open { text.to_owned() } else { String::new() };
    let mut entry = Entry::titled(key, Title::disclosed(label, open), body, Surface::Reasoning);
    entry.running = running;
    entry
}

/// Native block identity remains stable from live response through journal commit.
pub(super) fn block_key(response: ResponseRef, block: &BlockRef) -> EntryKey {
    EntryKey::Block {
        response,
        block: block.clone(),
    }
}

/// A response streams at the tail until its commit is observed; failures and
/// interruptions move it into the request's status card at its journal position.
pub(super) fn live_tail_response<'a>(
    snapshot: &'a ObservationSnapshot,
    agent: &AgentId,
    request: RequestSeq,
) -> Option<&'a ObservedResponse> {
    let live = (snapshot.ledger.get(request)).is_some_and(|record| record.phase.is_live_tail());
    live.then(|| snapshot.responses.get(&(agent.clone(), request)))
        .flatten()
}

pub(super) fn live_tail_responses<'a>(
    snapshot: &'a ObservationSnapshot,
    agent: &AgentId,
) -> Vec<(RequestSeq, &'a ObservedResponse)> {
    let mut responses: Vec<_> = snapshot
        .responses
        .keys()
        .filter(|(owner, _)| owner == agent)
        .filter_map(|(_, request)| {
            let request = *request;
            live_tail_response(snapshot, agent, request).map(|response| (request, response))
        })
        .collect();
    responses.sort_by_key(|(request, _)| *request);
    responses
}

pub(super) fn response_entries(
    request: RequestSeq,
    response: &ObservedResponse,
    view: &View,
    agent_name: &str,
) -> Vec<Entry> {
    let mut entries = Vec::new();
    for block in response.blocks() {
        let key = block_key(ResponseRef::Request(request), &block.block);
        match block.kind {
            ItemKind::Reasoning if !block.text.trim().is_empty() => {
                // Reasoning keeps streaming until a later block takes over or the
                // response ends.
                let entry = reasoning_entry(
                    key,
                    &block.text,
                    view,
                    if response.streaming(block) {
                        ReasoningStatus::Running
                    } else if response.incomplete() {
                        ReasoningStatus::Incomplete
                    } else {
                        ReasoningStatus::Complete
                    },
                );
                entries.push(entry);
            }
            ItemKind::Text if !block.text.trim().is_empty() => {
                let (title, surface) = if response.incomplete() {
                    ("Incomplete response", Surface::Error)
                } else {
                    (agent_name, Surface::Agent)
                };
                entries.push(Entry::titled(
                    key,
                    Title::plain(title),
                    block.text.clone(),
                    surface,
                ));
            }
            _ => {}
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::super::tests::{root, update};
    use super::*;
    use skyhook::agent::{Failure, RuntimeEvent, Settlement, TurnFailure};
    use skyhook::provider::protocol::{AssistantItem, BlockId, Completion, ItemId, ResponseEvent};
    use skyhook::session::{MessageSeq, RequestSeq};

    /// A settled answer of `text`, observed for a request that is never journaled.
    fn response(text: &str) -> ObservedResponse {
        let agent = root(1);
        let mut snapshot = ObservationSnapshot::default();
        let ended = Completion::answer(vec![AssistantItem::text("text", 0, text)]).unwrap();
        let event = RuntimeEvent::ResponseEvent {
            agent: agent.clone(),
            request: RequestSeq::default(),
            event: ResponseEvent::End(ended),
        };
        update(&mut snapshot, event);
        snapshot
            .responses
            .remove(&(agent, RequestSeq::default()))
            .unwrap()
    }

    #[test]
    fn reasoning_expansion_respects_defaults_and_explicit_overrides() {
        use ReasoningStatus::{Complete, Running};
        let mut view = View::default();
        let block = BlockRef {
            item: ItemId::try_from("item".to_owned()).unwrap(),
            block: BlockId::try_from("block".to_owned()).unwrap(),
        };
        let key = block_key(ResponseRef::Request(RequestSeq::default()), &block);
        let entry =
            |view: &View, text: &str, status| reasoning_entry(key.clone(), text, view, status);
        let active = entry(&view, "\nFirst\nSecond\r\n", Running);
        assert_eq!(active.title(), Some(&Title::disclosed("Reasoning", true)));
        assert_eq!(active.body(), "First\nSecond");
        assert_eq!(active.text(), "▾ Reasoning\nFirst\nSecond");
        assert!(active.running);
        view.set_expanded(key.clone(), false);
        let collapsed = entry(&view, "First\nSecond", Running);
        assert_eq!(
            collapsed.title(),
            Some(&Title::disclosed("Reasoning", false))
        );
        assert_eq!((collapsed.body(), collapsed.text()), ("", "▸ Reasoning"));
        view.clear_collapsed();
        let complete = entry(&view, "First\nSecond", Complete);
        assert_eq!(
            complete.title(),
            Some(&Title::disclosed("Reasoning", false))
        );
        assert!(!complete.running);
        view.set_expanded(key.clone(), true);
        assert_eq!(
            entry(&view, "First\nSecond", Complete).body(),
            "First\nSecond"
        );
        let single = entry(&view, "single line", Complete);
        assert!(!single.expandable() && single.title().is_none());
        assert_eq!(single.text(), "single line");
    }

    #[test]
    fn text_visibility_preserves_whitespace_and_failed_response_attribution() {
        let rows = |live: &ObservedResponse| {
            response_entries(RequestSeq::default(), live, &View::default(), "Agent")
        };
        for text in ["", "\n\n", "  "] {
            assert!(rows(&response(text)).is_empty());
        }
        let blocks = response("\n\n  Actual answer.\n").blocks().to_vec();
        let disconnected = TurnFailure::from(Failure::Other("disconnected".into()));
        for (how, label, surface) in [
            (
                Settlement::Committed(MessageSeq::default()),
                "Agent",
                Surface::Agent,
            ),
            (
                Settlement::Aborted(MessageSeq::default()),
                "Incomplete response",
                Surface::Error,
            ),
            (
                Settlement::Failed(disconnected),
                "Incomplete response",
                Surface::Error,
            ),
        ] {
            let live = ObservedResponse::Settled {
                blocks: blocks.clone(),
                how,
            };
            let rows = rows(&live);
            assert_eq!(rows[0].title(), Some(&Title::plain(label)));
            assert_eq!(rows[0].body(), "\n\n  Actual answer.\n");
            assert_eq!(rows[0].surface, surface);
        }
    }
}
