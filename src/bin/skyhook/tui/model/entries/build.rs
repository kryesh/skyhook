//! The entries one source of a history shows.
use super::super::jobs::{call_entry, job_entry};
use super::super::live::{ReasoningStatus, block_key, reasoning_entries, response_entries};
use super::super::notifications::job_event_entries;
use super::super::requests::{request_entry, request_timing};
use super::super::{
    Clean, Entry, EntryKey, EntryView, ResponseRef, Surface, Tab, Timing, Title, number, pretty,
    span,
};
use super::{Built, Dep, Inputs, Place, Source, ToolGroup, status_entry};
use skyhook::identity::JobId;
use skyhook::provider::protocol::{AssistantItem, BlockRef};
use skyhook::session::{
    CompactionTrigger, EventRecord, JobEvent, Message, PageFacts, SessionEvent, UserPart,
};
use std::fmt::Write as _;

/// A job's card, indented under its parents, and its tool group.
fn job_card(
    job: JobId,
    place: &Place,
    inputs: Inputs<'_>,
    built: &mut Built,
) -> Option<(Entry, Option<ToolGroup>)> {
    built.deps.push(Dep::Job(job));
    let info = inputs.job(&job)?;
    let mut entry = job_entry(info, inputs.projection, inputs.outputs, inputs.presentation);
    entry.indent = place.indent();
    Some((entry, group(place, inputs)))
}

/// The entries the source at `place` shows, with what they were built from.
pub(super) fn source(place: &Place, inputs: Inputs<'_>) -> Built {
    let mut built = Built::default();
    if let Some(gap) = place.source().gap() {
        let mut entry = Entry::new(EntryKey::Gap(gap), "Loading…".into(), Surface::Muted);
        entry.indent = place.indent();
        built.push(entry, group(place, inputs));
        return built;
    }
    match place.source() {
        Source::Unsaved(index) => {
            if let Some(status) = inputs.unsaved.get(index) {
                let key = EntryKey::UnsavedStatus(index);
                built.push(status_entry(key, &status.message), None);
            }
        }
        Source::Job(job) => {
            if let Some((mut entry, group)) = job_card(job, place, inputs, &mut built) {
                if inputs.presentation.tab == Tab::Jobs {
                    // The Jobs tab is a dense list; conversation grouping owns its
                    // spacing separately, and expanded documents stay unchanged.
                    entry.compact_after = true;
                    built.push(entry, None);
                } else {
                    built.push(entry, group);
                }
            }
        }
        Source::Request(request) => {
            built.deps.push(Dep::Request(request));
            if let Some(record) = inputs.request(request) {
                let today = inputs.presentation.today;
                built.push(request_entry(request, record, today), None);
            }
        }
        Source::Transcript(ordinal) => {
            if let Some(transcript) = inputs.window.transcript()
                && let Some(record) = transcript.record(ordinal)
            {
                conversation(record, place, &transcript.facts, inputs, &mut built);
            }
        }
        Source::Earlier | Source::More(_) | Source::Later => {}
    }
    built
}

fn conversation(
    record: &EventRecord,
    place: &Place,
    facts: &PageFacts,
    inputs: Inputs<'_>,
    built: &mut Built,
) {
    let Inputs {
        live,
        projection,
        presentation,
        ..
    } = inputs;
    let EntryView {
        agent,
        view,
        all_details,
        ..
    } = presentation;
    let agent_name = projection.agent_name(agent);
    let key = EntryKey::Record(record.sequence);
    match &record.event {
        SessionEvent::ModelRequested { .. } => {
            let request = record.sequence.request();
            built.deps.push(Dep::Request(request));
            if let Some(entry) = super::super::retry::retry_entry(inputs, agent, request) {
                built.push(entry, None);
            }
            // A settled response no commit replaced (an interrupted attempt)
            // stays at its journal position; a failure's is part of its card.
            if let Some(requested) = inputs.request(request)
                && requested.phase.settled_in_place()
                && let Some(response) = live.views.responses.get(&(agent.clone(), request))
                && response.settlement().is_some()
            {
                let since = requested.requested_millis;
                for entry in response_entries(request, response, view, agent_name, since) {
                    built.push(entry, None);
                }
            }
        }
        SessionEvent::MessageCommitted { message } => match message {
            Message::User(blocks) => {
                for (i, block) in blocks.iter().enumerate() {
                    let (title, text, surface) = match block {
                        UserPart::Text { text } => (
                            if agent.path().is_empty() {
                                "You"
                            } else {
                                "Parent"
                            },
                            text.clone(),
                            Surface::User,
                        ),
                        UserPart::ParentInput { text } => ("Parent", text.clone(), Surface::User),
                        UserPart::Attachment { attachment } => {
                            ("Attachment", pretty(attachment), Surface::User)
                        }
                        UserPart::JobEvents { events } => {
                            let jobs = events.iter().filter_map(JobEvent::job);
                            built.deps.extend(jobs.map(Dep::Job));
                            let group = ToolGroup::Notification(record.sequence, i);
                            let entries = job_event_entries(
                                record.sequence,
                                i,
                                events,
                                inputs,
                                view,
                                all_details,
                            );
                            for entry in entries {
                                built.push(entry, Some(group));
                            }
                            continue;
                        }
                        UserPart::Compaction { text } => {
                            ("Compaction", text.clone(), Surface::Muted)
                        }
                    };
                    let key = EntryKey::UserBlock {
                        record: record.sequence,
                        index: i,
                    };
                    let entry = Entry::titled(key, Title::plain(title), text, surface);
                    built.push(entry, None);
                }
            }
            Message::Assistant(items) => {
                assistant(record, items, facts, inputs, built);
            }
            // Persisted runtime state is model context, not conversation; a result
            // shows with the call it answers.
            Message::State(_) | Message::Tool(_) => {}
        },
        // A job its script issued shows under its parent's card instead.
        SessionEvent::JobCreated {
            job,
            parent,
            origin,
            ..
        } if !skyhook::job::nests(*parent, origin) => {
            if let Some((entry, group)) = job_card(*job, place, inputs, built) {
                built.push(entry, group);
            }
        }
        SessionEvent::Compaction { checkpoint } => {
            let open = view.is_expanded(&key, false);
            let title = match checkpoint.trigger {
                CompactionTrigger::Occupancy(tokens) => {
                    format!("Context compacted at {} tokens", number(tokens))
                }
                CompactionTrigger::Overflow => "Context compacted after it overflowed".to_owned(),
            };
            let body = if open {
                format!("Summary and retained sources\n{}", pretty(checkpoint))
            } else {
                String::new()
            };
            let title = Title::disclosed(title, open);
            built.push(Entry::titled(key, title, body, Surface::Muted), None);
        }
        SessionEvent::Status { message } => built.push(status_entry(key, message), None),
        // A failure its request's card reports is not repeated.
        SessionEvent::AgentFailed { failure } if !facts.reported.contains(&record.sequence) => {
            built.push(super::super::retry::failure_entry(key, failure), None);
        }
        SessionEvent::CompactionFailed { error, .. } => {
            let text = format!("Compaction failed; previous context retained\n{error}");
            built.push(Entry::new(key, text, Surface::Error), None);
        }
        _ => {}
    }
}

fn assistant(
    record: &EventRecord,
    items: &[AssistantItem],
    facts: &PageFacts,
    inputs: Inputs<'_>,
    built: &mut Built,
) {
    let Inputs {
        projection,
        presentation,
        ..
    } = inputs;
    let EntryView {
        agent,
        view,
        all_details,
        ..
    } = presentation;
    let agent_name = projection.agent_name(agent);
    let message = record.sequence.message();
    let answer = facts.answers.get(&message);
    let request = answer.map(|answer| answer.request);
    built.deps.extend(request.map(Dep::Request));
    let response = request.map_or(ResponseRef::Message(message), ResponseRef::Request);
    let footer = request
        .and_then(|request| inputs.request(request))
        .map(|request| {
            let mut footer = request.profile.profile.model.as_str().to_owned();
            // Once the answer settles: how long it took, and its whole turn
            // when that began with an earlier request.
            if let Timing::Took { since, until } = request_timing(request) {
                let _ = write!(footer, " · {}", span(since, until));
                if let Some(turn) = answer.and_then(|answer| answer.turn)
                    && turn < since
                {
                    let _ = write!(footer, " · turn {}", span(turn, until));
                }
            }
            Clean::from(footer)
        });
    // The model footer sits under the last visible text of an answer; a working
    // turn (one with calls) has none.
    let final_text = (!items.iter().any(|item| item.call().is_some()))
        .then(|| {
            items.iter().rev().find_map(|item| match item {
                AssistantItem::Text { blocks, .. } => {
                    blocks.iter().rfind(|block| !block.text.trim().is_empty())
                }
                _ => None,
            })
        })
        .flatten();
    for item in items {
        match item {
            AssistantItem::Text { id, blocks, .. } => {
                for block in blocks.iter().filter(|block| !block.text.trim().is_empty()) {
                    let block_ref = BlockRef {
                        item: id.clone(),
                        block: block.id.clone(),
                    };
                    let mut entry = Entry::titled(
                        block_key(response, &block_ref),
                        Title::plain(agent_name),
                        block.text.clone(),
                        Surface::Agent,
                    );
                    if final_text.is_some_and(|last| std::ptr::eq(last, block)) {
                        entry.footer.clone_from(&footer);
                    }
                    built.push(entry, None);
                }
            }
            AssistantItem::Reasoning { id, blocks, .. } => {
                for block in blocks {
                    let block_ref = BlockRef {
                        item: id.clone(),
                        block: block.id.clone(),
                    };
                    let status = ReasoningStatus::Complete;
                    for entry in reasoning_entries(response, &block_ref, &block.text, view, status)
                    {
                        built.push(entry, None);
                    }
                }
            }
            AssistantItem::ToolCall { call, .. } => {
                let id = call.id().to_owned();
                built.deps.push(Dep::Call(message, id.clone()));
                // An admitted call is shown by its job's card instead.
                let made = (message, id);
                if !facts.admitted.contains(&made) {
                    let result = facts.results.get(&made);
                    let key = EntryKey::ToolCall {
                        message,
                        call: made.1,
                    };
                    let open = view.is_expanded(&key, all_details);
                    let call = (call.name(), Some(call.arguments()), result);
                    let entry = call_entry(key, call, agent, projection, open);
                    built.push(entry, Some(ToolGroup::Response(message)));
                }
            }
        }
    }
}

/// The tool group of the card at `place`: its root job's, which a response's call
/// groups with that response.
fn group(place: &Place, inputs: Inputs<'_>) -> Option<ToolGroup> {
    let Source::Transcript(ordinal) = *place.0.first()? else {
        return None;
    };
    let record = inputs.window.transcript()?.record(ordinal)?;
    match &record.event {
        SessionEvent::JobCreated { job, origin, .. } => {
            Some(origin.as_ref().map_or(ToolGroup::Job(*job), |origin| {
                ToolGroup::Response(origin.message)
            }))
        }
        _ => None,
    }
}
