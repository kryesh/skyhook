//! Saved output documents and captures: loading, preparing and sizing them.
use super::*;
use crate::session::{CaptureExtent, StagedBytes};
use skyhook_worker::fs::spill::SpillError;

/// Strings longer than this are offloaded from a saved document into captures.
const OFFLOAD_BYTES: usize = 4 * 1024;

fn database(error: crate::db::DbError) -> ToolError {
    ToolError::io(std::io::Error::other(error))
}

/// One run's output rows in the session database, and where its fields past the
/// saved-field limit are kept. Every operation locks the connection briefly, so call
/// it off async worker threads for anything large.
#[derive(Clone)]
pub(crate) struct Output {
    pub(super) db: SharedDb,
    pub(super) reads: crate::session::Reads,
    pub(super) changes: crate::session::OutputChanges,
    pub(super) spill: Spill,
    session: crate::identity::SessionId,
    pub(super) job: JobId,
    pub(super) invocation: crate::job::Invocation,
}

impl Output {
    /// The rows of run `invocation` of job `job`, spilling as `spill` says.
    pub(crate) fn of(
        store: &crate::session::SessionStore,
        spill: &Spill,
        job: JobId,
        invocation: crate::job::Invocation,
    ) -> Self {
        Self {
            db: store.outputs(),
            reads: store.reads(),
            changes: store.output_changes(),
            spill: spill.clone(),
            session: store.id(),
            job,
            invocation,
        }
    }

    /// The rows' key: the job and its run.
    pub(super) fn run(&self) -> crate::job::Run {
        crate::job::Run(self.job, self.invocation)
    }

    /// A new, empty file in the session host's spill directory for `part` of the run's
    /// output, created for its user alone. There is no other place to keep it: the
    /// write that needed the file fails as creating it did.
    pub(super) fn spill_file(
        &self,
        part: impl std::fmt::Display,
    ) -> Result<(std::fs::File, skyhook_worker::op::TargetPath), SpillError> {
        let stem = format!("job{}-part{part}", self.job);
        let name = skyhook_worker::fs::spill::SpillName::unique(&self.session, &stem);
        skyhook_worker::fs::spill::create(self.spill.root.as_deref(), &name)
    }
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
    pub(super) reads: crate::session::Reads,
    pub(super) product: Option<Product>,
    /// Pointers the product references.
    pub(super) fields: BTreeSet<FieldPointer>,
    pub(super) captures: BTreeMap<FieldPointer, CaptureRow>,
    /// Fields with a declared presentation, as saved with the result.
    pub(crate) presented: Presented,
    /// The fields whose file in the session host's spill directory no longer holds
    /// what was saved there, and why: a preview shows each as its placeholder, and
    /// reading one fails.
    pub(super) unreadable: BTreeMap<FieldPointer, Unreadable>,
    /// The latest of each capture whose target is keeping its stream as it runs.
    following: std::collections::HashMap<i64, crate::session::LiveTail>,
    /// The captures whose file failed a reader of this snapshot, until
    /// [`Saved::reading`] makes their fields unreadable.
    lost: (reader::Lost, std::sync::mpsc::Receiver<(i64, Unreadable)>),
}

/// Why the file the session host kept a field in cannot be read, as its user is told.
#[derive(Clone, Copy, Debug, thiserror::Error)]
pub(super) enum Unreadable {
    #[error("{}", .0.as_str().replace('_', " "))]
    Os(skyhook_worker::op::IoKind),
    #[error("not a regular file")]
    NotRegular,
    #[error("shorter than was saved")]
    Short,
}

impl Unreadable {
    /// Why `capture`'s file is not the regular file, at least as long, that was saved.
    fn of(capture: &CaptureRow) -> Option<Self> {
        let crate::session::CaptureBytes::Spilled { path, extent } = &capture.bytes else {
            return None;
        };
        match std::fs::symlink_metadata(path) {
            Err(error) => Some(Self::Os(error.kind().into())),
            Ok(found) if !found.is_file() => Some(Self::NotRegular),
            Ok(found) if found.len() < extent.bytes => Some(Self::Short),
            Ok(_) => None,
        }
    }

    /// Why a read of a field's file failed with `error`.
    pub(super) fn read(error: &std::io::Error) -> Self {
        use skyhook_worker::op::FsError;
        match error.get_ref().and_then(|source| source.downcast_ref()) {
            Some(FsError::NotRegular) => Self::NotRegular,
            _ if error.kind() == std::io::ErrorKind::UnexpectedEof => Self::Short,
            _ => Self::Os(error.kind().into()),
        }
    }
}

impl Saved {
    pub(crate) fn load(output: &Output) -> Result<Self, ToolError> {
        output.changes.read(output.run());
        let rows = output.reads.saved(output.run()).map_err(database)?;
        let mut saved = Self::of(output.reads.clone(), rows)?;
        saved.following = output.changes.following();
        Ok(saved)
    }

    /// What a run saved, from its `rows`; capture bytes are read through `reads`.
    pub(crate) fn of(
        reads: crate::session::Reads,
        rows: crate::session::SavedRows,
    ) -> Result<Self, ToolError> {
        let crate::session::SavedRows {
            output: saved,
            captures,
            presented,
        } = rows;
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
        let unreadable = captures
            .iter()
            .filter_map(|capture| Some((capture.pointer.clone(), Unreadable::of(capture)?)));
        Ok(Self {
            reads,
            product,
            fields,
            presented,
            following: Default::default(),
            lost: std::sync::mpsc::channel(),
            unreadable: unreadable.collect(),
            captures: captures
                .into_iter()
                .map(|capture| (capture.pointer.clone(), capture))
                .collect(),
        })
    }

    /// What `read` makes of this snapshot, with the snapshot. A field whose file
    /// fails `read` is unreadable from then on, and `read` starts over: the failure
    /// is that field's, as if its file had been found gone.
    pub(crate) fn reading<T>(
        mut self,
        read: impl Fn(&Self) -> Result<T, ToolError>,
    ) -> Result<(Self, T), ToolError> {
        loop {
            let made = read(&self);
            let mut found = false;
            for (capture, why) in self.lost.1.try_iter() {
                if let Some(row) = self.captures.values().find(|row| row.id == capture) {
                    found |= self.unreadable.insert(row.pointer.clone(), why).is_none();
                }
            }
            if !found {
                return Ok((self, made?));
            }
        }
    }

    /// A reader of what the session holds of `capture`.
    pub(super) fn reader(&self, capture: &CaptureRow) -> reader::CaptureReader {
        reader::CaptureReader::new(self.reads.clone(), capture.id).reporting(self.lost.0.clone())
    }

    /// What the session holds of any registered capture at `field`, complete or not:
    /// all of it, or the first bytes of a file on the job's target.
    pub(super) fn held(&self, field: &FieldPointer) -> Option<Source> {
        let capture = self.captures.get(field)?;
        Some(Source::Capture(self.reader(capture)))
    }

    /// What the session holds of `capture`, unless its file there is unreadable:
    /// all of it, or the first bytes of a file on the job's target.
    pub(super) fn bytes(&self, capture: &CaptureRow) -> Result<Source, ToolError> {
        self.readable(&capture.pointer)?;
        let latest = self.following.get(&capture.id).cloned();
        Ok(Source::Capture(self.reader(capture).following(latest)))
    }

    /// Fails for a `field` whose file the session host can no longer read.
    pub(super) fn readable(&self, field: &FieldPointer) -> Result<(), ToolError> {
        match self.unreadable.contains_key(field) {
            true => Err(files::unavailable(field)),
            false => Ok(()),
        }
    }

    /// The capture of a referenced field, which the document stores as a placeholder.
    pub(super) fn referenced(&self, field: &FieldPointer) -> Option<&CaptureRow> {
        (self
            .fields
            .contains(field)
            .then(|| self.captures.get(field)))
        .flatten()
    }

    /// All the bytes of a referenced field's `capture`. A detected sequence reads as
    /// the array it presents.
    pub(super) fn whole(&self, capture: &CaptureRow) -> Result<Box<dyn Read>, ToolError> {
        let source = self.bytes(capture)?;
        Ok(
            if capture.detection == Some(skyhook_tools::output::json::Detection::Sequence) {
                let source = BufReader::with_capacity(IO_BUFFER_BYTES, source);
                Box::new(super::json::SequenceArray::new(source))
            } else {
                Box::new(source)
            },
        )
    }
}

/// Fields a saved document treats specially.
pub(super) struct Stored<'a> {
    /// Completed captures of the registered inventory the result installs.
    pub(super) referenced: &'a BTreeSet<FieldPointer>,
    /// Strings the schema declares as possibly JSON, classified here, once.
    pub(super) candidates: &'a BTreeSet<FieldPointer>,
    /// Fields whose presentation is declared; the paths of complete ones stay in
    /// the document.
    pub(super) presented: Presented,
    /// Registered captures that hold the first bytes of a file on the job's target.
    pub(super) files: Vec<(i64, skyhook_tools::output::CaptureFile)>,
}

/// Prepare a compact product, so loading it never parses a large value. Captures
/// hold the `referenced` fields, JSON or long candidate text, other strings over
/// `OFFLOAD_BYTES`, and the outermost larger containers that enclose none of these,
/// no complete field and no long string reached through object members alone.
/// A pointer an unreferenced raw capture owns stays inline. Offloaded captures are
/// written here, staged for the finish that saves the rest.
pub(super) fn prepare_document(
    output: &Output,
    registered: &BTreeMap<FieldPointer, CaptureRow>,
    (result, captures_complete): (Option<Value>, bool),
    stored: Stored<'_>,
) -> std::io::Result<OutputRows> {
    let has_result = result.is_some();
    let mut document = json!({"result": result});
    let mut offload = Offload {
        output,
        registered,
        stored: &stored,
        fields: Vec::new(),
        staged: Vec::new(),
    };
    let visited = offload.visit(&FieldPointer::root(), &mut document);
    let Offload { fields, staged, .. } = offload;
    if let Err(error) = visited {
        // Nothing keeps what was staged for a product that is not saved.
        for capture in staged {
            if let Ok(Some(file)) = output.db.delete_capture(capture.id()) {
                let _ = std::fs::remove_file(file);
            }
        }
        return Err(error);
    }
    let result = has_result
        .then(|| serde_json::to_string(&document["result"]))
        .transpose()?;
    Ok(OutputRows {
        run: output.run(),
        result,
        captures_complete,
        fields,
        staged,
        files: stored.files,
        presented: stored.presented,
    })
}

struct Offload<'a> {
    output: &'a Output,
    registered: &'a BTreeMap<FieldPointer, CaptureRow>,
    stored: &'a Stored<'a>,
    /// Registered captures the saved document references.
    fields: Vec<(i64, Option<skyhook_tools::output::json::Detection>)>,
    staged: Vec<StagedCapture>,
}

impl Offload<'_> {
    fn visit(&mut self, field: &FieldPointer, value: &mut Value) -> std::io::Result<()> {
        if self.stored.referenced.contains(field)
            && let Some(capture) = self.registered.get(field)
        {
            let detection = match value {
                Value::String(_) if self.stored.candidates.contains(field) => {
                    let source = reader::CaptureReader::new(self.output.reads.clone(), capture.id);
                    Some(classify(
                        skyhook_tools::output::json::detect(source)?,
                        value,
                    ))
                }
                Value::String(_) | Value::Object(_) | Value::Array(_) => {
                    clear(value);
                    None
                }
                _ => return Ok(()),
            };
            self.fields.push((capture.id, detection));
            return Ok(());
        }
        // Never overwrite or adopt an unreferenced raw capture merely because the
        // ordinary result happens to use the same pointer.
        if self.registered.contains_key(field) {
            return self.children(field, value);
        }
        match value {
            Value::String(text) if self.stored.candidates.contains(field) => {
                let detection = skyhook_tools::output::json::detect(text.as_bytes())?;
                // Short text that is not JSON stays inline, unclassified.
                if detection.placeholder().is_none() && text.len() <= OFFLOAD_BYTES {
                    return Ok(());
                }
                let text = std::mem::take(text).into_bytes();
                let detection = classify(detection, value);
                self.store(field, (CaptureKind::Text, Some(detection)), &text)?;
            }
            Value::String(text) if text.len() > OFFLOAD_BYTES => {
                let text = std::mem::take(text).into_bytes();
                self.store(field, (CaptureKind::Text, None), &text)?;
            }
            Value::Object(_) | Value::Array(_) if self.offloadable(field, value) => {
                let bytes = serde_json::to_vec(value)?;
                self.store(field, (CaptureKind::Json, None), &bytes)?;
                clear(value);
            }
            _ => return self.children(field, value),
        }
        Ok(())
    }

    fn children(&mut self, field: &FieldPointer, value: &mut Value) -> std::io::Result<()> {
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
                .chain(self.stored.presented.complete())
                .any(|inner| field.contains(inner))
            && !text_field(value)
    }

    /// Stage `data` as a capture at `field`: in rows, or past the saved-field limit
    /// in a file of its own.
    fn store(
        &mut self,
        field: &FieldPointer,
        kind: (CaptureKind, Option<skyhook_tools::output::json::Detection>),
        data: &[u8],
    ) -> std::io::Result<()> {
        let (run, db) = (self.output.run(), &self.output.db);
        let stage =
            |bytes| (db.stage_capture(run, field, kind, bytes)).map_err(std::io::Error::other);
        let staged = if data.len() as u64 > self.output.spill.bytes {
            let (mut file, path) = self.output.spill_file(self.staged.len())?;
            let extent = CaptureExtent::default().after(data);
            let kept = (file.write_all(data).and_then(|()| file.sync_data()))
                .map_err(|error| SpillError::At(error).into())
                .and_then(|()| stage(StagedBytes::File(&path, extent)));
            // Nothing records a file its capture was not staged with.
            kept.inspect_err(|_| drop(std::fs::remove_file(path.as_str())))?
        } else {
            stage(StagedBytes::Rows(data))?
        };
        self.staged.push(staged);
        Ok(())
    }
}

/// What candidate text holds; a JSON field presents as its empty container.
fn classify(
    detection: skyhook_tools::output::json::Detection,
    value: &mut Value,
) -> skyhook_tools::output::json::Detection {
    *value = detection
        .placeholder()
        .unwrap_or_else(|| Value::String(String::new()));
    detection
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

impl Saved {
    /// The saved result with its stored fields installed; none until the run saved one.
    pub(crate) fn result(&self) -> Result<Option<Value>, ToolError> {
        let Some(product) = &self.product else {
            return Ok(None);
        };
        let mut document = hydrate(self, product.document())?;
        Ok(product.result.is_some().then(|| document["result"].take()))
    }

    /// The result's fields at or under `within` that the job's target holds in files,
    /// of which the session holds only the first bytes.
    pub(super) fn on_target(&self, within: &FieldPointer) -> Held {
        let held = |field: &FieldPointer| match &self.captures.get(field)?.bytes {
            crate::session::CaptureBytes::Target(file) => Some((field.clone(), file.clone())),
            _ => None,
        };
        let fields = self.fields.iter();
        let fields = fields.filter(|field| *field == within || within.contains(field));
        fields.filter_map(held).collect()
    }

    /// This with each of `texts` as its field's value, as if the session held it:
    /// what a read of a field its job's target holds read there.
    pub(super) fn holding(mut self, texts: Vec<(FieldPointer, String)>) -> Self {
        let root = FieldPointer::result();
        for (field, text) in texts {
            self.fields.remove(&field);
            self.captures.remove(&field);
            let result = self
                .product
                .as_mut()
                .and_then(|product| product.result.as_mut());
            let within = field.as_str().strip_prefix(root.as_str());
            if let Some(slot) = (result.zip(within)).and_then(|(result, at)| result.pointer_mut(at))
            {
                *slot = text.into();
            }
        }
        self
    }
}

/// Fields a document holds only the first bytes of, with the files on the job's target
/// that hold them whole.
pub(super) type Held = Vec<(FieldPointer, crate::session::TargetFile)>;

/// Install referenced fields into the document.
pub(super) fn hydrate(saved: &Saved, value: Value) -> Result<Value, ToolError> {
    let root = FieldPointer::root();
    let mut text = stream::json_text(saved, &root, &value, stream::Reading::Whole)?;
    // Parsing bytes in memory is several times faster than through a reader, and
    // the document is held whole either way.
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut text, &mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// The bytes automatic presentation emits for this output, which notification
/// batching budgets: the previewed result, its shape and its truncation records, or
/// a page when it cannot be previewed.
pub(crate) fn presented_size(output: &Output) -> usize {
    let cancellation = crate::job::CancellationToken::new();
    let measure = |saved: &Saved| {
        let projected = projection::project(saved, &cancellation)?;
        let truncated = (!projected.truncated.is_empty()).then_some(&projected.truncated);
        Ok(preview::json_bytes(&projected.result)
            + projected.shape.as_ref().map_or(0, preview::json_bytes)
            + truncated.map_or(0, preview::json_bytes))
    };
    let measured = Saved::load(output).and_then(|saved| saved.reading(measure));
    measured.map_or(PAGE_BYTES, |(_, bytes)| bytes)
}

#[cfg(test)]
impl Output {
    /// What the run saved, read on the writer's connection, as a test reads it
    /// outside the pool's readers.
    pub(crate) fn test_saved(&self) -> Saved {
        let reads = crate::session::Reads::writer(self.db.clone());
        let output = Self {
            reads,
            ..self.clone()
        };
        Saved::load(&output).unwrap()
    }

    /// Register `field` holding `bytes`, as an unfinished producer leaves it,
    /// replacing any capture already there.
    pub(crate) fn test_capture(&self, field: &str, kind: CaptureKind, bytes: &[u8]) {
        let field: FieldPointer = field.parse().unwrap();
        if let Some(existing) = self.test_saved().captures.get(&field) {
            self.db.delete_capture(existing.id).unwrap();
        }
        let mut writer = crate::job::output::create_capture(self, &field, kind)
            .unwrap()
            .open();
        writer.write_all(bytes).unwrap();
        writer.flush().unwrap();
        // Keep the row: dropping a writer only deletes abandoned builtin text captures.
        drop(writer);
    }

    pub(crate) fn test_bytes(&self, field: &str) -> Option<Vec<u8>> {
        let mut source = self.test_saved().held(&field.parse().unwrap())?;
        let mut bytes = Vec::new();
        source.read_to_end(&mut bytes).unwrap();
        Some(bytes)
    }

    /// The compact saved product as `{"result": ...}`, if the job has finished.
    pub(crate) fn test_document(&self) -> Option<Value> {
        (self.test_saved().product).map(|product| product.document())
    }

    pub(crate) fn test_captures_complete(&self) -> Option<bool> {
        (self.test_saved().product).map(|product| product.captures_complete)
    }

    pub(crate) fn test_fields(&self) -> Vec<FieldPointer> {
        self.test_saved().fields.into_iter().collect()
    }

    pub(crate) fn test_delete_capture(&self, capture: &CompletedCapture) {
        self.db.delete_capture(capture.capture_id()).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{JobManager, JobOutcome, output::tests::fixture_with, tests::runtime};
    use crate::tool::ToolOutput;
    use skyhook_tools::output::json::Detection;

    /// The result `output`'s saved document holds.
    fn value(output: &Output) -> Value {
        let saved = output.test_saved();
        hydrate(&saved, saved.product.as_ref().unwrap().document()).unwrap()["result"].take()
    }

    /// Parts of a result past the saved-field limit are kept in files, text and JSON
    /// alike, and stay fields: paged and queried as before, and read whole into the
    /// value a script holds. Host inspection tells where they are kept. Once a file
    /// cannot be read its field shows empty and incomplete, and reading it fails as
    /// unavailable.
    #[tokio::test]
    async fn result_parts_past_the_limit_are_kept_in_files_and_read_as_fields() {
        let runtime = crate::tests::TestRuntime::new()
            .await
            .spilling_past(8 * 1024);
        let (manager, agent) = (&runtime.jobs, &runtime.agent);
        let log: String = (0..2000).map(|line| format!("line {line}\n")).collect();
        let rows: Vec<Value> = (0..2000).map(|row| json!({"row": row})).collect();
        let result = json!({"log": log, "rows": rows, "trace": log, "note": "kept inline"});
        let completed = JobOutcome::Completed(ToolOutput::new(result.clone()).into());
        let id = fixture_with(manager, agent, None, Some(completed)).await;
        let output = manager.output(id).await;
        let saved = Saved::load(&output).unwrap();
        let spills = runtime.root.path().to_str().unwrap();
        let capabilities = CapabilitySet::default();
        let inspect = async || {
            let inspected = manager.inspect_output_with_captures(
                OutputArgs::new(id),
                Default::default(),
                &capabilities,
            );
            inspected.await.unwrap()
        };
        let files = inspect().await.files;
        let [log, rows, trace] = &files[..] else {
            panic!("three fields are kept in files: {files:?}")
        };
        for file in &files {
            let held = std::fs::metadata(file.path.as_str()).unwrap().len();
            assert_eq!((file.bytes, &file.unreadable), (held, &None));
            let host = skyhook_tools::target::TargetRef::Root;
            assert!(file.path.as_str().starts_with(spills) && file.target == host);
        }
        for capture in saved.captures.values() {
            let stored = output.reads.capture_window(capture.id, 0, 1).unwrap().1;
            assert!(matches!(stored, crate::session::Stored::File(_)));
        }
        assert_eq!(saved.result().unwrap(), Some(result));

        let mut args = OutputArgs::new(id);
        (args.field, args.start) = (Some("/result/log".parse().unwrap()), Some(1999));
        let page = manager.present_output(args, &Default::default()).await;
        let page = &page.unwrap()["presentation"]["preview"];
        assert_eq!(page["lines"], json!(["line 1998", "line 1999"]));
        let mut args = OutputArgs::new(id);
        args.field = Some("/result/rows".parse().unwrap());
        args.query = Some("$[?@.row == 1500]".into());
        let found = manager.present_output(args, &Default::default()).await;
        let found = &found.unwrap()["presentation"]["preview"]["matches"];
        assert_eq!(found[0]["value"], json!({"row": 1500}));

        // A file that is gone, cut short or closed to its reader leaves the result
        // readable: its field shows empty and incomplete, the session's user is told
        // why, and reading the field fails as unavailable.
        std::fs::remove_file(log.path.as_str()).unwrap();
        let cut = std::fs::File::options()
            .write(true)
            .open(rows.path.as_str());
        cut.unwrap().set_len(10).unwrap();
        let closed = std::os::unix::fs::PermissionsExt::from_mode(0o000);
        std::fs::set_permissions(trace.path.as_str(), closed).unwrap();
        let whole = manager.present_output(OutputArgs::new(id), &capabilities);
        let whole = whole.await.unwrap();
        let shown = json!({"log": "", "rows": [], "trace": "", "note": "kept inline"});
        assert_eq!(whole["result"], shown);
        let emitted = serde_json::to_vec(&shown).unwrap().len();
        assert_eq!(presented_size(&output), emitted);
        let lost = json!([{"field": "/result/log", "complete": false},
            {"field": "/result/rows", "complete": false},
            {"field": "/result/trace", "complete": false}]);
        assert_eq!(whole["presentation"]["captures"], lost);
        let reasons = inspect().await.files.into_iter();
        let reasons: Vec<_> = reasons.filter_map(|file| file.unreadable).collect();
        let told = ["not found", "shorter than was saved", "permission denied"];
        assert_eq!(reasons, told);
        for (field, kept) in [
            ("/result/log", "/result/log"),
            ("/result/rows/3", "/result/rows"),
            ("/result/trace", "/result/trace"),
        ] {
            let mut args = OutputArgs::new(id);
            args.field = Some(field.parse().unwrap());
            let page = manager.present_output(args, &Default::default()).await;
            let failed = page.unwrap_err().to_string();
            let plain = format!("saved output for {kept} is no longer available");
            assert!(failed.ends_with(&plain), "{failed}");
        }
    }

    /// A part its spill directory cannot keep fails its job, saying why, with no
    /// result saved, and the session goes on to journal the next job. A long key is
    /// kept like any other: files are named by ordinal.
    #[tokio::test]
    async fn a_part_that_cannot_be_kept_fails_its_job_and_the_session_goes_on() {
        let runtime = crate::tests::TestRuntime::new().await;
        let blocked = runtime.root.path().join("blocked");
        std::fs::write(&blocked, b"").unwrap();
        let large = |key: &str| {
            let result = json!({key: "x".repeat(16 * 1024)});
            JobOutcome::Completed(ToolOutput::new(result).into())
        };
        let spilling = |root: Option<&std::path::Path>| {
            let (root, bytes) = (root.map(Into::into), 8 * 1024);
            JobManager::spilling(runtime.store.clone(), Spill { root, bytes })
        };
        let unkept = [
            (
                Some(&*blocked),
                "save failed: not a directory (OS error 20)",
            ),
            (None, "save failed: this user has no cache directory"),
        ];
        for (root, told) in unkept {
            let manager = spilling(root);
            let id = fixture_with(&manager, &runtime.agent, None, Some(large("text"))).await;
            let failed = manager.test_snapshot(id).await.unwrap();
            assert_eq!(failed.state, crate::job::JobState::Failed);
            assert_eq!(failed.output, None);
            let hidden = skyhook_tools::diagnostic::TargetView::Hidden;
            let reason = failed.diagnostic.as_ref().unwrap().render(hidden);
            assert_eq!(reason, told);
        }
        let manager = spilling(Some(&runtime.root.path().join("spills")));
        let long = "k".repeat(300);
        let id = fixture_with(&manager, &runtime.agent, None, Some(large(&long))).await;
        let kept = manager.test_snapshot(id).await.unwrap();
        assert_eq!(kept.output.unwrap()[&long], "x".repeat(16 * 1024));
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
            let id = fixture_with(&manager, &agent, Some(schema.clone()), None).await;
            let output = manager.output(id).await;
            // Candidate text is classified whether it streamed or returned inline;
            // other strings are never parsed.
            let mut result = json!({"note":"{\"x\":1}"});
            let mut receipts = Vec::new();
            if captured {
                let mut writer =
                    crate::job::output::create_capture(&output, &stdout, CaptureKind::Text)
                        .unwrap()
                        .open();
                writer.write_all(text.as_bytes()).unwrap();
                receipts.push(writer.finish().unwrap());
            } else {
                result["stdout"] = text.into();
            }
            let completed = ToolOutput::new(result).with_captures(receipts);
            manager.test_complete(id, completed).await;
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
                (args.field, args.start) = (Some(stdout.clone()), Some(2));
                args.unit = Some(PageUnit::Elements);
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
    ) -> (tempfile::TempDir, JobManager, Saved) {
        let (root, manager, agent) = runtime().await;
        let mut output = ToolOutput::new(result);
        if !captures_complete {
            output.streams = skyhook_tools::output::StreamEnd::Cut;
        }
        let outcome = crate::job::JobOutcome::Completed(output.into());
        let id = fixture_with(&manager, &agent, Some(schema.clone()), Some(outcome)).await;
        let saved = Saved::load(&manager.output(id).await).unwrap();
        (root, manager, saved)
    }

    /// Text whose schema declares JSON content is read as the value it holds, unless
    /// its producer did not finish it.
    #[tokio::test]
    async fn output_a_producer_did_not_finish_is_never_taken_as_json() {
        let schema = json!({"type":"object","properties":{
            "stdout":{"type":"string","contentMediaType":"application/json"},
            "stderr":{"type":"string"}
        }});
        let result = json!({"stdout": "{\"a\":1}", "stderr": "[]"});
        for (finished, stdout) in [(true, json!({"a":1})), (false, json!("{\"a\":1}"))] {
            let (_root, _manager, saved) = saved(&schema, result.clone(), finished).await;
            let cancellation = crate::job::CancellationToken::new();
            let projected = super::projection::project(&saved, &cancellation);
            let projected = projected.unwrap().result.unwrap();
            assert_eq!(projected, json!({"stdout": stdout, "stderr": "[]"}));
        }
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

    /// Notification sizing is exactly what automatic presentation emits: the
    /// previewed result, its shape and its records, escapes included.
    #[tokio::test]
    async fn presentation_size_is_what_automatic_presentation_emits() {
        let (_root, manager, agent) = runtime().await;
        let text = "\u{1}".repeat(3 * CONTENT_BYTES);
        let items: Vec<_> = (0..2000).map(|id| json!({"id": id})).collect();
        let complete = json!({"properties":{"text":{"x-skyhook-preview":"complete"}}});
        for schema in [json!(true), complete] {
            let result = ToolOutput::new(json!({"text": text, "items": items}));
            let outcome = crate::job::JobOutcome::Completed(result.into());
            let id = fixture_with(&manager, &agent, Some(schema), Some(outcome)).await;
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
            assert_eq!(presented_size(&manager.output(id).await), emitted);
        }
    }

    #[tokio::test]
    async fn large_containers_are_stored_whole_unless_something_inside_is_stored_alone() {
        let (_root, manager, agent) = runtime().await;
        let id = fixture_with(&manager, &agent, None, None).await;
        let output = manager.output(id).await;
        let items: Vec<_> = (0..400).map(|id| json!({"id":id,"name":"pod"})).collect();
        let result = json!({
            "items": items,
            // A long string keeps its own capture, so its container stays inline.
            "details": {"log": "l".repeat(OFFLOAD_BYTES + 1), "lines": 1},
            "small": {"a": 1},
        });
        manager
            .test_complete(id, ToolOutput::new(result.clone()))
            .await;
        let saved = Saved::load(&output).unwrap();
        let fields: Vec<_> = saved.fields.iter().map(FieldPointer::as_str).collect();
        assert_eq!(fields, ["/result/details/log", "/result/items"]);
        assert_eq!(value(&output), result);
    }
}
