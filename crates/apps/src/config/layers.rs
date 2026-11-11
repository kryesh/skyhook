//! Each configuration file read as a node tree that remembers where it was written,
//! and the files merged by the schema's named entries.

use std::{path::Path, sync::Arc};

use skyhook::config::{Refused, process};
use skyhook_base::decode::{Entry, Map, Node, Source, Value};
use tokio::fs;

use super::loader::{LayerError, LayeredConfig};

/// A file's settings, each placed where the file writes it.
pub(super) struct Layer {
    entries: Map,
    at: Source,
}

pub(super) async fn read_layer(path: &Path) -> Result<Layer, LayerError> {
    let text = fs::read_to_string(path)
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => LayerError::Missing,
            _ => LayerError::Read(error),
        })?;
    let file: Arc<Path> = std::path::absolute(path).map_err(LayerError::Read)?.into();
    let node =
        skyhook::yaml::read(&text, Some(file)).map_err(|error| LayerError::Yaml(error.into()))?;
    let kind = node.kind();
    match node.value {
        Value::Map(entries) => Ok(Layer {
            entries,
            at: node.at,
        }),
        Value::Null => Err(LayerError::Blank),
        _ => Err(LayerError::NotMapping(kind)),
    }
}

/// The layers read so far, merged: each value still knows the file that wrote it.
#[derive(Default)]
pub(super) struct Merged {
    entries: Map,
    /// Where the last layer starts, which a refusal of the whole document names.
    at: Option<Source>,
}

impl Merged {
    /// Overlay `layer`: a named entry of a map `T` merges by name replaces the entry of
    /// that name whole, and any other top-level value replaces the value before it.
    pub(super) fn overlay<T: LayeredConfig>(&mut self, layer: Layer) {
        self.at = Some(layer.at);
        for entry in layer.entries {
            let named = T::NAMED_ENTRIES.contains(&entry.key.as_str());
            let held = self.entries.get_mut(&entry.key).map(|held| &mut held.value);
            match (held, entry.value.value) {
                (Some(Value::Map(held)), Value::Map(overlay)) if named => {
                    overlay.into_iter().for_each(|named| held.insert(named));
                }
                (_, value) => {
                    let at = entry.value.at;
                    let value = Node { value, at };
                    self.entries.insert(Entry { value, ..entry });
                }
            }
        }
    }

    /// The merged document as `T`, refusing what it refuses of itself given `context`.
    pub(super) fn decode<T: LayeredConfig>(
        &self,
        context: T::Context<'_>,
        complete: bool,
    ) -> Result<T, LayerError> {
        if self.entries.is_empty() && !T::OPTIONAL {
            return Err(LayerError::Empty);
        }
        let root = Node {
            value: Value::Map(self.entries.clone()),
            at: self.at.clone().unwrap_or(Source::START),
        };
        let check = |config: &T| -> Vec<Refused> { config.check(context, complete) };
        process(&root, check).map_err(LayerError::Invalid)
    }
}
