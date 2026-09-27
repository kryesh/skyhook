//! The native event types, and dispatch of their normalized form.

use super::normalization::NormalizedEvent;
use super::*;
use crate::named_enum::named_enum;

named_enum! {
    /// The native event types this codec reads; others carry nothing it uses.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) parsed enum Event {
        Created = "response.created",
        InProgress = "response.in_progress",
        Queued = "response.queued",
        ItemAdded = "response.output_item.added",
        ItemDone = "response.output_item.done",
        TextDelta = "response.output_text.delta",
        TextDone = "response.output_text.done",
        Annotation = "response.output_text.annotation.added",
        RefusalDelta = "response.refusal.delta",
        RefusalDone = "response.refusal.done",
        ReasoningDelta = "response.reasoning_text.delta",
        ReasoningDone = "response.reasoning_text.done",
        SummaryDelta = "response.reasoning_summary_text.delta",
        SummaryDone = "response.reasoning_summary_text.done",
        ArgumentsDelta = "response.function_call_arguments.delta",
        ArgumentsDone = "response.function_call_arguments.done",
        PartAdded = "response.content_part.added",
        PartDone = "response.content_part.done",
        ReasoningPartAdded = "response.reasoning_part.added",
        ReasoningPartDone = "response.reasoning_part.done",
        SummaryPartAdded = "response.reasoning_summary_part.added",
        SummaryPartDone = "response.reasoning_summary_part.done",
        Completed = "response.completed",
        Incomplete = "response.incomplete",
        Failed = "response.failed",
        Error = "error" | "response.error",
        Aborted = "response.aborted" | "response.cancelled" | "response.canceled"
            | "response.interrupted",
    }
}

impl Event {
    /// The kind of item an event on a live item addresses; `None` for the
    /// generic content-part events, whose item decides.
    pub(super) fn item_kind(self) -> Option<ItemKind> {
        match self {
            Self::TextDelta
            | Self::TextDone
            | Self::Annotation
            | Self::RefusalDelta
            | Self::RefusalDone => Some(ItemKind::Text),
            Self::ReasoningDelta
            | Self::ReasoningDone
            | Self::SummaryDelta
            | Self::SummaryDone
            | Self::ReasoningPartAdded
            | Self::ReasoningPartDone
            | Self::SummaryPartAdded
            | Self::SummaryPartDone => Some(ItemKind::Reasoning),
            Self::ArgumentsDelta | Self::ArgumentsDone => Some(ItemKind::ToolCall),
            _ => None,
        }
    }

    /// Whether the event addresses the summary namespace of a reasoning item.
    pub(super) fn is_summary(self) -> bool {
        matches!(
            self,
            Self::SummaryDelta | Self::SummaryDone | Self::SummaryPartAdded | Self::SummaryPartDone
        )
    }

    /// Whether a content-part event ends its part rather than announcing it.
    pub(super) fn is_done(self) -> bool {
        matches!(
            self,
            Self::PartDone | Self::ReasoningPartDone | Self::SummaryPartDone
        )
    }
}

impl Decoder {
    /// Feed a native Responses event.
    pub(crate) fn feed(&mut self, event: Value) -> Result<Vec<ResponseEvent>, ProviderError> {
        if self.completed {
            return Err(NATIVE.error("event after terminal response"));
        }
        let mut events = Vec::new();
        match self.normalize_event(&event)? {
            NormalizedEvent::Ignored => {}
            NormalizedEvent::ItemAdded {
                index,
                wire,
                native,
            } => self.start(index, wire, native)?,
            NormalizedEvent::ItemDone { index, native } => {
                self.end(index, native, false)?;
            }
            NormalizedEvent::Delta {
                part: (id, position),
                text,
            } => {
                self.delta(id, position, text, &mut events)?;
            }
            NormalizedEvent::ArgumentsDelta { item: id, text } => {
                if self.items[&id]
                    .function()?
                    .streaming()?
                    .final_arguments
                    .is_some()
                {
                    return Err(NATIVE.error("arguments delta after done"));
                }
                self.delta(id, 0, text, &mut events)?;
            }
            NormalizedEvent::ArgumentsDone { item: id, text } => {
                let arguments = match arguments(text) {
                    Ok(arguments) => arguments,
                    Err(_) => {
                        let item = self
                            .items
                            .get_mut(&id)
                            .expect("checked item")
                            .function_mut()?
                            .streaming_mut()?;
                        if item.final_arguments.is_some() {
                            return Err(NATIVE.error("conflicting final function arguments"));
                        }
                        item.final_arguments = Some(FinalArguments::Incomplete(text.into()));
                        return Ok(events);
                    }
                };
                // A placeholder `done` carries no input; the final item decides.
                if arguments.is_empty() {
                    return Ok(events);
                }
                self.validate_arguments(id, &arguments)?;
                let item = self
                    .items
                    .get_mut(&id)
                    .expect("checked item")
                    .function_mut()?
                    .streaming_mut()?;
                item.final_arguments = Some(FinalArguments::Object(arguments.clone()));
                if let (Some(call_id), Some(name)) = (&item.call_id, &item.name) {
                    let content = Content::ToolCall(
                        ToolCall::new(call_id.clone(), name.clone(), Value::Object(arguments))
                            .map_err(|error| NATIVE.error(error.to_string()))?,
                    );
                    self.close_part(id, 0, content)?;
                }
            }
            NormalizedEvent::PartEnded {
                part: (id, position),
                content,
            } => {
                self.close_part(id, position, content)?;
            }
            NormalizedEvent::PartAdded {
                part: (id, position),
                text,
            } => {
                match self.part(id, position)? {
                    Part::Streaming { text, added } if !*added && text.is_empty() => *added = true,
                    _ => return Err(NATIVE.error("duplicate or late content part added")),
                }
                if !text.is_empty() {
                    self.delta(id, position, text, &mut events)?;
                }
            }
            NormalizedEvent::Terminal {
                output,
                usage,
                finish,
            } => {
                self.complete_response(output, usage, finish, &mut events)?;
            }
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;

    #[test]
    fn function_arguments_are_typed_validated_and_identity_can_arrive_at_item_end() {
        let mut decoder = decoder();
        decoder
            .feed(added(
                0,
                json!({"id":"fc_1","type":"function_call","arguments":""}),
            ))
            .unwrap();
        let events = decoder.feed(json!({"type":"response.function_call_arguments.delta", "output_index":0, "item_id":"fc_1", "delta":"{\"query\":\"rust\"}"})).unwrap();
        assert_eq!(
            events,
            [ResponseEvent::Delta {
                block: BlockRef {
                    item: ItemId::try_from("fc_1".to_owned()).unwrap(),
                    block: BlockId::try_from("arguments_0".to_owned()).unwrap(),
                },
                kind: ItemKind::ToolCall,
                text: "{\"query\":\"rust\"}".into(),
            }]
        );
        assert!(decoder.feed(json!({"type":"response.function_call_arguments.done", "output_index":0, "item_id":"fc_1", "arguments":"{\"query\":\"rust\"}"})).unwrap().is_empty());
        assert!(decoder.feed(done(0, call_item())).unwrap().is_empty());
        let reduced = reduce(decoder.feed(completed(vec![call_item()])).unwrap());
        assert_eq!(reduced.completion.outcome(), Outcome::ToolUse);
        assert_eq!(reduced.items()[0].call().unwrap().name(), "search");
    }
}
