//! Saved output documents and captures: loading, persisting and sizing them.
use super::*;

/// Strings longer than this are offloaded from a saved document into captures.
const OFFLOAD_BYTES: usize = 4 * 1024;

/// The result field an output diagnostic renders into, where a producer's result
/// type declares a `DiagnosticSlot`. Without an output diagnostic, user JSON of
/// the same shape remains opaque.
pub fn diagnostic_slot() -> FieldPointer {
    FieldPointer::result().property("error").property("message")
}

/// A result's [`diagnostic_slot`] field, serialized as null: presentation renders
/// the output diagnostic into it for each viewer's capabilities.
#[derive(Serialize)]
pub(crate) struct DiagnosticSlot;

/// The diagnostic slot within a result value.
pub(crate) fn diagnostic_slot_in(result: &mut Value) -> Option<&mut Value> {
    diagnostic_slot()
        .segments()
        .skip(1)
        .try_fold(result, |value, key| value.get_mut(key.as_str()))
}

pub(super) fn database(error: crate::session::DbError) -> ToolError {
    ToolError::io(std::io::Error::other(error))
}

/// One job's output rows in the session database. Every operation locks the
/// connection briefly, so call it off async worker threads for anything large.
#[derive(Clone)]
pub(crate) struct Output {
    pub(super) db: SharedDb,
    pub(super) job: JobId,
}

/// One run's terminal product. `result` is `None` when the run produced none; a
/// literal null result is `Some(Value::Null)`.
pub(super) struct Product {
    pub(super) result: Option<Value>,
    /// Every capture the producer finished was admitted into the result.
    pub(super) captures_complete: bool,
}

impl Product {
    /// The product as pointers address it: `{"result": ...}`, with referenced
    /// fields as placeholders and a missing result as null.
    pub(super) fn document(&self) -> Value {
        json!({"result": self.result})
    }
}

/// A snapshot of the saved product and capture registrations. Capture bytes stay
/// in the database and are read live, so an open capture can still grow.
pub(crate) struct Saved {
    pub(super) output: Output,
    pub(super) product: Option<Product>,
    /// Pointers the product references.
    pub(super) fields: BTreeSet<FieldPointer>,
    pub(super) captures: BTreeMap<FieldPointer, CaptureRow>,
    /// Capability-filtered slots must never reuse capability-independent render caches.
    pub(super) diagnostic_fields: BTreeSet<FieldPointer>,
}

impl Saved {
    pub(crate) fn load(output: &Output) -> Result<Self, ToolError> {
        let job = output.job.get();
        let saved = output.db.output(job).map_err(database)?;
        let captures = output.db.captures(job).map_err(database)?;
        let (product, fields) = match saved {
            Some(saved) => {
                let result = saved.result.as_deref().map(serde_json::from_str);
                let product = Product {
                    result: result.transpose()?,
                    captures_complete: saved.captures_complete,
                };
                (Some(product), saved.fields.into_iter().collect())
            }
            None => (None, BTreeSet::new()),
        };
        Ok(Self {
            output: output.clone(),
            product,
            fields,
            diagnostic_fields: BTreeSet::new(),
            captures: captures
                .into_iter()
                .map(|capture| (capture.pointer.clone(), capture))
                .collect(),
        })
    }

    /// Replace the registered presentation-owned slot before selection or paging.
    /// No inference from tool names or traversal of user error objects.
    pub(super) fn present_diagnostics(
        &mut self,
        output_diagnostic: Option<&crate::tool::diagnostic::Diagnostic>,
        viewer: DiagnosticViewer<'_>,
    ) -> Result<(), ToolError> {
        let Some(diagnostic) = output_diagnostic else {
            return Ok(());
        };
        let Some(mut product) = self.product.take() else {
            return Ok(());
        };
        let field = diagnostic_slot();
        let mut document = product.document();
        // A producer can have offloaded a containing value. Hydrate only that
        // ancestor before replacing its registered diagnostic slot.
        let ancestor = self
            .fields
            .iter()
            .find(|stored| *stored == &field || stored.contains(&field))
            .cloned();
        if let Some(ancestor) = &ancestor {
            hydrate_field(self, &mut document, ancestor)?;
        }
        if let Some(value) = document.pointer_mut(field.as_str()) {
            *value = Value::String(diagnostic.render_for(viewer));
            product.result = Some(document["result"].take());
            if let Some(ancestor) = ancestor {
                self.fields.retain(|stored| stored != &ancestor);
            }
            self.captures
                .retain(|pointer, _| pointer != &field && !pointer.contains(&field));
            self.diagnostic_fields.insert(field);
        }
        self.product = Some(product);
        Ok(())
    }

    pub(super) fn cacheable(&self, field: &FieldPointer) -> bool {
        !self
            .diagnostic_fields
            .iter()
            .any(|diagnostic| diagnostic == field || field.contains(diagnostic))
    }

    /// Any registered capture at `field`, complete or not.
    pub(super) fn capture(&self, field: &FieldPointer) -> Option<Source> {
        self.captures.get(field).map(|capture| {
            Source::Capture(reader::CaptureReader::new(
                self.output.db.clone(),
                capture.id,
            ))
        })
    }

    /// The bytes of a referenced field, which the document stores as a placeholder.
    pub(super) fn stored(&self, field: &FieldPointer) -> Option<Source> {
        self.fields
            .contains(field)
            .then(|| self.capture(field))
            .flatten()
    }

    /// All bytes of the capture at `field`, if one is registered.
    pub(crate) fn bytes(&self, field: &FieldPointer) -> Result<Option<Vec<u8>>, ToolError> {
        let Some(mut source) = self.capture(field) else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        source.read_to_end(&mut bytes)?;
        Ok(Some(bytes))
    }
}

/// Presentation annotations for a script's independently saved return value.
/// Pointers are rooted at /result/value. Annotated fields opt into truncation and paging.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct ScriptPresentation {
    #[serde(default)]
    pub fields: BTreeSet<FieldPointer>,
}

impl ScriptPresentation {
    pub(super) fn load(output: &Output) -> Result<Self, ToolError> {
        let rows = output.db.presentation(output.job.get()).map_err(database)?;
        Ok(Self {
            fields: rows.fields.into_iter().collect(),
        })
    }
}

/// Run blocking output work off the async workers.
pub(in crate::job) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ToolError> + Send + 'static,
) -> Result<T, ToolError> {
    let joined = tokio::task::spawn_blocking(work).await;
    joined.map_err(ToolError::failed)?
}

/// Persist a compact product whose referenced and large strings live in captures.
/// `referenced` names completed captures of the `registered` inventory the result
/// installs; other strings over `OFFLOAD_BYTES` are offloaded into new text
/// captures, unless an unreferenced raw capture already owns that pointer.
pub(super) fn save_document(
    output: &Output,
    registered: &BTreeMap<FieldPointer, CaptureRow>,
    result: Option<Value>,
    captures_complete: bool,
    referenced: &BTreeSet<FieldPointer>,
) -> Result<(), ToolError> {
    let has_result = result.is_some();
    let mut document = json!({"result": result});
    let mut fields = Vec::new();
    fn visit(
        output: &Output,
        registered: &BTreeMap<FieldPointer, CaptureRow>,
        referenced: &BTreeSet<FieldPointer>,
        field: &FieldPointer,
        value: &mut Value,
        fields: &mut Vec<i64>,
    ) -> Result<(), ToolError> {
        if referenced.contains(field)
            && let Some(capture) = registered.get(field)
        {
            match value {
                Value::String(text) => text.clear(),
                Value::Object(map) => map.clear(),
                Value::Array(items) => items.clear(),
                _ => return Ok(()),
            }
            fields.push(capture.id);
            return Ok(());
        }
        match value {
            // Do not overwrite or adopt an unreferenced raw capture merely
            // because the ordinary result happens to use the same pointer.
            Value::String(text)
                if text.len() > OFFLOAD_BYTES && !registered.contains_key(field) =>
            {
                let mut writer = PendingCapture::create(output, field, CaptureKind::Text)?.open();
                writer.write_all(text.as_bytes())?;
                fields.push(writer.finish()?.capture_id());
                text.clear();
            }
            Value::Object(map) => {
                for (key, value) in map {
                    let child = field.property(key);
                    visit(output, registered, referenced, &child, value, fields)?;
                }
            }
            Value::Array(items) => {
                for (index, value) in items.iter_mut().enumerate() {
                    let child = field.index(index);
                    visit(output, registered, referenced, &child, value, fields)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    visit(
        output,
        registered,
        referenced,
        &FieldPointer::root(),
        &mut document,
        &mut fields,
    )?;
    let result = has_result
        .then(|| serde_json::to_string(&document["result"]))
        .transpose()?;
    output
        .db
        .save_output(
            output.job.get(),
            result.as_deref(),
            captures_complete,
            &fields,
        )
        .map_err(database)
}

/// Install referenced fields into the document.
pub(super) fn hydrate(saved: &Saved, mut value: Value) -> Result<Value, ToolError> {
    for field in &saved.fields {
        hydrate_field(saved, &mut value, field)?;
    }
    Ok(value)
}

pub(super) fn hydrate_field(
    saved: &Saved,
    value: &mut Value,
    field: &FieldPointer,
) -> Result<(), ToolError> {
    let target = value
        .pointer_mut(field.as_str())
        .ok_or_else(|| ToolError::failed("invalid saved output field"))?;
    load_field(saved, target, field)
}

/// Replace a stored field's placeholder with its bytes.
pub(super) fn load_field(
    saved: &Saved,
    target: &mut Value,
    field: &FieldPointer,
) -> Result<(), ToolError> {
    let bytes = saved
        .bytes(field)?
        .ok_or_else(|| ToolError::failed("saved output field is missing"))?;
    *target = if target.is_string() {
        Value::String(String::from_utf8_lossy(&bytes).into_owned())
    } else {
        serde_json::from_slice(&bytes)?
    };
    Ok(())
}

/// Upper bound on the bytes automatic presentation would emit for this output
/// under its result `schema`, used to budget notification batches. Inline content
/// counts as its stored length. A truncatable capture-backed field counts as the
/// serialized prefix presentation retains, and one inside a truncatable field as
/// at most a page. Any other is hydrated in full (`bytes * 6` covers JSON escaping).
pub(crate) fn presentation_size(output: &Output, schema: &Value) -> usize {
    let estimate = || -> Result<usize, ToolError> {
        let sizes = output.db.output_sizes(output.job.get()).map_err(database)?;
        let Some(crate::session::OutputSizes { result, fields }) = sizes else {
            return Ok(PAGE_BYTES);
        };
        let saved = Saved::load(output)?;
        let document = saved.product.as_ref().map(Product::document);
        let mut truncatable = ScriptPresentation::load(output)?.fields;
        if let Some(document) = &document {
            let result = &document["result"];
            truncatable.extend(truncation::annotated_fields(
                result,
                &FieldPointer::result(),
                schema,
            ));
        }
        let size = |bytes| usize::try_from(bytes).unwrap_or(usize::MAX);
        fields
            .into_iter()
            .try_fold(size(result), |total, (field, bytes)| {
                let emitted = size(bytes).saturating_mul(6);
                let emitted = if (truncatable.iter())
                    .any(|annotated| annotated != &field && annotated.contains(&field))
                {
                    emitted.min(PAGE_BYTES)
                } else if truncatable.contains(&field)
                    && let Some(document) = &document
                    && let Some(placeholder) = document.pointer(field.as_str())
                    && let Some(mut source) = saved.stored(&field)
                {
                    let (prefix, ..) =
                        truncation::retained(placeholder, &mut source, &mut Vec::new())?;
                    serde_json::to_vec(&prefix)?.len()
                } else {
                    emitted
                };
                Ok(total.saturating_add(emitted))
            })
    };
    estimate().unwrap_or(PAGE_BYTES)
}

#[cfg(test)]
impl Output {
    /// Register `field` holding `bytes`, as an unfinished producer leaves it,
    /// replacing any capture already there.
    pub(crate) fn test_capture(&self, field: &str, kind: CaptureKind, bytes: &[u8]) {
        let field: FieldPointer = field.parse().unwrap();
        if let Some(existing) = Saved::load(self).unwrap().captures.get(&field) {
            self.db.delete_capture(existing.id).unwrap();
        }
        let mut writer = PendingCapture::create(self, &field, kind).unwrap().open();
        writer.write_all(bytes).unwrap();
        writer.flush().unwrap();
        // Keep the row: dropping a writer only deletes abandoned builtin text captures.
        drop(writer);
    }

    pub(crate) fn test_bytes(&self, field: &str) -> Option<Vec<u8>> {
        Saved::load(self)
            .unwrap()
            .bytes(&field.parse().unwrap())
            .unwrap()
    }

    /// The compact saved product as `{"result": ...}`, if the job has finished.
    pub(crate) fn test_document(&self) -> Option<Value> {
        Saved::load(self)
            .unwrap()
            .product
            .map(|product| product.document())
    }

    pub(crate) fn test_captures_complete(&self) -> Option<bool> {
        let saved = Saved::load(self).unwrap();
        saved.product.map(|product| product.captures_complete)
    }

    pub(crate) fn test_fields(&self) -> Vec<FieldPointer> {
        Saved::load(self).unwrap().fields.into_iter().collect()
    }

    pub(crate) fn test_delete_capture(&self, capture: &CompletedCapture) {
        self.db.delete_capture(capture.capture_id()).unwrap();
    }
}
