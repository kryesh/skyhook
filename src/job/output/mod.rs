//! Saved output presentation, independent of model providers and execution transports.
mod captures;
mod finalization;
mod products;
pub(crate) use finalization::save_completed;
pub(crate) use products::{OutputPreview, OutputTruncation};
pub use products::{OutputSelection, PresentedOutput};
mod reader;
pub(crate) use captures::CaptureDescriptor;
pub use captures::CaptureKind;
pub(crate) use captures::{
    AsyncCapture, CaptureWriter, CompletedCapture, PendingCapture, TextCaptureField,
};
pub(crate) use reader::Source;
mod truncation;
use super::{JobError, JobManager, JobRole, JobState, OutputPresentation, views};
use crate::{
    identity::JobId,
    session::{CaptureRow, SharedDb},
    tool::{ToolError, policy::CapabilitySet},
};
use base64::Engine as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Read, Write},
};

// Leave room for a typical 100-line page, plus its JobView envelope.
pub(crate) const CONTENT_BYTES: usize = 32 * 1024;
pub(crate) const PAGE_BYTES: usize = CONTENT_BYTES + 2048;

pub(crate) use truncation::annotated_fields;

/// Omit null-valued object fields from human-only UI display copies.
/// Never use for model, history, saved output, or JavaScript response data.
/// Null array entries and literal strings stay intact to preserve indices and text.
pub fn omit_null_fields(value: &mut Value) {
    match value {
        Value::Object(fields) => fields.retain(|_, value| {
            omit_null_fields(value);
            !value.is_null()
        }),
        Value::Array(values) => values.iter_mut().for_each(omit_null_fields),
        _ => {}
    }
}

fn database(error: crate::session::DbError) -> ToolError {
    ToolError::Io(std::io::Error::other(error))
}

/// One job's output rows in the session database. Every operation locks the
/// connection briefly, so call it off async worker threads for anything large.
#[derive(Clone)]
pub(crate) struct Output {
    db: SharedDb,
    job: JobId,
}

/// A snapshot of the saved document and capture registrations. Capture bytes stay
/// in the database and are read live, so an open capture can still grow.
pub(crate) struct Saved {
    output: Output,
    /// Compact terminal document; referenced fields are emptied placeholders.
    document: Option<Value>,
    /// Pointers the document references, in pointer order.
    fields: Vec<String>,
    captures: BTreeMap<String, CaptureRow>,
}

impl Saved {
    pub(crate) fn load(output: &Output) -> Result<Self, ToolError> {
        let job = output.job.get();
        let saved = output.db.output(job).map_err(database)?;
        let captures = output.db.captures(job).map_err(database)?;
        let (document, fields) = match saved {
            Some((document, fields)) => (Some(serde_json::from_str(&document)?), fields),
            None => (None, Vec::new()),
        };
        Ok(Self {
            output: output.clone(),
            document,
            fields,
            captures: captures
                .into_iter()
                .map(|capture| (capture.pointer.clone(), capture))
                .collect(),
        })
    }

    /// Saved presence is explicit because JSON cannot distinguish `None` from
    /// `Some(null)`.
    fn has_result(&self) -> bool {
        self.document
            .as_ref()
            .is_some_and(|document| document["has_result"] == true)
    }

    /// Any registered capture at `field`, complete or not.
    fn capture(&self, field: &str) -> Option<Source> {
        self.captures.get(field).map(|capture| {
            Source::Capture(reader::CaptureReader::new(
                self.output.db.clone(),
                capture.id,
            ))
        })
    }

    /// The bytes of a referenced field, which the document stores as a placeholder.
    fn stored(&self, field: &str) -> Option<Source> {
        self.fields
            .iter()
            .any(|stored| stored == field)
            .then(|| self.capture(field))
            .flatten()
    }

    /// All bytes of the capture at `field`, if one is registered.
    pub(crate) fn bytes(&self, field: &str) -> Result<Option<Vec<u8>>, ToolError> {
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
    pub fields: BTreeSet<String>,
}

impl ScriptPresentation {
    fn load(output: &Output) -> Result<Self, ToolError> {
        let rows = output.db.presentation(output.job.get()).map_err(database)?;
        Ok(Self {
            fields: rows.fields.into_iter().collect(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutputArgs {
    pub job: JobId,
    /// JSON Pointer in saved content, e.g. /result/stdout, /result/content, /result/console.
    pub field: Option<String>,
    /// One-based first source line; use returned next_start to continue.
    #[schemars(range(min = 1), extend("default" = 1))]
    pub start: Option<usize>,
    /// Maximum returned lines, including match context.
    #[schemars(range(min = 1, max = 1000), extend("default" = 100))]
    pub limit: Option<usize>,
    /// Case-sensitive line regex; use (?i) for case-insensitive matching.
    pub pattern: Option<String>,
    /// Surrounding lines per match.
    #[schemars(range(min = 0, max = 20), extend("default" = 0))]
    pub context: Option<usize>,
    /// Zero-based UTF-8 byte offset within the starting line; use returned next_offset to continue.
    #[schemars(range(min = 0), extend("default" = 0))]
    pub offset: Option<usize>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) cancellation: Option<super::CancellationToken>,
}

impl OutputArgs {
    pub fn new(job: JobId) -> Self {
        Self {
            job,
            field: None,
            start: None,
            limit: None,
            pattern: None,
            context: None,
            offset: None,
            cancellation: None,
        }
    }
}
#[derive(Clone)]
struct Selection {
    field: String,
    matcher: Option<std::sync::Arc<grep_regex::RegexMatcher>>,
    context: usize,
    start: usize,
    offset: usize,
}

/// The JSON Pointer of object member `key` under `field`.
fn property_field(field: &str, key: &str) -> String {
    format!("{field}/{}", key.replace('~', "~0").replace('/', "~1"))
}

/// Run blocking output work off the async workers.
pub(super) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ToolError> + Send + 'static,
) -> Result<T, ToolError> {
    let joined = tokio::task::spawn_blocking(work).await;
    joined.map_err(|error| ToolError::Failed(error.to_string()))?
}

/// Persist a compact document whose referenced and large strings live in captures.
/// `referenced` names completed captures the document installs; other strings over
/// 4 KiB are offloaded into new text captures, unless an unreferenced raw capture
/// already owns that pointer.
fn save_document(
    output: &Output,
    document: &Value,
    referenced: &BTreeSet<String>,
) -> Result<(), ToolError> {
    let registered = output
        .db
        .captures(output.job.get())
        .map_err(database)?
        .into_iter()
        .map(|capture| (capture.pointer, capture.id))
        .collect::<BTreeMap<_, _>>();
    let mut document = document.clone();
    let mut fields = Vec::new();
    fn visit(
        output: &Output,
        registered: &BTreeMap<String, i64>,
        referenced: &BTreeSet<String>,
        field: &str,
        value: &mut Value,
        fields: &mut Vec<i64>,
    ) -> Result<(), ToolError> {
        if referenced.contains(field)
            && let Some(&capture) = registered.get(field)
        {
            match value {
                Value::String(text) => text.clear(),
                Value::Object(map) => map.clear(),
                Value::Array(items) => items.clear(),
                _ => return Ok(()),
            }
            fields.push(capture);
            return Ok(());
        }
        match value {
            // Do not overwrite or adopt an unreferenced raw capture merely
            // because the ordinary result happens to use the same pointer.
            Value::String(text) if text.len() > 4096 && !registered.contains_key(field) => {
                let mut writer = PendingCapture::create(output, field, CaptureKind::Text)?.open();
                writer.write_all(text.as_bytes())?;
                fields.push(writer.finish()?.capture_id());
                text.clear();
            }
            Value::Object(map) => {
                for (key, value) in map {
                    let child = property_field(field, key);
                    visit(output, registered, referenced, &child, value, fields)?;
                }
            }
            Value::Array(items) => {
                for (index, value) in items.iter_mut().enumerate() {
                    let child = format!("{field}/{index}");
                    visit(output, registered, referenced, &child, value, fields)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    visit(
        output,
        &registered,
        referenced,
        "",
        &mut document,
        &mut fields,
    )?;
    output
        .db
        .save_output(
            output.job.get(),
            &serde_json::to_string(&document)?,
            &fields,
        )
        .map_err(database)
}

/// Install referenced fields no larger than `maximum` bytes into the document.
fn hydrate(saved: &Saved, mut value: Value, maximum: u64) -> Result<Value, ToolError> {
    for field in &saved.fields {
        if saved
            .captures
            .get(field)
            .is_some_and(|capture| capture.bytes > maximum)
        {
            continue;
        }
        hydrate_field(saved, &mut value, field)?;
    }
    Ok(value)
}

fn hydrate_field(saved: &Saved, value: &mut Value, field: &str) -> Result<(), ToolError> {
    let target = value
        .pointer_mut(field)
        .ok_or_else(|| ToolError::Failed("invalid saved output field".into()))?;
    load_field(saved, target, field)
}

/// Replace a stored field's placeholder with its bytes.
fn load_field(saved: &Saved, target: &mut Value, field: &str) -> Result<(), ToolError> {
    let bytes = saved
        .bytes(field)?
        .ok_or_else(|| ToolError::Failed("saved output field is missing".into()))?;
    *target = if target.is_string() {
        Value::String(String::from_utf8_lossy(&bytes).into_owned())
    } else {
        serde_json::from_slice(&bytes)?
    };
    Ok(())
}

fn render(
    saved: &Saved,
    field: &str,
    value: &Value,
    out: &mut impl Write,
    cancellation: &super::CancellationToken,
) -> Result<(), ToolError> {
    if cancellation.is_cancelled() {
        return Err(ToolError::Cancelled);
    }
    if let Some(mut source) = saved.stored(field) {
        if !value.is_string() {
            std::io::copy(&mut source, out)?;
            return Ok(());
        }
        out.write_all(b"\"")?;
        let mut input = BufReader::new(source);
        // Escape a stored string incrementally rather than hydrating it.
        loop {
            if cancellation.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let bytes = input.fill_buf()?;
            if bytes.is_empty() {
                break;
            }
            for byte in bytes {
                match byte {
                    b'"' => out.write_all(b"\\\"")?,
                    b'\\' => out.write_all(b"\\\\")?,
                    0..=31 => write!(out, "\\u{:04x}", byte)?,
                    _ => out.write_all(&[*byte])?,
                }
            }
            let length = bytes.len();
            input.consume(length);
        }
        out.write_all(b"\"")?;
        return Ok(());
    }
    match value {
        Value::Object(map) => {
            out.write_all(b"{\n")?;
            for (index, (key, value)) in map.iter().enumerate() {
                if index > 0 {
                    out.write_all(b",\n")?;
                }
                serde_json::to_writer(&mut *out, key)?;
                out.write_all(b": ")?;
                render(saved, &property_field(field, key), value, out, cancellation)?;
            }
            out.write_all(b"\n}")?;
        }
        Value::Array(items) => {
            out.write_all(b"[\n")?;
            for (index, value) in items.iter().enumerate() {
                if index > 0 {
                    out.write_all(b",\n")?;
                }
                render(saved, &format!("{field}/{index}"), value, out, cancellation)?;
            }
            out.write_all(b"\n]")?;
        }
        _ => serde_json::to_writer(out, value)?,
    }
    Ok(())
}

/// Who reads a presentation. Model-facing reads acknowledge the job's pending
/// notification; host inspection never does and always shows details.
#[derive(Clone, Copy)]
pub(crate) enum OutputOptions {
    Host { presentation: OutputPresentation },
    Model { presentation: OutputPresentation },
}

impl OutputOptions {
    pub(crate) const HOST: Self = Self::Host {
        presentation: OutputPresentation::Full,
    };
}

impl JobManager {
    pub(crate) async fn save_script_presentation(
        &self,
        job: JobId,
        presentation: ScriptPresentation,
    ) -> Result<(), ToolError> {
        if presentation.fields.is_empty() {
            return Ok(());
        }
        let output = self.output(job);
        let rows = crate::session::Presentation {
            fields: presentation.fields.into_iter().collect(),
        };
        let job = output.job.get();
        blocking(move || output.db.save_presentation(job, &rows).map_err(database)).await
    }

    pub(crate) async fn output_changed(&self, id: JobId) {
        if let Some(entry) = self.inner.jobs.lock().await.get(&id) {
            entry.notify.notify_waiters();
        }
    }

    pub(crate) fn output(&self, id: JobId) -> Output {
        Output {
            db: self.inner.store.outputs(),
            job: id,
        }
    }

    #[cfg(test)]
    pub(crate) async fn present_output(
        &self,
        args: OutputArgs,
        capabilities: &CapabilitySet,
    ) -> Result<Value, ToolError> {
        self.present_output_with(
            args,
            capabilities,
            OutputOptions::Model {
                presentation: crate::job::OutputPresentation::Full,
            },
        )
        .await
        .map(PresentedOutput::into_view)
    }

    /// Host inspection never acknowledges an agent's pending notification.
    /// Stored captures are listed as `captures: [{field, kind, complete}]`; an
    /// incomplete capture is exposed only as raw pages, never as a result.
    pub async fn inspect_output(
        &self,
        args: OutputArgs,
        capabilities: &CapabilitySet,
    ) -> Result<Value, ToolError> {
        self.present_output_with(args, capabilities, OutputOptions::HOST)
            .await
            .map(PresentedOutput::into_view)
    }

    /// Host UI field discovery uses the saved tree rather than presentation
    /// wrappers, previews, or capture descriptors.
    pub async fn inspect_output_fields(&self, job: JobId) -> Result<Vec<String>, ToolError> {
        let terminal = self
            .metadata(job)
            .await
            .map_err(|error| ToolError::Failed(error.to_string()))?
            .state
            .is_terminal();
        let output = self.output(job);
        blocking(move || {
            fn visit(value: &Value, pointer: String, paths: &mut Vec<String>) {
                paths.push(pointer.clone());
                match value {
                    Value::Object(object) => {
                        for (key, value) in object {
                            visit(value, property_field(&pointer, key), paths);
                        }
                    }
                    Value::Array(array) => {
                        for (index, value) in array.iter().enumerate() {
                            visit(value, format!("{pointer}/{index}"), paths);
                        }
                    }
                    _ => {}
                }
            }
            let mut paths = Vec::new();
            let saved = Saved::load(&output)?;
            if terminal && let Some(mut document) = saved.document.clone() {
                // Containers have selectable descendants; large text captures
                // do not need to be loaded merely to enumerate their pointers.
                for field in &saved.fields {
                    if document
                        .pointer(field)
                        .is_some_and(|value| value.is_object() || value.is_array())
                    {
                        hydrate_field(&saved, &mut document, field)?;
                    }
                }
                if let Some(result) = document.get("result") {
                    visit(result, "/result".into(), &mut paths);
                }
            }
            for capture in captures::available_captures(&saved, terminal) {
                if !paths.contains(&capture.field) {
                    paths.push(capture.field);
                }
            }
            Ok(paths)
        })
        .await
    }

    /// Host inspection with automatic pages for captures absent from the whole
    /// presentation. Any explicit selection (including `context: 0`) suppresses
    /// hydration, and a present JSON null is not an absent capture. A page
    /// failure is embedded as `{error}` in that capture's `output`.
    pub async fn inspect_output_with_captures(
        &self,
        args: OutputArgs,
        capabilities: &CapabilitySet,
    ) -> Result<PresentedOutput, ToolError> {
        use futures_util::{StreamExt, stream};

        let mut output = self
            .present_output_with(args.clone(), capabilities, OutputOptions::HOST)
            .await?;
        let mut pages = stream::iter(std::mem::take(&mut output.capture_targets))
            .map(|(index, field)| {
                let mut query = OutputArgs::new(args.job);
                query.field = Some(field);
                query.cancellation = args.cancellation.clone();
                async move {
                    let page = self
                        .present_output_with(query, capabilities, OutputOptions::HOST)
                        .await;
                    (index, page)
                }
            })
            .buffer_unordered(4);
        while let Some((index, page)) = pages.next().await {
            output.attach_capture(index, page);
        }
        Ok(output)
    }

    /// Return the state and presentation from the same job snapshot. Explicit
    /// field/page queries always retain full output, even under Automatic policy.
    pub(crate) async fn present_output_with(
        &self,
        args: OutputArgs,
        capabilities: &CapabilitySet,
        options: OutputOptions,
    ) -> Result<PresentedOutput, ToolError> {
        let (acknowledge, presentation) = match options {
            OutputOptions::Host { presentation } => (false, presentation),
            OutputOptions::Model { presentation } => (true, presentation),
        };
        let limit = args.limit.unwrap_or(100);
        if !(1..=1000).contains(&limit) || args.start == Some(0) || args.context.unwrap_or(0) > 20 {
            return Err(ToolError::InvalidArguments(
                "limit must be 1-1000, start positive, and context 0-20".into(),
            ));
        }
        if args.context.unwrap_or(0) > 0 && args.pattern.is_none() {
            return Err(ToolError::InvalidArguments(
                "context requires pattern".into(),
            ));
        }
        let output_selection = args.selection();
        let explicit = output_selection == OutputSelection::Explicit;
        let mut selection = Selection {
            field: args.field.clone().unwrap_or_else(|| "/result".into()),
            matcher: None,
            context: args.context.unwrap_or(0),
            start: args.start.unwrap_or(1),
            offset: args.offset.unwrap_or(0),
        };
        if !selection.field.is_empty() && !selection.field.starts_with('/') {
            return Err(ToolError::InvalidArguments(
                "field must be a JSON Pointer".into(),
            ));
        }
        // Validate the pattern even when no output exists yet.
        if let Some(pattern) = &args.pattern {
            selection.matcher = Some(std::sync::Arc::new(
                crate::tool::builtins::search::output_matcher(pattern)?,
            ));
        }
        let (mut envelope, output_schema, last_message, images) = {
            let jobs = self.inner.jobs.lock().await;
            let entry = jobs
                .get(&args.job)
                .ok_or_else(|| ToolError::Failed(format!("unknown job {}", args.job)))?;
            (
                entry.envelope(args.job),
                entry.output_schema.clone().unwrap_or(Value::Bool(true)),
                // A background child's reply arrived as an event; a foreground
                // child's is its result.
                entry.last_agent_message.filter(|_| {
                    !explicit
                        && presentation == OutputPresentation::Automatic
                        && entry.role == JobRole::Agent
                        && entry.state == JobState::Completed
                        && views::effectively_background(&jobs, args.job)
                }),
                if output_selection == OutputSelection::WholeWithImages {
                    entry.images.clone()
                } else {
                    Vec::new()
                },
            )
        };
        if last_message.is_some() {
            envelope.output = None;
            let mut view = envelope.metadata_view(capabilities);
            view.meta.as_mut().expect("metadata view").last_message = last_message;
            if acknowledge {
                self.claim(args.job)
                    .await
                    .map_err(|error| ToolError::Failed(error.to_string()))?;
            }
            return Ok(PresentedOutput {
                state: envelope.state,
                view: view.into_value(),
                images,
                capture_targets: Vec::new(),
            });
        }
        let terminal = envelope.state.is_terminal();
        let question = envelope.state == JobState::WaitingInput;
        let live_question = envelope.output.take();
        let mut view = if !acknowledge || explicit || presentation == OutputPresentation::Full {
            envelope.metadata_view(capabilities)
        } else {
            envelope.response_view(capabilities)
        };
        let mut annotations = views::Presentation::default();
        let output = self.output(args.job);
        let saved = blocking(move || Saved::load(&output).map(std::sync::Arc::new)).await?;
        let captures = captures::available_captures(&saved, terminal);
        let incomplete_capture = terminal
            && captures.iter().any(|capture| {
                !capture.complete
                    && (!explicit
                        || selection.field.is_empty()
                        || capture.field == selection.field
                        || capture
                            .field
                            .strip_prefix(&selection.field)
                            .is_some_and(|suffix| suffix.starts_with('/')))
            });
        let structured = !explicit && terminal && saved.document.is_some();
        let mut presented_question = false;
        let mut question_page = None;
        if structured {
            let presentation_output = saved.output.clone();
            let script_presentation =
                blocking(move || ScriptPresentation::load(&presentation_output)).await?;
            let projected = saved.clone();
            let cancellation = args.cancellation.clone().unwrap_or_default();
            let projected = blocking(move || {
                truncation::project(
                    &projected,
                    &output_schema,
                    &cancellation,
                    &script_presentation.fields,
                )
            })
            .await?;
            view.result = projected.result;
            annotations.truncated = projected.truncated;
            annotations.notice = projected.notice;
            // A terminal document may contain only an error/capture inventory.
            // Result presence, not document presence or non-nullness, establishes
            // availability (an explicitly stored null is still a real result).
            view.has_result = saved.has_result();
        } else if question && let Some(value) = live_question {
            if !explicit {
                annotations.question = Some(value);
                presented_question = true;
            } else {
                let bytes = serde_json::to_vec_pretty(&value)?;
                let key =
                    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(&bytes));
                let field = format!("/questions/{key}");
                if args.field.is_none() {
                    selection.field = field.clone();
                }
                presented_question = selection.field == field;
                if presented_question {
                    question_page = Some(bytes);
                }
            }
        }
        if !structured && annotations.question.is_none() {
            if !explicit && !question && saved.document.is_some() {
                selection.field = String::new();
            }
            let paged = saved.clone();
            let c = selection.clone();
            let cancellation = args.cancellation.clone().unwrap_or_default();
            // A whole-result query always resolves to "/result" or "" here.
            let unavailable =
                !terminal && !question && (c.field == "/result" || c.field.is_empty());
            let page = if unavailable {
                reader::empty(&c, None, false)
            } else {
                blocking(move || {
                    let closed = terminal || c.field.starts_with("/questions/");
                    let source = match question_page {
                        Some(bytes) => Some(Source::Memory(std::io::Cursor::new(bytes))),
                        None => field_source(&paged, &c.field, &cancellation)?,
                    };
                    reader::page(source, &c, limit, closed, &cancellation)
                })
                .await?
            };
            if terminal {
                let complete = saved
                    .document
                    .as_ref()
                    .and_then(|document| document.get("capture_complete"))
                    .and_then(Value::as_bool);
                if complete == Some(false) {
                    annotations.notice = Some("Output incomplete.".into());
                }
            }
            annotations.preview = Some(page);
        }
        if incomplete_capture {
            annotations.notice = Some("Output incomplete.".into());
        }
        if acknowledge
            && ((terminal && !selection.field.starts_with("/questions/")) || presented_question)
        {
            self.claim(args.job)
                .await
                .map_err(|e| ToolError::Failed(e.to_string()))?;
        }
        // An unavailable result gets a public null placeholder, but an explicit
        // payload null remains authoritative. Hydration targets come from
        // discovered records, never decoded wire descriptors.
        let mut capture_targets: Vec<_> = captures
            .iter()
            .enumerate()
            .filter(|_| output_selection == OutputSelection::WholeWithImages)
            .map(|(index, capture)| (index, capture.field.clone()))
            .collect();
        annotations.captures = captures;
        view.presentation = annotations.into_option();
        let view = view.into_value();
        capture_targets.retain(|(_, field)| {
            view.pointer(field).is_none() || (field == "/result" && view["has_result"] == false)
        });
        Ok(PresentedOutput {
            state: envelope.state,
            view,
            images,
            capture_targets,
        })
    }

    pub(crate) async fn hydrate_envelope(
        &self,
        envelope: &mut super::JobEnvelope,
    ) -> Result<(), JobError> {
        self.hydrate_envelope_up_to(envelope, u64::MAX).await
    }

    pub(crate) async fn hydrate_envelope_up_to(
        &self,
        envelope: &mut super::JobEnvelope,
        maximum: u64,
    ) -> Result<(), JobError> {
        if !envelope.state.is_terminal() {
            return Ok(());
        }
        let output = self.output(envelope.id);
        let value = blocking(move || {
            let saved = Saved::load(&output)?;
            let has_result = saved.has_result();
            saved
                .document
                .clone()
                .map(|document| hydrate(&saved, document, maximum).map(|value| (has_result, value)))
                .transpose()
        })
        .await
        .map_err(|e| JobError::Internal(e.to_string()))?;
        if let Some((has_result, mut value)) = value {
            envelope.output = if has_result {
                value.get_mut("result").map(Value::take)
            } else {
                None
            };
        }
        Ok(())
    }
}

/// The pageable bytes of `field`: a registered capture, or a rendering of the value
/// the saved document holds there. `None` when neither exists yet.
fn field_source(
    saved: &Saved,
    field: &str,
    cancellation: &super::CancellationToken,
) -> Result<Option<Source>, ToolError> {
    if let Some(source) = saved.capture(field) {
        return Ok(Some(source));
    }
    let Some(mut document) = saved.document.clone() else {
        return Ok(None);
    };
    if document.pointer(field).is_none() {
        // Stored containers hide their descendants in the compact document.
        // Only load the selected ancestor, not unrelated large output fields.
        if let Some(ancestor) = saved.fields.iter().find(|stored| {
            field
                .strip_prefix(stored.as_str())
                .is_some_and(|suffix| suffix.starts_with('/'))
        }) {
            hydrate_field(saved, &mut document, ancestor)?;
        }
    }
    if field.is_empty() {
        // Whole-output pages contain the public document, not internal capture metadata.
        let output = document.as_object_mut().expect("saved output document");
        output.remove("capture_complete");
    }
    let value = document
        .pointer(field)
        .ok_or_else(|| ToolError::InvalidArguments("field does not exist in this result".into()))?;
    materialize_field(saved, field, value, cancellation).map(Some)
}

// Projection already owns the resolved value; keep the same renderer for stable
// pagination positions.
fn materialize_field(
    saved: &Saved,
    field: &str,
    value: &Value,
    cancellation: &super::CancellationToken,
) -> Result<Source, ToolError> {
    if let Some(source) = saved.stored(field) {
        return Ok(source);
    }
    let write = |out: &mut dyn Write| -> Result<(), ToolError> {
        match value.as_str() {
            Some(text) => Ok(out.write_all(text.as_bytes())?),
            None => render(saved, field, value, &mut &mut *out, cancellation),
        }
    };
    // Only a value enclosing stored fields can be large; render it to the
    // database once rather than into memory on every page.
    let encloses_stored = saved.fields.iter().any(|stored| {
        field.is_empty()
            || stored
                .strip_prefix(field)
                .is_some_and(|suffix| suffix.starts_with('/'))
    });
    let db = &saved.output.db;
    let job = saved.output.job.get();
    if encloses_stored {
        if let Some(capture) = db.rendering(job, field).map_err(database)? {
            return Ok(Source::Capture(reader::CaptureReader::new(
                db.clone(),
                capture,
            )));
        }
        // A concurrent page may be rendering it; fall back to memory then.
        if let Ok(pending) = PendingCapture::rendering(&saved.output, field) {
            let mut writer = pending.open();
            write(&mut writer)?;
            let capture = writer.finish()?.capture_id();
            return Ok(Source::Capture(reader::CaptureReader::new(
                db.clone(),
                capture,
            )));
        }
    }
    let mut bytes = Vec::new();
    write(&mut bytes)?;
    Ok(Source::Memory(std::io::Cursor::new(bytes)))
}

/// Upper bound on the bytes automatic presentation would emit for this output, used
/// to budget notification batches. Inline content counts whole; a capture-backed
/// field counts as at most a page (`bytes * 6` covers JSON escaping).
pub(crate) fn presentation_size(output: &Output) -> usize {
    let Ok(saved) = Saved::load(output) else {
        return PAGE_BYTES;
    };
    let Some(document) = &saved.document else {
        return PAGE_BYTES;
    };
    let compact = serde_json::to_vec(document).map_or(PAGE_BYTES, |bytes| bytes.len());
    saved.fields.iter().fold(compact, |total, field| {
        total.saturating_add(saved.captures.get(field).map_or(PAGE_BYTES, |capture| {
            usize::try_from(capture.bytes)
                .unwrap_or(PAGE_BYTES)
                .saturating_mul(6)
                .min(PAGE_BYTES)
        }))
    })
}

/// Captures a remote worker streams separately from its result frame.
pub(crate) fn transfer_fields(
    output: &Output,
) -> Result<Vec<(String, CaptureKind, Source)>, ToolError> {
    let saved = Saved::load(output)?;
    Ok(captures::available_captures(&saved, true)
        .into_iter()
        .filter(|capture| {
            // Unreferenced/unfinished captures have no result value to carry
            // their bytes, even when they fit in a normal result frame.
            !capture.complete || saved.captures[&capture.field].bytes > PAGE_BYTES as u64
        })
        .filter_map(|capture| {
            let source = saved.capture(&capture.field)?;
            Some((capture.field, capture.kind, source))
        })
        .collect())
}

#[cfg(test)]
impl Output {
    /// Register `field` holding `bytes`, as an unfinished producer leaves it,
    /// replacing any capture already there.
    pub(crate) fn test_capture(&self, field: &str, kind: CaptureKind, bytes: &[u8]) {
        if let Some(existing) = Saved::load(self).unwrap().captures.get(field) {
            self.db.delete_capture(existing.id).unwrap();
        }
        let mut writer = PendingCapture::create(self, field, kind).unwrap().open();
        writer.write_all(bytes).unwrap();
        writer.flush().unwrap();
        // Keep the row: dropping a writer only deletes abandoned builtin text captures.
        drop(writer);
    }

    pub(crate) fn test_bytes(&self, field: &str) -> Option<Vec<u8>> {
        Saved::load(self).unwrap().bytes(field).unwrap()
    }

    /// The compact saved document, if the job has finished.
    pub(crate) fn test_document(&self) -> Option<Value> {
        Saved::load(self).unwrap().document
    }

    pub(crate) fn test_fields(&self) -> Vec<String> {
        Saved::load(self).unwrap().fields
    }

    pub(crate) fn test_delete_capture(&self, capture: &CompletedCapture) {
        self.db.delete_capture(capture.capture_id()).unwrap();
    }
}

/// A running job's output for synchronous tests, with the runtime that owns it.
#[cfg(test)]
pub(crate) struct TestOutput {
    pub output: Output,
    _manager: JobManager,
    _root: tempfile::TempDir,
    _runtime: tokio::runtime::Runtime,
}

#[cfg(test)]
impl TestOutput {
    pub(crate) fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (root, manager, job) = runtime.block_on(tests::fixture(None));
        Self {
            output: manager.output(job),
            _manager: manager,
            _root: root,
            _runtime: runtime,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{JobOutcome, JobSpec, tests::runtime};

    /// A running fixture job, finished with `value` when one is given.
    pub(super) async fn fixture(value: Option<Value>) -> (tempfile::TempDir, JobManager, JobId) {
        let (root, manager, agent) = runtime().await;
        let id = manager
            .test_lease(JobSpec::test(agent, "fixture"))
            .await
            .id();
        manager.transition(id, JobState::Running).await.unwrap();
        if let Some(value) = value {
            manager.test_finish(id, value).await;
        }
        (root, manager, id)
    }

    fn field_args(id: JobId, field: &str) -> OutputArgs {
        let mut args = OutputArgs::new(id);
        args.field = Some(field.into());
        args
    }

    async fn host(manager: &JobManager, args: OutputArgs) -> Value {
        manager
            .inspect_output(args, &Default::default())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn script_returned_json_is_not_replaced_by_child_output() {
        let (_root, manager, agent) = runtime().await;
        let mut script = JobSpec::test(agent.clone(), "script");
        script.role = JobRole::Script;
        let script = manager.test_create(script).await;
        manager.transition(script, JobState::Running).await.unwrap();

        let mut child = JobSpec::test(agent, "read");
        child.parent = Some(script);
        let child = manager.test_create(child).await;
        manager.transition(child, JobState::Running).await.unwrap();
        manager.test_finish(child, json!({"child":"native"})).await;

        let returned = json!({"kind":"script-value", "child_id":child.get()});
        manager
            .test_finish(script, json!({"value":returned.clone()}))
            .await;

        let view = host(&manager, OutputArgs::new(script)).await;
        assert_eq!(view["result"]["value"], returned);
        assert_eq!(view["has_result"], true);
    }

    async fn model(manager: &JobManager, args: OutputArgs) -> Value {
        manager
            .present_output(args, &Default::default())
            .await
            .unwrap()
    }

    fn incomplete(field: &str, kind: &str) -> Value {
        json!([{ "field":field, "kind":kind, "complete":false, "output":null }])
    }

    /// Any explicit selector, including an explicitly supplied default, also
    /// suppresses automatic capture hydration.
    #[tokio::test]
    async fn output_product_owns_image_eligibility_and_typed_page() {
        use crate::{
            media::{BlobRef, ImageFormat, ImageRef},
            tool::ToolOutput,
        };
        let (_root, manager, id) = fixture(None).await;
        let blob: BlobRef = manager.store().store_blob(b"image").await.unwrap();
        let images = ["first", "second"]
            .map(|name| ImageRef {
                file: Some(name.into()),
                format: ImageFormat::Png,
                blob,
            })
            .to_vec();
        let output = ToolOutput::new(json!({"text": "first\nsecond"})).with_images(images.clone());
        manager
            .finish(id, JobOutcome::Completed(output))
            .await
            .unwrap();
        manager
            .output(id)
            .test_capture("/result/raw", CaptureKind::Text, b"line");
        let whole = manager
            .present_output_with(
                OutputArgs::new(id),
                &Default::default(),
                OutputOptions::HOST,
            )
            .await
            .unwrap();
        assert_eq!((whole.state, &whole.images), (JobState::Completed, &images));
        assert_eq!(
            OutputArgs::new(id).selection(),
            OutputSelection::WholeWithImages
        );
        assert!(whole.view()["presentation"]["preview"].is_null());
        for field in ["field", "start", "limit", "pattern", "context", "offset"] {
            let mut args = OutputArgs::new(id);
            match field {
                "field" => args.field = Some("/result/text".into()),
                "start" => args.start = Some(1),
                "limit" => args.limit = Some(100),
                "pattern" => args.pattern = Some(String::new()),
                "context" => args.context = Some(0),
                "offset" => args.offset = Some(0),
                _ => unreachable!(),
            }
            let selection = args.selection();
            let product = manager
                .inspect_output_with_captures(args, &Default::default())
                .await
                .unwrap();
            assert!(
                product.images.is_empty(),
                "explicit {field} attached images"
            );
            // `context` without a pattern selects nothing: the whole result, without images.
            let whole = field == "context";
            let expected = if whole {
                OutputSelection::Whole
            } else {
                OutputSelection::Explicit
            };
            assert_eq!(selection, expected);
            assert_eq!(
                product.view()["presentation"]["preview"].is_object(),
                !whole,
                "{field}"
            );
            assert_eq!(
                product.view()["presentation"]["captures"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
            assert!(product.view()["presentation"]["captures"][0]["output"].is_null());
        }
        assert_eq!(manager.images(id).await.unwrap(), images);
    }

    #[tokio::test]
    async fn presentation_preserves_nulls_defaults_and_saved_result_presence() {
        for raw in [
            Value::Null,
            json!({
                "error": null,
                "nested": {"absent": null, "ok": false},
                "array": [null, {"absent": null, "count": 0}]
            }),
        ] {
            let (_root, manager, id) = fixture(Some(raw.clone())).await;
            for view in [
                model(&manager, OutputArgs::new(id)).await,
                host(&manager, OutputArgs::new(id)).await,
            ] {
                assert_eq!(view.get("result"), Some(&raw));
                assert_eq!(view["has_result"], true);
                assert_eq!(view["presentation"], Value::Null);
            }
            assert!(host(&manager, OutputArgs::new(id)).await["meta"].is_object());
            assert_eq!(manager.output(id).test_document().unwrap()["result"], raw);
            let metadata = manager
                .metadata(id)
                .await
                .unwrap()
                .metadata_view(&Default::default())
                .into_value();
            assert_eq!(metadata["has_result"], false);
        }
    }

    /// Unfinished captures, whether abandoned by a successful result, a failure,
    /// or cancellation, remain discoverable, readable, and marked incomplete.
    #[tokio::test]
    async fn incomplete_captures_remain_discoverable_and_readable() {
        for outcome in ["failed", "cancelled", "unreferenced"] {
            let (_root, manager, id) = fixture(None).await;
            let field = if outcome == "unreferenced" {
                "/result/abandoned"
            } else {
                "/result/events~1custom/nested~0key"
            };
            assert_eq!(
                host(&manager, OutputArgs::new(id)).await["presentation"]["captures"],
                json!([])
            );
            let bytes = "{\"partial\":"; // Deliberately unfinished JSON.
            let output = manager.output(id);
            output.test_capture(field, CaptureKind::Json, bytes.as_bytes());
            for terminal in [false, true] {
                if terminal {
                    let outcome = match outcome {
                        "cancelled" => JobOutcome::Cancelled,
                        "failed" => JobOutcome::Failed {
                            message: "injected failure".into(),
                            output: None,
                            denial: None,
                        },
                        _ => JobOutcome::Completed(crate::tool::ToolOutput::new(
                            json!({"abandoned":null}),
                        )),
                    };
                    manager.finish(id, outcome).await.unwrap();
                }
                let view = host(&manager, OutputArgs::new(id)).await;
                let page = host(&manager, field_args(id, field)).await;
                assert_eq!(view["presentation"]["captures"], incomplete(field, "json"));
                assert_eq!(
                    page["presentation"]["preview"]["lines"],
                    json!([bytes]),
                    "{outcome}"
                );
                if outcome == "unreferenced" && terminal {
                    assert_eq!(view["presentation"]["notice"], "Output incomplete.");
                    assert_eq!(page["presentation"]["notice"], "Output incomplete.");
                } else {
                    assert!(view["result"].is_null(), "{outcome}");
                }
            }
            assert_eq!(output.test_bytes(field).unwrap(), bytes.as_bytes());
        }
    }

    #[tokio::test]
    async fn persisted_presence_distinguishes_null_results_from_missing_failure_output() {
        for (state, available) in [
            (JobState::Completed, true),
            (JobState::Failed, true),
            (JobState::Failed, false),
            (JobState::Cancelled, false),
        ] {
            let output = available.then(|| crate::tool::ToolOutput::new(Value::Null));
            let outcome = match state {
                JobState::Completed => JobOutcome::Completed(output.unwrap()),
                JobState::Failed => JobOutcome::Failed {
                    message: "failure".into(),
                    output,
                    denial: None,
                },
                _ => JobOutcome::Cancelled,
            };
            let (_root, manager, id) = fixture(None).await;
            manager.finish(id, outcome).await.unwrap();
            let view = model(&manager, OutputArgs::new(id)).await;
            assert_eq!(view["has_result"], available);
            assert_eq!(view.get("result"), Some(&Value::Null));
            // Option<Value> in older journal decoding cannot retain Some(null).
            // Hydration must recover availability from the saved presence bit.
            let mut envelope = manager.snapshot(id).await.unwrap();
            envelope.output = None;
            manager.hydrate_envelope(&mut envelope).await.unwrap();
            assert_eq!(envelope.output.is_some(), available);
        }
    }

    #[tokio::test]
    async fn script_console_capture_can_be_paged_while_running_and_after_cancellation() {
        let (_root, manager, agent) = runtime().await;
        let mut spec = JobSpec::test(agent, "script");
        spec.output_schema = Some(json!({"type":"object","properties":{
            "value":{}, "console":{"type":"string","x-skyhook-truncatable":true}
        }}));
        let id = manager.test_create(spec).await;
        manager.transition(id, JobState::Running).await.unwrap();
        let console = "console\n".repeat(150);
        manager
            .output(id)
            .test_capture("/result/console", CaptureKind::Text, console.as_bytes());
        let mut query = field_args(id, "/result/console");
        query.start = Some(101);
        let lines = |page: &Value| {
            page["presentation"]["preview"]["lines"]
                .as_array()
                .unwrap()
                .len()
        };
        assert_eq!(lines(&model(&manager, query.clone()).await), 50);
        manager.finish(id, JobOutcome::Cancelled).await.unwrap();
        let view = model(&manager, OutputArgs::new(id)).await;
        assert!(view["result"].is_null());
        assert!(!view["has_result"].as_bool().unwrap());
        assert_eq!(
            view["presentation"]["captures"],
            incomplete("/result/console", "text")
        );
        assert_eq!(view["presentation"]["notice"], "Output incomplete.");
        assert_eq!(lines(&model(&manager, query).await), 50);
    }

    #[tokio::test]
    async fn capture_completeness_is_internal_and_only_partial_finished_jobs_have_a_notice() {
        for complete in [true, false] {
            let (_root, manager, job) = fixture(None).await;
            let mut output = crate::tool::ToolOutput::new(json!({"stdout":"line\n".repeat(120)}));
            if !complete {
                output.streams = crate::tool::StreamEnd::Cut;
            }
            manager
                .finish(job, JobOutcome::Completed(output))
                .await
                .unwrap();
            for field in [None, Some("/result/stdout"), Some("")] {
                let mut query = OutputArgs::new(job);
                query.field = field.map(str::to_owned);
                let view = model(&manager, query).await;
                assert!(!view.to_string().contains("capture_complete"));
                if complete {
                    assert!(view["presentation"]["notice"].is_null());
                } else {
                    assert_eq!(view["presentation"]["notice"], "Output incomplete.");
                }
            }
        }
    }

    #[tokio::test]
    async fn unicode_continuations_fit_the_complete_job_envelope() {
        // Exhaustive reconstruction and replay live in reader tests. Here retain
        // the integration boundary: correct offsets and the entire view budget.
        let expected = format!("{}\nlast", "🦀\"\\".repeat(CONTENT_BYTES));
        let (_root, manager, id) = fixture(Some(json!({"content":expected}))).await;
        let mut args = field_args(id, "/result/content");
        for _ in 0..2 {
            let offset = args.offset.unwrap_or(0);
            let view = model(&manager, args.clone()).await;
            assert!(serde_json::to_vec(&view).unwrap().len() <= PAGE_BYTES);
            let page = &view["presentation"]["preview"];
            assert_eq!(page["next_start"], 1);
            let next = page["next_offset"].as_u64().unwrap() as usize;
            assert!(next > offset);
            assert_eq!(page["lines"], json!([&expected[offset..next]]));
            args.start = Some(1);
            args.offset = Some(next);
        }
    }

    #[tokio::test]
    async fn search_reaches_beyond_preview_and_merges_context_across_pages() {
        let text: String = (1..=500)
            .map(|n| {
                format!(
                    "{} {n}\n",
                    if n == 450 || n == 452 { "ERROR" } else { "ok" }
                )
            })
            .collect();
        let (_root, manager, id) = fixture(Some(json!({"stdout":text}))).await;
        let (mut start, mut offset, mut numbers) = (None, None, Vec::new());
        loop {
            let mut args = field_args(id, "/result/stdout");
            (args.start, args.offset) = (start, offset);
            (args.pattern, args.context, args.limit) = (Some("(?i)error".into()), Some(2), Some(2));
            let view = model(&manager, args).await;
            let lines = view["presentation"]["preview"]["lines"].as_array().unwrap();
            numbers.extend(lines.iter().map(|line| line.as_str().unwrap().to_owned()));
            let Some(next) = view["presentation"]["preview"]["next_start"].as_u64() else {
                break;
            };
            assert_eq!(view["presentation"]["preview"]["field"], "/result/stdout");
            start = Some(next as usize);
            offset = Some(
                view["presentation"]["preview"]["next_offset"]
                    .as_u64()
                    .unwrap_or(0) as usize,
            );
        }
        assert_eq!(numbers, text.lines().skip(447).take(7).collect::<Vec<_>>());
    }

    // Automatic capture assembly contracts; capture ownership lives in captures/.

    fn capture(manager: &JobManager, id: JobId, field: &str, kind: CaptureKind, bytes: &[u8]) {
        manager.output(id).test_capture(field, kind, bytes);
    }

    async fn hydrated(manager: &JobManager, job: JobId) -> PresentedOutput {
        let args = OutputArgs::new(job);
        let output = manager
            .inspect_output_with_captures(args, &Default::default())
            .await
            .unwrap();
        assert!(
            jsonschema::is_valid(&crate::job::presented_job_schema(false), output.view()),
            "{}",
            output.view()
        );
        output
    }

    #[tokio::test]
    async fn automatic_hydration_reads_live_and_terminal_captures_in_descriptor_order() {
        let (_root, manager, job) = fixture(None).await;
        // More than the concurrency bound, with raw/incomplete JSON and escaped pointers.
        let fields: Vec<_> = (0..7)
            .map(|i| format!("/result/events~1custom/{i}~0key"))
            .collect();
        for terminal in [false, true] {
            let bytes: &[u8] = if terminal {
                b"{\"partial\":\n42"
            } else {
                b"{\"partial\":"
            };
            for field in &fields {
                capture(&manager, job, field, CaptureKind::Json, bytes);
            }
            if terminal {
                manager.test_finish(job, json!({"status":"done"})).await;
            }
            let output = hydrated(&manager, job).await;
            assert_eq!(output.state.is_terminal(), terminal);
            let captures = output.view()["presentation"]["captures"]
                .as_array()
                .unwrap();
            assert_eq!(captures.len(), fields.len());
            for (capture, field) in captures.iter().zip(&fields) {
                assert_eq!(
                    (&capture["field"], &capture["kind"], &capture["complete"]),
                    (&json!(field), &json!("json"), &json!(false))
                );
                let preview = &capture["output"]["presentation"]["preview"];
                assert_eq!(
                    (&preview["field"], &preview["lines"][0]),
                    (&json!(field), &json!("{\"partial\":"))
                );
                assert_eq!(
                    preview["lines"].as_array().unwrap().len(),
                    if terminal { 2 } else { 1 }
                );
                // Capture pages are not recursively hydrated.
                for nested in capture["output"]["presentation"]["captures"]
                    .as_array()
                    .unwrap()
                {
                    assert!(nested["output"].is_null());
                }
            }
        }
    }

    #[tokio::test]
    async fn automatic_hydration_distinguishes_absent_from_present_and_null() {
        let (_root, manager, job) = fixture(None).await;
        for field in ["/result/absent", "/result/null", "/result/present"] {
            capture(&manager, job, field, CaptureKind::Text, b"raw capture");
        }
        manager
            .test_finish(job, json!({"null":null,"present":"structured value"}))
            .await;
        let output = hydrated(&manager, job).await;
        assert_eq!(output.view()["result"]["present"], "structured value");
        // An explicit null remains present without turning into permission to
        // hydrate an abandoned capture at the same pointer.
        assert!(output.view().pointer("/result/null").unwrap().is_null());
        for capture in output.view()["presentation"]["captures"]
            .as_array()
            .unwrap()
        {
            assert_eq!(
                !capture["output"].is_null(),
                capture["field"] == "/result/absent"
            );
        }
    }

    #[tokio::test]
    async fn automatic_hydration_retains_nested_page_continuations() {
        let (_root, manager, job) = fixture(None).await;
        capture(
            &manager,
            job,
            "/result/log",
            CaptureKind::Text,
            "line\n".repeat(150).as_bytes(),
        );
        let output = hydrated(&manager, job).await;
        // The live whole-result page is empty but retains its polling position;
        // the capture has an independent source-page continuation.
        assert_eq!(
            output.view()["presentation"]["preview"],
            json!({
                "field": "/result", "lines": [], "total_lines": null,
                "next_start": 1, "next_offset": 0
            })
        );
        let preview =
            &output.view()["presentation"]["captures"][0]["output"]["presentation"]["preview"];
        assert_eq!(preview["field"], "/result/log");
        assert_eq!(preview["lines"].as_array().unwrap().len(), 100);
        assert_eq!(preview["next_start"], 101);
        assert_eq!(preview["next_offset"], 0);
    }

    #[tokio::test]
    async fn automatic_hydration_embeds_page_errors_but_propagates_initial_errors() {
        let (_root, manager, job) = fixture(None).await;
        // Discovery succeeds, but this page cannot decode its capture.
        capture(&manager, job, "/result/bad", CaptureKind::Text, b"\xffbad");
        capture(&manager, job, "/result/good", CaptureKind::Text, b"good");
        let output = hydrated(&manager, job).await;
        let captures = &output.view()["presentation"]["captures"];
        assert!(!captures[0]["output"]["error"].as_str().unwrap().is_empty());
        assert_eq!(
            captures[1]["output"]["presentation"]["preview"]["lines"],
            json!(["good"])
        );
        let mut invalid = OutputArgs::new(job);
        invalid.start = Some(0);
        let result = manager
            .inspect_output_with_captures(invalid, &Default::default())
            .await;
        assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
        let unknown = OutputArgs::new(JobId::new(job.get() + 100).unwrap());
        assert!(
            manager
                .inspect_output_with_captures(unknown, &Default::default())
                .await
                .is_err()
        );
    }
}
