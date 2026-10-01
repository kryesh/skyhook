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
    /// Slots holding a diagnostic rendered for the viewer, shown whole.
    pub(super) diagnostic_fields: BTreeSet<FieldPointer>,
    /// Fields with a declared presentation, as saved with the result.
    pub(crate) presented: Presented,
}

impl Saved {
    pub(crate) fn load(output: &Output) -> Result<Self, ToolError> {
        let job = output.job.get();
        let saved = output.db.output(job).map_err(database)?;
        let captures = output.db.captures(job).map_err(database)?;
        let presented = output.db.presented_fields(job).map_err(database)?;
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
            presented,
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
        if let Some(value) = document.pointer_mut(field.as_str()) {
            *value = Value::String(diagnostic.render_for(viewer));
            product.result = Some(document["result"].take());
            self.fields.remove(&field);
            self.captures
                .retain(|pointer, _| pointer != &field && !pointer.contains(&field));
            self.diagnostic_fields.insert(field);
        }
        self.product = Some(product);
        Ok(())
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
    /// A detected sequence reads as the array it presents.
    pub(super) fn stored(&self, field: &FieldPointer) -> Option<Box<dyn Read>> {
        let capture = self
            .fields
            .contains(field)
            .then(|| self.captures.get(field))
            .flatten()?;
        let source = reader::CaptureReader::new(self.output.db.clone(), capture.id);
        Some(if capture.detection == Some(json::Detection::Sequence) {
            let source = BufReader::with_capacity(IO_BUFFER_BYTES, source);
            Box::new(json::SequenceArray::new(source))
        } else {
            Box::new(source)
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

/// Fields a saved document treats specially.
pub(super) struct Stored<'a> {
    /// Completed captures of the registered inventory the result installs.
    pub(super) referenced: &'a BTreeSet<FieldPointer>,
    /// Strings the schema declares as possibly JSON, classified here, once.
    pub(super) candidates: &'a BTreeSet<FieldPointer>,
    /// Fields whose presentation the schema declares; the paths of complete
    /// ones stay in the document.
    pub(super) presented: &'a Presented,
}

/// Persist a compact product, so loading it never parses a large value. Captures
/// hold the `referenced` fields, JSON or long candidate text, other strings over
/// `OFFLOAD_BYTES`, and the outermost larger containers that enclose none of these,
/// no complete field and no long string reached through object members alone.
/// A pointer an unreferenced raw capture owns stays inline.
pub(super) fn save_document(
    output: &Output,
    registered: &BTreeMap<FieldPointer, CaptureRow>,
    result: Option<Value>,
    captures_complete: bool,
    stored: &Stored<'_>,
) -> Result<(), ToolError> {
    let has_result = result.is_some();
    let mut document = json!({"result": result});
    let mut offload = Offload {
        output,
        registered,
        stored,
        fields: Vec::new(),
    };
    offload.visit(&FieldPointer::root(), &mut document)?;
    let result = has_result
        .then(|| serde_json::to_string(&document["result"]))
        .transpose()?;
    let job = output.job.get();
    (output.db)
        .save_output(
            job,
            result.as_deref(),
            captures_complete,
            &offload.fields,
            stored.presented,
        )
        .map_err(database)?;
    // Measured once, so batching notifications need not preview it again.
    measure_presentation(output);
    Ok(())
}

struct Offload<'a> {
    output: &'a Output,
    registered: &'a BTreeMap<FieldPointer, CaptureRow>,
    stored: &'a Stored<'a>,
    /// Captures the saved document references.
    fields: Vec<i64>,
}

impl Offload<'_> {
    fn visit(&mut self, field: &FieldPointer, value: &mut Value) -> Result<(), ToolError> {
        if self.stored.referenced.contains(field)
            && let Some(capture) = self.registered.get(field)
        {
            match value {
                Value::String(_) if self.stored.candidates.contains(field) => {
                    let source = reader::CaptureReader::new(self.output.db.clone(), capture.id);
                    self.classify(capture.id, json::detect(source)?, value)?;
                }
                Value::String(_) | Value::Object(_) | Value::Array(_) => clear(value),
                _ => return Ok(()),
            }
            self.fields.push(capture.id);
            return Ok(());
        }
        // Never overwrite or adopt an unreferenced raw capture merely because the
        // ordinary result happens to use the same pointer.
        if self.registered.contains_key(field) {
            return self.children(field, value);
        }
        match value {
            Value::String(text) if self.stored.candidates.contains(field) => {
                let detection = json::detect(text.as_bytes())?;
                // Short text that is not JSON stays inline, unclassified.
                if detection.placeholder().is_none() && text.len() <= OFFLOAD_BYTES {
                    return Ok(());
                }
                let text = std::mem::take(text);
                let capture = self.store(field, CaptureKind::Text, text.as_bytes())?;
                self.classify(capture, detection, value)
            }
            Value::String(text) if text.len() > OFFLOAD_BYTES => {
                let text = std::mem::take(text);
                self.store(field, CaptureKind::Text, text.as_bytes())?;
                Ok(())
            }
            Value::Object(_) | Value::Array(_) if self.offloadable(field, value) => {
                self.store(field, CaptureKind::Json, &serde_json::to_vec(value)?)?;
                clear(value);
                Ok(())
            }
            _ => self.children(field, value),
        }
    }

    fn children(&mut self, field: &FieldPointer, value: &mut Value) -> Result<(), ToolError> {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    self.visit(&field.property(key), value)?;
                }
            }
            Value::Array(items) => {
                for (index, value) in items.iter_mut().enumerate() {
                    self.visit(&field.index(index), value)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// A container over the limit is stored whole unless something inside it is
    /// stored or found on its own: a capture, candidate text, a complete field, or a
    /// long text field (reached through object members alone), which stays a text
    /// capture whose extent previews read without scanning it.
    fn offloadable(&self, field: &FieldPointer, value: &Value) -> bool {
        fn text_field(value: &Value) -> bool {
            match value {
                Value::String(text) => text.len() > OFFLOAD_BYTES,
                Value::Object(map) => map.values().any(text_field),
                _ => false,
            }
        }
        !field.is_root()
            && serialized_bytes(value, OFFLOAD_BYTES).is_err()
            && !(self.registered.keys())
                .chain(self.stored.candidates)
                .chain(&self.stored.presented.complete)
                .any(|inner| field.contains(inner))
            && !text_field(value)
    }

    fn store(
        &mut self,
        field: &FieldPointer,
        kind: CaptureKind,
        bytes: &[u8],
    ) -> Result<i64, ToolError> {
        let mut writer = PendingCapture::create(self.output, field, kind)?.open();
        writer.write_all(bytes)?;
        let capture = writer.finish()?.capture_id();
        self.fields.push(capture);
        Ok(capture)
    }

    /// Record what candidate text holds; a JSON field presents as its empty container.
    fn classify(
        &self,
        capture: i64,
        detection: json::Detection,
        value: &mut Value,
    ) -> Result<(), ToolError> {
        self.output
            .db
            .set_detection(capture, detection)
            .map_err(database)?;
        *value = detection
            .placeholder()
            .unwrap_or_else(|| Value::String(String::new()));
        Ok(())
    }
}

/// A stored field's placeholder: the empty value of its type.
fn clear(value: &mut Value) {
    match value {
        Value::String(text) => text.clear(),
        Value::Object(map) => map.clear(),
        Value::Array(items) => items.clear(),
        _ => {}
    }
}

/// Count JSON bytes without retaining them, stopping on the first write over `limit`.
pub(super) fn serialized_bytes(
    value: &impl serde::Serialize,
    limit: usize,
) -> serde_json::Result<usize> {
    struct Budget(usize);
    impl Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("over budget"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut budget = Budget(limit);
    serde_json::to_writer(&mut budget, value)?;
    Ok(limit - budget.0)
}

/// Install referenced fields into the document.
pub(super) fn hydrate(saved: &Saved, mut value: Value) -> Result<Value, ToolError> {
    for field in &saved.fields {
        hydrate_field(saved, &mut value, field)?;
    }
    Ok(value)
}

/// Replace a stored field's placeholder in `value` with the value its bytes hold.
fn hydrate_field(saved: &Saved, value: &mut Value, field: &FieldPointer) -> Result<(), ToolError> {
    let target = value
        .pointer_mut(field.as_str())
        .ok_or_else(|| ToolError::failed("invalid saved output field"))?;
    let mut bytes = Vec::new();
    saved
        .stored(field)
        .ok_or_else(|| ToolError::failed("saved output field is missing"))?
        .read_to_end(&mut bytes)?;
    *target = if target.is_string() {
        Value::String(String::from_utf8_lossy(&bytes).into_owned())
    } else {
        serde_json::from_slice(&bytes)?
    };
    Ok(())
}

/// The bytes automatic presentation emits for this output: the previewed result,
/// its shape and its truncation records. A rendered output diagnostic is not
/// counted.
fn presentation_size(output: &Output) -> Result<usize, ToolError> {
    let saved = Saved::load(output)?;
    let cancellation = crate::job::CancellationToken::new();
    let projected = projection::project(&saved, &cancellation)?;
    let truncated = (!projected.truncated.is_empty()).then_some(&projected.truncated);
    Ok(preview::json_bytes(&projected.result)
        + projected.shape.as_ref().map_or(0, preview::json_bytes)
        + truncated.map_or(0, preview::json_bytes))
}

/// The size notification batching budgets for this output: measured when it was
/// saved, or now when that did not finish.
pub(crate) fn presented_size(output: &Output) -> usize {
    match output.db.presented_bytes(output.job.get()) {
        Ok(Some(bytes)) => bytes,
        _ => measure_presentation(output),
    }
}

/// Measure and record the output's presentation size, a page when it cannot be
/// previewed. The size only budgets notifications, so failing to record it fails
/// nothing else.
fn measure_presentation(output: &Output) -> usize {
    let bytes = presentation_size(output).unwrap_or(PAGE_BYTES);
    let _ = output.db.set_presented_bytes(output.job.get(), bytes);
    bytes
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
        let mut source = Saved::load(self)
            .unwrap()
            .capture(&field.parse().unwrap())?;
        let mut bytes = Vec::new();
        source.read_to_end(&mut bytes).unwrap();
        Some(bytes)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{JobSpec, output::save_completed, tests::runtime};

    fn value(output: &Output) -> Value {
        let saved = Saved::load(output).unwrap();
        hydrate(&saved, saved.product.as_ref().unwrap().document()).unwrap()["result"].take()
    }

    #[tokio::test]
    async fn candidate_text_is_classified_once_and_reads_as_the_value_it_holds() {
        let (_root, manager, agent) = runtime().await;
        let schema = json!({"type":"object","properties":{
            "stdout":{"type":"string","contentMediaType":"application/json"},
            "note":{"type":"string"}
        }});
        let stdout: FieldPointer = "/result/stdout".parse().unwrap();
        let inexact = r#"{"id":9007199254740993}"#;
        for (text, captured, detection, expected) in [
            (
                r#"{"a":[1,2]}"#,
                false,
                Some(Detection::Object),
                json!({"a":[1,2]}),
            ),
            (
                "{\"a\":1}\n\n[2]\n",
                true,
                Some(Detection::Sequence),
                json!([{"a":1},[2]]),
            ),
            ("42\n", true, Some(Detection::NotJson), json!("42\n")),
            (
                inexact,
                true,
                Some(Detection::InexactNumber),
                json!(inexact),
            ),
            // Short inline text that is not JSON stays inline, unclassified.
            ("42\n", false, None, json!("42\n")),
        ] {
            let id = manager
                .test_running(JobSpec::test(agent.clone(), "classified"))
                .await
                .into_test_id();
            let output = manager.output(id);
            // Candidate text is classified whether it streamed or returned inline;
            // other strings are never parsed.
            let mut result = json!({"note":"{\"x\":1}"});
            let mut receipts = Vec::new();
            if captured {
                let mut writer = PendingCapture::create(&output, &stdout, CaptureKind::Text)
                    .unwrap()
                    .open();
                writer.write_all(text.as_bytes()).unwrap();
                receipts.push(writer.finish().unwrap());
            } else {
                result["stdout"] = text.into();
            }
            save_completed(&output, &schema, Some(result), true, receipts).unwrap();
            let saved = Saved::load(&output).unwrap();
            let classified = saved
                .captures
                .get(&stdout)
                .and_then(|capture| capture.detection);
            assert_eq!(classified, detection, "{text}");
            let placeholder = match detection {
                Some(detection) => detection.placeholder().unwrap_or_else(|| json!("")),
                None => json!(text),
            };
            assert_eq!(
                saved.product.as_ref().unwrap().document()["result"]["stdout"],
                placeholder
            );
            assert!(
                !saved
                    .captures
                    .contains_key(&FieldPointer::result().property("note"))
            );
            assert_eq!(
                value(&output),
                json!({"note":"{\"x\":1}", "stdout": expected})
            );
            if detection == Some(Detection::Sequence) {
                let mut args = OutputArgs::new(id);
                (args.field, args.index) = (Some(stdout.clone()), Some(1));
                let view = manager
                    .inspect_output(args, Default::default(), &Default::default())
                    .await
                    .unwrap();
                let preview = &view["presentation"]["preview"];
                assert_eq!(preview["elements"], json!([[2]]));
                assert_eq!(preview["total_elements"], 2);
            }
        }
    }

    /// `result` saved under `schema`, its captures finished or cut off.
    async fn saved(
        schema: &Value,
        result: Value,
        captures_complete: bool,
    ) -> (tempfile::TempDir, crate::job::JobManager, Saved) {
        let (root, manager, agent) = runtime().await;
        let id = manager
            .test_running(JobSpec::test(agent, "saved"))
            .await
            .into_test_id();
        let output = manager.output(id);
        save_completed(&output, schema, Some(result), captures_complete, Vec::new()).unwrap();
        let saved = Saved::load(&output).unwrap();
        (root, manager, saved)
    }

    #[tokio::test]
    async fn output_a_producer_did_not_finish_is_never_taken_as_json() {
        let schema = json!({"type":"object","properties":{
            "stdout":{"type":"string","contentMediaType":"application/json"}
        }});
        let (_root, _manager, saved) = saved(&schema, json!({"stdout": "{\"a\":1}"}), false).await;
        let document = saved.product.as_ref().unwrap().document();
        assert_eq!(document["result"]["stdout"], "{\"a\":1}");
    }

    #[tokio::test]
    async fn containers_of_complete_fields_stay_in_the_document() {
        let schema = json!({"type":"object","properties":{
            "details":{"type":"object","properties":{"instructions":{"x-skyhook-preview":"complete"}}}
        }});
        let assets: Vec<_> = (0..400).map(|index| format!("asset-{index}")).collect();
        let instructions = "step\n".repeat(150);
        let details = json!({"instructions": instructions, "assets": assets});
        let (_root, _manager, saved) = saved(&schema, json!({"details": details}), true).await;
        let details = FieldPointer::result().property("details");
        assert!(!saved.fields.contains(&details));
        let cancellation = crate::job::CancellationToken::new();
        let projected = super::projection::project(&saved, &cancellation);
        let projected = projected.unwrap().result.unwrap();
        assert_eq!(projected["details"]["instructions"], instructions);
    }

    /// Notification sizing, measured when the result is saved, is exactly what
    /// automatic presentation emits: the previewed result, its shape and its
    /// records, escapes included.
    #[tokio::test]
    async fn presentation_size_is_what_automatic_presentation_emits() {
        let (_root, manager, agent) = runtime().await;
        let text = "\u{1}".repeat(3 * CONTENT_BYTES);
        let items: Vec<_> = (0..2000).map(|id| json!({"id": id})).collect();
        let complete = json!({"properties":{"text":{"x-skyhook-preview":"complete"}}});
        for schema in [json!(true), complete] {
            let mut spec = JobSpec::test(agent.clone(), "sized");
            spec.output_schema = Some(schema.clone());
            let id = manager.test_running(spec).await.into_test_id();
            manager
                .test_finish(id, json!({"text": text, "items": items}))
                .await;
            let args = OutputArgs::new(id);
            let view = (manager.inspect_output(args, Default::default(), &Default::default()))
                .await
                .unwrap();
            let presentation = &view["presentation"];
            let emitted: usize = [
                &view["result"],
                &presentation["shape"],
                &presentation["truncated"],
            ]
            .into_iter()
            .filter(|value| !value.is_null())
            .map(|value| serde_json::to_vec(value).unwrap().len())
            .sum();
            assert!(presentation["shape"].is_object());
            assert_eq!(presented_size(&manager.output(id)), emitted);
        }
    }

    #[tokio::test]
    async fn large_containers_are_stored_whole_unless_something_inside_is_stored_alone() {
        let (_root, manager, agent) = runtime().await;
        let id = manager
            .test_running(JobSpec::test(agent, "offloaded"))
            .await
            .into_test_id();
        let output = manager.output(id);
        let items: Vec<_> = (0..400).map(|id| json!({"id":id,"name":"pod"})).collect();
        let result = json!({
            "items": items,
            // A long string keeps its own capture, so its container stays inline.
            "details": {"log": "l".repeat(OFFLOAD_BYTES + 1), "lines": 1},
            "small": {"a": 1},
            // So does the container of the slot presentation renders a diagnostic into.
            "error": {"message": null, "paths": items},
        });
        save_completed(
            &output,
            &json!(true),
            Some(result.clone()),
            true,
            Vec::new(),
        )
        .unwrap();
        let saved = Saved::load(&output).unwrap();
        let fields: Vec<_> = saved.fields.iter().map(FieldPointer::as_str).collect();
        assert_eq!(
            fields,
            [
                "/result/details/log",
                "/result/error/paths",
                "/result/items"
            ]
        );
        assert_eq!(
            saved.captures[&FieldPointer::result().property("items")].kind,
            CaptureKind::Json
        );
        assert_eq!(value(&output), result);
    }
}
