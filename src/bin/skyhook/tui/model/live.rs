//! Live response cards, reasoning expansion, and stable native block identities.

use super::{
    AgentDisplayState, Entry, EntryKey, EntryView, Projection, ResponseRef, Surface, Timing, Title,
    View,
};
use crate::tui::render::{Section, sections};
use skyhook::agent::{AgentActivity, ObservationSnapshot, ObservedResponse};
use skyhook::identity::AgentId;
use skyhook::provider::protocol::{BlockRef, ItemKind};
use skyhook::session::{RequestPhase, RequestRecord, RequestSeq};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReasoningStatus {
    /// Streaming, counting from when its request began.
    Running {
        since: i64,
    },
    Complete,
    Incomplete,
}

fn working_entry(
    snapshot: &ObservationSnapshot,
    agent: &AgentId,
    streaming: bool,
) -> Option<Entry> {
    if streaming {
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
    let activity = snapshot.activity.get(agent)?;
    let state = match &activity.state {
        // A committed answer needs no indicator under it while activity catches up.
        AgentActivity::Working
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
        AgentActivity::Reconnecting { attempt } => {
            AgentDisplayState::Reconnecting { attempt: *attempt }
        }
        AgentActivity::Compacting => AgentDisplayState::Compacting,
        _ => return None,
    };
    let mut entry = Entry::new(
        EntryKey::Working(agent.clone()),
        state.label(),
        Surface::Muted,
    );
    entry.timing = Timing::Since(activity.since);
    Some(entry)
}

/// One entry per section of a reasoning block. Earlier sections are settled;
/// only the last carries the block's status.
pub(super) fn reasoning_entries(
    response: ResponseRef,
    block: &BlockRef,
    text: &str,
    view: &View,
    status: ReasoningStatus,
) -> Vec<Entry> {
    let sections = sections(text);
    let last = sections.len().saturating_sub(1);
    let entries = sections.into_iter().enumerate().map(|(index, section)| {
        let key = EntryKey::Reasoning {
            response,
            block: block.clone(),
            section: index,
        };
        let status = if index == last {
            status
        } else {
            ReasoningStatus::Complete
        };
        reasoning_entry(key, section, view, status)
    });
    entries.collect()
}

/// A titled section is collapsible under its title unless it has no body.
fn reasoning_entry(
    key: EntryKey,
    Section { title, body }: Section<'_>,
    view: &View,
    status: ReasoningStatus,
) -> Entry {
    let timing = match status {
        ReasoningStatus::Running { since } => Timing::Since(since),
        ReasoningStatus::Complete | ReasoningStatus::Incomplete => Timing::Untimed,
    };
    // Source lines determine collapsibility; terminal wrapping must not change interaction.
    let inline = match &title {
        Some(title) => body.text.is_empty().then_some(title),
        None => (body.text.lines().count() <= 1).then_some(&body),
    };
    let mut entry = if let Some(source) = inline {
        Entry::new(key, source.markdown(), Surface::Reasoning)
    } else {
        let open = view.is_expanded(&key, timing.live());
        let mut title = match title {
            Some(title) => Title::markdown(title.text, title.definitions, open),
            None => Title::disclosed("Reasoning", open),
        };
        if status == ReasoningStatus::Incomplete {
            title.suffix = Some(" · incomplete");
        }
        let body = if open { body.markdown() } else { String::new() };
        Entry::titled(key, title, body, Surface::Reasoning)
    };
    entry.timing = timing;
    entry
}

/// Native block identity remains stable from live response through journal commit.
pub(super) fn block_key(response: ResponseRef, block: &BlockRef) -> EntryKey {
    EntryKey::Block {
        response,
        block: block.clone(),
    }
}

/// The live tail after history: the responses of `requests` still streaming there,
/// then the working indicator unless reasoning streams. Returns the requests shown.
pub(super) fn live_tail(
    snapshot: &ObservationSnapshot,
    projection: &Projection,
    presentation: EntryView<'_>,
    requests: impl IntoIterator<Item = RequestSeq>,
    entries: &mut Vec<Entry>,
) -> Vec<RequestSeq> {
    let EntryView { agent, view, .. } = presentation;
    let agent_name = projection.agent_name(agent);
    let start = entries.len();
    let mut shown = Vec::new();
    for request in requests {
        if let Some((record, response)) = live_tail_response(snapshot, agent, request) {
            shown.push(request);
            let since = record.requested_millis;
            entries.extend(response_entries(request, response, view, agent_name, since));
        }
    }
    let streaming = entries[start..].iter().any(|entry| entry.timing.live());
    entries.extend(working_entry(snapshot, agent, streaming));
    shown
}

/// A response streams at the tail until its commit is observed; failures and
/// interruptions move it into the request's status card at its journal position.
fn live_tail_response<'a>(
    snapshot: &'a ObservationSnapshot,
    agent: &AgentId,
    request: RequestSeq,
) -> Option<(&'a RequestRecord, &'a ObservedResponse)> {
    let record = (snapshot.ledger.get(request)).filter(|record| record.phase.is_live_tail())?;
    Some((record, snapshot.responses.get(&(agent.clone(), request))?))
}

pub(super) fn live_tail_responses(
    snapshot: &ObservationSnapshot,
    agent: &AgentId,
) -> Vec<RequestSeq> {
    let mut requests: Vec<_> = snapshot
        .responses
        .keys()
        .filter(|(owner, request)| {
            owner == agent && live_tail_response(snapshot, agent, *request).is_some()
        })
        .map(|(_, request)| *request)
        .collect();
    requests.sort_unstable();
    requests
}

pub(super) fn response_entries(
    request: RequestSeq,
    response: &ObservedResponse,
    view: &View,
    agent_name: &str,
    since: i64,
) -> Vec<Entry> {
    let mut entries = Vec::new();
    let response_ref = ResponseRef::Request(request);
    for block in response.blocks() {
        match block.kind {
            ItemKind::Reasoning => {
                // Reasoning keeps streaming until a later block takes over or the
                // response ends.
                entries.extend(reasoning_entries(
                    response_ref,
                    &block.block,
                    &block.text,
                    view,
                    if response.streaming(block) {
                        ReasoningStatus::Running { since }
                    } else if response.incomplete() {
                        ReasoningStatus::Incomplete
                    } else {
                        ReasoningStatus::Complete
                    },
                ));
            }
            ItemKind::Text if !block.text.trim().is_empty() => {
                let (title, surface) = if response.incomplete() {
                    ("Incomplete response", Surface::Error)
                } else {
                    (agent_name, Surface::Agent)
                };
                entries.push(Entry::titled(
                    block_key(response_ref, &block.block),
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

    fn block() -> BlockRef {
        BlockRef {
            item: ItemId::try_from("item".to_owned()).unwrap(),
            block: BlockId::try_from("block".to_owned()).unwrap(),
        }
    }

    #[test]
    fn reasoning_expansion_respects_defaults_and_explicit_overrides() {
        use ReasoningStatus::Complete;
        let mut view = View::default();
        let key = EntryKey::Reasoning {
            response: ResponseRef::Request(RequestSeq::default()),
            block: block(),
            section: 0,
        };
        let entry = |view: &View, text, status| {
            let section = sections(text).pop().unwrap();
            reasoning_entry(key.clone(), section, view, status)
        };
        // Streaming reasoning counts from its request.
        let running = ReasoningStatus::Running { since: 1_000 };
        let active = entry(&view, "\nFirst\nSecond\r\n", running);
        assert_eq!(active.title(), Some(&Title::disclosed("Reasoning", true)));
        assert_eq!(active.body(), "First\nSecond");
        assert_eq!(active.text(), "▾ Reasoning\nFirst\nSecond");
        assert_eq!(active.timing, Timing::Since(1_000));
        view.set_expanded(key.clone(), false);
        let collapsed = entry(&view, "First\nSecond", running);
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
        assert!(!complete.timing.live());
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
    fn reasoning_titles_disclose_the_sections_they_open() {
        let text = "Intro\n\n**First**\n\nbody\n\n## Second\n\n*Third*\n\ntail";
        let (response, block) = (ResponseRef::Request(RequestSeq::default()), block());
        let entries = |status| reasoning_entries(response, &block, text, &View::default(), status);
        let running = entries(ReasoningStatus::Running { since: 1_000 });
        let keys: Vec<_> = running.iter().map(|entry| entry.key().clone()).collect();
        let expected: Vec<_> = (0..4)
            .map(|section| EntryKey::Reasoning {
                response,
                block: block.clone(),
                section,
            })
            .collect();
        assert_eq!(keys, expected);
        // A title without a body stays inline like single-line reasoning.
        assert!(!running[0].expandable() && !running[2].expandable());
        assert_eq!(running[2].text(), "## Second");
        // Settled sections collapse under their titles; the streaming one stays open.
        assert_eq!(
            running[1].title(),
            Some(&Title::markdown("**First**", "", false))
        );
        assert_eq!(
            running[3].title(),
            Some(&Title::markdown("*Third*", "", true))
        );
        assert_eq!(running[3].body(), "tail");
        let live = running.iter().map(|entry| entry.timing.live());
        assert!(live.eq([false, false, false, true]));
        let incomplete = entries(ReasoningStatus::Incomplete);
        let suffixes = incomplete
            .iter()
            .map(|entry| entry.title().and_then(|title| title.suffix));
        assert!(suffixes.skip(1).eq([None, None, Some(" · incomplete")]));
    }

    #[test]
    fn text_visibility_preserves_whitespace_and_failed_response_attribution() {
        let rows = |live: &ObservedResponse| {
            response_entries(RequestSeq::default(), live, &View::default(), "Agent", 0)
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
