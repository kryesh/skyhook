//! Shape-based normalization for omitted Responses bookkeeping. Never infer a
//! tool call identity or resolve an ambiguous reference from array position.
use super::*;

fn optional_index(value: &Value, key: &str) -> Result<Option<usize>, ProviderError> {
    value.get(key).map(|_| index(value, key)).transpose()
}

fn optional_id<'a>(value: &'a Value, key: &str) -> Result<Option<&'a str>, ProviderError> {
    value
        .get(key)
        .map(|_| {
            let id = string(value, key)?;
            if id.is_empty() {
                Err(protocol("empty item ID"))
            } else {
                Ok(id)
            }
        })
        .transpose()
}

impl Decoder {
    fn next_index(&self) -> Result<usize, ProviderError> {
        self.items.last_key_value().map_or(Ok(0), |(id, _)| {
            id.checked_add(1)
                .ok_or_else(|| protocol("output index overflow"))
        })
    }

    fn item_by_id(&self, native_id: &str) -> Option<usize> {
        self.items.iter().find_map(|(id, item)| {
            (item.native_id == native_id || item.aliases.contains(native_id)).then_some(*id)
        })
    }

    fn bind_wire_index(&mut self, id: usize, wire: Option<usize>) -> Result<(), ProviderError> {
        let Some(wire) = wire else {
            return Ok(());
        };
        if self.items[&id].wire_index.is_some_and(|old| old != wire)
            || self
                .items
                .iter()
                .any(|(other, item)| *other != id && item.wire_index == Some(wire))
        {
            return Err(protocol("contradictory output item index"));
        }
        self.items.get_mut(&id).expect("known item").wire_index = Some(wire);
        Ok(())
    }

    fn vacant_index(&self, wire: Option<usize>) -> Result<usize, ProviderError> {
        if let Some(wire) = wire {
            if self
                .items
                .values()
                .any(|item| item.wire_index == Some(wire))
            {
                return Err(protocol("conflicting output item identity"));
            }
            if !self.items.contains_key(&wire) {
                return Ok(wire);
            }
        }
        self.next_index()
    }

    /// Only completed, semantically equivalent content can establish an alias
    /// for an output-item ID. Executable call IDs are never aliases.
    fn equivalent_snapshot(&self, item: &Item, native: &Value) -> bool {
        if kind(native).ok() != Some(item.kind) {
            return false;
        }
        if item.kind == Kind::Function {
            let call = item
                .call_id
                .as_deref()
                .or_else(|| item.ended.as_ref()?.get("call_id")?.as_str());
            if call.is_none() || call != native.get("call_id").and_then(Value::as_str) {
                return false;
            }
            if item
                .name
                .as_deref()
                .is_some_and(|name| Some(name) != native.get("name").and_then(Value::as_str))
            {
                return false;
            }
        }
        let Ok(parts) = final_parts(native) else {
            return false;
        };
        if let Some(old) = &item.ended {
            return final_parts(old).ok().as_ref() == Some(&parts);
        }
        if item.kind == Kind::Function {
            let Some(text) = item
                .final_arguments
                .as_deref()
                .or_else(|| item.parts.get(&0).map(|p| p.streamed.as_str()))
            else {
                return false;
            };
            return arguments(text).ok().as_ref()
                == native
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|s| arguments(s).ok())
                    .as_ref();
        }
        if item.parts.is_empty() {
            return false;
        }
        let observed: Vec<_> = item
            .parts
            .values()
            .map(|part| {
                part.ended.clone().unwrap_or_else(|| {
                    if item.kind == Kind::Reasoning {
                        BlockContent::Reasoning {
                            text: part.streamed.clone(),
                        }
                    } else {
                        BlockContent::Text {
                            text: part.streamed.clone(),
                        }
                    }
                })
            })
            .collect();
        observed == parts
    }

    pub(super) fn snapshot_index(
        &mut self,
        native: &Value,
        wire_index: Option<usize>,
        alias_candidates: Option<&BTreeSet<usize>>,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<usize, ProviderError> {
        let native_id = string(native, "id")?;
        if native_id.is_empty() {
            return Err(protocol("empty item ID"));
        }
        if let Some(id) = self.item_by_id(native_id) {
            self.bind_wire_index(id, wire_index)?;
            return Ok(id);
        }
        let candidates: Vec<_> = self
            .items
            .iter()
            .filter_map(|(id, item)| {
                let eligible = alias_candidates.map_or_else(
                    || wire_index.is_some_and(|wire| item.wire_index == Some(wire)),
                    |candidates| candidates.contains(id),
                );
                (eligible && self.equivalent_snapshot(item, native)).then_some(*id)
            })
            .collect();
        if candidates.len() > 1 {
            return Err(protocol("ambiguous final output item identity"));
        }
        if let Some(&id) = candidates.first() {
            self.bind_wire_index(id, wire_index)?;
            self.items
                .get_mut(&id)
                .expect("matched item")
                .aliases
                .insert(native_id.into());
            return Ok(id);
        }
        let id = self.vacant_index(wire_index)?;
        self.start(id, native, chunks)?;
        self.items.get_mut(&id).expect("started item").wire_index = wire_index;
        Ok(id)
    }

    /// Fill omitted indices/headers before the strict event handlers run.
    /// Internal positions are not evidence of provider-supplied wire indices.
    pub(super) fn normalize_event(
        &mut self,
        event: &mut Value,
        chunks: &mut Vec<ResponseChunk>,
    ) -> Result<(), ProviderError> {
        let name = string(event, "type")?.to_owned();
        if name == "response.output_item.added" {
            let wire = optional_index(event, "output_index")?;
            let id = self.vacant_index(wire)?;
            event["output_index"] = json!(id);
            return Ok(());
        }
        if name == "response.output_item.done" {
            let wire = optional_index(event, "output_index")?;
            let native = event
                .get("item")
                .ok_or_else(|| protocol("missing item"))?
                .clone();
            let id = self.snapshot_index(&native, wire, None, chunks)?;
            event["output_index"] = json!(id);
            return Ok(());
        }
        let wire = optional_index(event, "output_index")?;
        let wire_owner = wire.and_then(|wire| {
            self.items
                .iter()
                .find_map(|(id, item)| (item.wire_index == Some(wire)).then_some(*id))
        });
        let expected = if name.starts_with("response.reasoning_") {
            Some(Kind::Reasoning)
        } else if name.starts_with("response.function_call_arguments.") {
            Some(Kind::Function)
        } else if name.starts_with("response.output_text.") || name.starts_with("response.refusal.")
        {
            Some(Kind::Text)
        } else if name.starts_with("response.content_part.") {
            // Generic part events can also address reasoning items. Identity,
            // not a provider label, disambiguates output_text inside reasoning.
            optional_id(event, "item_id")?
                .and_then(|id| self.item_by_id(id))
                .or(wire_owner)
                .map(|id| self.items[&id].kind)
                .or_else(
                    || match event.pointer("/part/type").and_then(Value::as_str) {
                        Some("reasoning_text" | "summary_text") => Some(Kind::Reasoning),
                        _ => Some(Kind::Text),
                    },
                )
        } else {
            None
        };
        let Some(expected) = expected else {
            return Ok(());
        };
        let native_id = optional_id(event, "item_id")?.map(str::to_owned);
        let known = native_id.as_deref().and_then(|id| self.item_by_id(id));
        let id = if let Some(id) = known {
            let item = &self.items[&id];
            if item.kind != expected {
                return Err(protocol("event does not match output item kind"));
            }
            self.bind_wire_index(id, wire)?;
            id
        } else if let Some(native_id) = native_id.as_deref() {
            let id = self.vacant_index(wire)?;
            let native = match expected {
                Kind::Text => {
                    json!({"id":native_id,"type":"message","role":"assistant","content":[]})
                }
                Kind::Reasoning => json!({"id":native_id,"type":"reasoning","summary":[]}),
                Kind::Function => json!({"id":native_id,"type":"function_call"}),
            };
            self.start(id, &native, chunks)?;
            self.items.get_mut(&id).expect("started item").wire_index = wire;
            id
        } else if let Some(id) = wire_owner {
            if self.items[&id].kind != expected {
                return Err(protocol("event does not match output item kind"));
            }
            id
        } else {
            let candidates: Vec<_> = self
                .items
                .iter()
                .filter_map(|(id, item)| {
                    (item.kind == expected
                        && item.ended.is_none()
                        && wire.is_none_or(|wire| item.wire_index.is_none_or(|old| old == wire)))
                    .then_some(*id)
                })
                .collect();
            if candidates.len() != 1 {
                return Err(protocol("ambiguous or missing output item reference"));
            }
            self.bind_wire_index(candidates[0], wire)?;
            candidates[0]
        };
        event["output_index"] = json!(id);
        event["item_id"] = json!(self.items[&id].native_id);
        if expected == Kind::Function {
            return Ok(());
        }
        let summary = name.starts_with("response.reasoning_summary_");
        let key = if summary {
            "summary_index"
        } else {
            "content_index"
        };
        if optional_index(event, key)?.is_none() {
            let item = &self.items[&id];
            let positions: Vec<_> = item
                .parts
                .keys()
                .filter_map(|position| {
                    if expected == Kind::Reasoning {
                        ((position.is_multiple_of(2)) == summary).then_some(*position / 2)
                    } else {
                        Some(*position)
                    }
                })
                .collect();
            let position = match positions.as_slice() {
                [] => 0,
                [only] => *only,
                _ => return Err(protocol("ambiguous missing content index")),
            };
            event[key] = json!(position);
        }
        Ok(())
    }
}
