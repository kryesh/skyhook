//! Saved output presentation, independent of model providers and execution transports.
mod captures;
mod finalization;
mod products;
pub(crate) use finalization::save_completed;
pub(crate) use products::{OutputContinuation, OutputPreview, OutputTruncation};
pub use products::{OutputSelection, PresentedOutput};
mod reader;
pub use captures::CaptureKind;
pub(crate) use captures::{
    AsyncCapture, CaptureWriter, CompletedCapture, PendingCapture, TextCaptureField,
};
pub(crate) use reader::Source;
mod truncation;
use super::{JobError, JobManager, JobRole, JobState, OutputPresentation};
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

pub(crate) const PAGE_BYTES: usize = 8 * 1024;
pub(crate) const CONTENT_BYTES: usize = PAGE_BYTES - 2048;

pub(crate) use truncation::annotated_fields;

/// Omit null-valued object fields from tool-output presentation copies.
/// Use for model, history, and UI views, not lossless saved or script-native output.
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

fn capture_notice(output: &mut serde_json::Map<String, Value>, complete: Option<bool>) {
    if complete == Some(false) {
        output.insert("notice".into(), json!("Output incomplete."));
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

/// Presentation provenance for a script's full, independently saved return value.
/// Pointers are rooted at /result/value. Child jobs own their native output ranges;
/// annotated script fields own ranges in this job instead.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct ScriptPresentation {
    #[serde(default)]
    pub jobs: BTreeMap<String, JobId>,
    #[serde(default)]
    pub fields: BTreeSet<String>,
}

impl ScriptPresentation {
    fn load(output: &Output) -> Result<Self, ToolError> {
        let rows = output.db.presentation(output.job.get()).map_err(database)?;
        Ok(Self {
            jobs: rows
                .children
                .into_iter()
                .map(|(field, child)| {
                    JobId::new(child)
                        .map(|child| (field, child))
                        .map_err(|error| ToolError::Failed(error.to_string()))
                })
                .collect::<Result<_, _>>()?,
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

fn escape_pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
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
                    let child = format!("{field}/{}", escape_pointer(key));
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
                let child = format!("{field}/{}", escape_pointer(key));
                render(saved, &child, value, out, cancellation)?;
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
pub(crate) enum OutputOptions<'a> {
    Host {
        viewer: Option<&'a crate::execution::ExecutionLocation>,
        presentation: OutputPresentation,
    },
    Model {
        viewer: Option<&'a crate::execution::ExecutionLocation>,
        detailed: bool,
        presentation: OutputPresentation,
    },
}

impl OutputOptions<'_> {
    pub(crate) const HOST: Self = Self::Host {
        viewer: None,
        presentation: OutputPresentation::Full,
    };
}

impl JobManager {
    pub(crate) async fn save_script_presentation(
        &self,
        job: JobId,
        presentation: ScriptPresentation,
    ) -> Result<(), ToolError> {
        // Provenance may only refer to this script's own executed child calls.
        for child in presentation.jobs.values() {
            if self
                .metadata(*child)
                .await
                .map_err(|error| ToolError::Failed(error.to_string()))?
                .parent
                != Some(job)
            {
                return Err(ToolError::Failed("invalid script result provenance".into()));
            }
        }
        if presentation.jobs.is_empty() && presentation.fields.is_empty() {
            return Ok(());
        }
        let output = self.output(job);
        let rows = crate::session::Presentation {
            children: presentation
                .jobs
                .into_iter()
                .map(|(field, child)| (field, child.get()))
                .collect(),
            fields: presentation.fields.into_iter().collect(),
        };
        tokio::task::spawn_blocking(move || {
            output
                .db
                .save_presentation(output.job.get(), &rows)
                .map_err(database)
        })
        .await
        .map_err(|error| ToolError::Failed(error.to_string()))?
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
                viewer: None,
                detailed: true,
                presentation: crate::job::OutputPresentation::Full,
            },
        )
        .await
        .map(PresentedOutput::into_view)
    }

    /// Host inspection never acknowledges an agent's pending notification.
    ///
    /// When stored captures exist, the view includes `captures`, an array of
    /// `{field, kind, complete}` descriptors. `field` is an explicit output-query
    /// JSON Pointer; `kind` is `text`, `json`, or `unknown` (streamed transport data
    /// whose type is not known yet). Live or recovered unfinished captures have
    /// `complete: false`; they may contain invalid JSON and are exposed only as
    /// raw pages, never as a synthesized result. Discovery does not select a
    /// default capture or change explicit field/paging queries.
    pub async fn inspect_output(
        &self,
        args: OutputArgs,
        capabilities: &CapabilitySet,
    ) -> Result<Value, ToolError> {
        self.present_output_with(args, capabilities, OutputOptions::HOST)
            .await
            .map(PresentedOutput::into_view)
    }

    /// Host UI field discovery uses the saved tree, never presentation wrappers
    /// that may substitute a child's JobView for a script's original return.
    pub async fn inspect_output_fields(&self, job: JobId) -> Result<Vec<String>, ToolError> {
        let terminal = self
            .metadata(job)
            .await
            .map_err(|error| ToolError::Failed(error.to_string()))?
            .state
            .is_terminal();
        let output = self.output(job);
        tokio::task::spawn_blocking(move || {
            fn visit(value: &Value, pointer: String, paths: &mut Vec<String>) {
                paths.push(pointer.clone());
                match value {
                    Value::Object(object) => {
                        for (key, value) in object {
                            visit(
                                value,
                                format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1")),
                                paths,
                            );
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
            for capture in captures::available_captures(&saved, terminal)? {
                if !paths.contains(&capture.field) {
                    paths.push(capture.field);
                }
            }
            Ok(paths)
        })
        .await
        .map_err(|error| ToolError::Failed(error.to_string()))?
    }

    /// Host inspection with automatic pages for captures absent from the whole
    /// presentation. Any explicit selection (including `context: 0`) suppresses
    /// hydration. A present JSON null is not an absent capture.
    ///
    /// Metadata/images belong to the initial job snapshot; captures remain
    /// live reads and can advance while up to four pages are read concurrently.
    /// A failed initial inspection is returned; individual page failures are
    /// embedded as `{error}` in that capture's `output`. This never acknowledges
    /// pending notifications, and does not recursively hydrate capture pages.
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
        options: OutputOptions<'_>,
    ) -> Result<PresentedOutput, ToolError> {
        let (acknowledge, viewer, detailed, presentation) = match options {
            OutputOptions::Host {
                viewer,
                presentation,
            } => (false, viewer, true, presentation),
            OutputOptions::Model {
                viewer,
                detailed,
                presentation,
            } => (true, viewer, detailed, presentation),
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
                entry.last_agent_message.filter(|_| {
                    !explicit
                        && presentation == OutputPresentation::Automatic
                        && entry.role == JobRole::Agent
                        && entry.state == JobState::Completed
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
            let view =
                envelope.presented_with_reference(capabilities, viewer, detailed, last_message)?;
            if acknowledge {
                self.claim(args.job)
                    .await
                    .map_err(|error| ToolError::Failed(error.to_string()))?;
            }
            return Ok(PresentedOutput {
                state: envelope.state,
                view,
                images,
                capture_targets: Vec::new(),
            });
        }
        let terminal = envelope.state.is_terminal();
        let question = envelope.state == JobState::WaitingInput;
        let live_question = envelope.output.take();
        let mut view = envelope.presented_for(capabilities, viewer, detailed)?;
        let map = view.as_object_mut().expect("job envelope is an object");
        map.remove("output");
        let output = self.output(args.job);
        let saved =
            tokio::task::spawn_blocking(move || Saved::load(&output).map(std::sync::Arc::new))
                .await
                .map_err(|error| ToolError::Failed(error.to_string()))??;
        let captures = captures::available_captures(&saved, terminal)?;
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
        if !captures.is_empty() {
            map.insert("captures".into(), serde_json::to_value(&captures)?);
        }
        let structured = !explicit && terminal && saved.document.is_some();
        let mut presented_question = false;
        let mut question_page = None;
        if structured {
            let presentation_output = saved.output.clone();
            let script_presentation =
                tokio::task::spawn_blocking(move || ScriptPresentation::load(&presentation_output))
                    .await
                    .map_err(|error| ToolError::Failed(error.to_string()))??;
            let mut children = BTreeMap::new();
            let mut replacements = BTreeMap::new();
            for (field, child) in script_presentation.jobs {
                if self
                    .metadata(child)
                    .await
                    .map_err(|error| ToolError::Failed(error.to_string()))?
                    .parent
                    != Some(args.job)
                {
                    return Err(ToolError::Failed("invalid script result provenance".into()));
                }
                if let std::collections::btree_map::Entry::Vacant(entry) = children.entry(child) {
                    let mut query = OutputArgs::new(child);
                    query.cancellation = args.cancellation.clone();
                    let view = Box::pin(self.present_output_with(
                        query,
                        capabilities,
                        OutputOptions::Host {
                            viewer,
                            presentation: OutputPresentation::Full,
                        },
                    ))
                    .await?;
                    entry.insert(view.into_view());
                }
                replacements.insert(field, children[&child].clone());
            }
            let projected = saved.clone();
            let cancellation = args.cancellation.clone().unwrap_or_default();
            let view = tokio::task::spawn_blocking(move || {
                truncation::project(
                    &projected,
                    &output_schema,
                    &cancellation,
                    &script_presentation.fields,
                    &replacements,
                )
            })
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))??;
            map.extend(view);
        } else if question && let Some(value) = live_question {
            if !explicit {
                map.insert("question".into(), value);
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
        if !structured && !map.contains_key("question") {
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
                tokio::task::spawn_blocking(move || {
                    let closed = terminal || c.field.starts_with("/questions/");
                    let source = match question_page {
                        Some(bytes) => Some(Source::Memory(std::io::Cursor::new(bytes))),
                        None => field_source(&paged, &c.field, &cancellation)?,
                    };
                    reader::page(source, &c, limit, closed, &cancellation)
                })
                .await
                .map_err(|e| ToolError::Failed(e.to_string()))??
            };
            if terminal {
                let complete = saved
                    .document
                    .as_ref()
                    .and_then(|document| document.get("capture_complete"))
                    .and_then(Value::as_bool);
                capture_notice(map, complete);
            }
            map.insert("preview".into(), serde_json::to_value(&page)?);
        }
        if incomplete_capture {
            capture_notice(map, Some(false));
        }
        if acknowledge
            && ((terminal && !selection.field.starts_with("/questions/")) || presented_question)
        {
            self.claim(args.job)
                .await
                .map_err(|e| ToolError::Failed(e.to_string()))?;
        }
        // Preserve presence before presentation elides object nulls. Hydration
        // targets come from discovered records, never decoded wire descriptors.
        let capture_targets = captures
            .iter()
            .enumerate()
            .filter(|(_, capture)| {
                output_selection == OutputSelection::WholeWithImages
                    && view.pointer(&capture.field).is_none()
            })
            .map(|(index, capture)| (index, capture.field.clone()))
            .collect();
        omit_null_fields(&mut view);
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
        let value = tokio::task::spawn_blocking(move || {
            let saved = Saved::load(&output)?;
            saved
                .document
                .clone()
                .map(|document| hydrate(&saved, document, maximum))
                .transpose()
        })
        .await
        .map_err(|e| JobError::Internal(e.to_string()))?
        .map_err(|e| JobError::Internal(e.to_string()))?;
        if let Some(mut value) = value {
            envelope.output = value.get_mut("result").map(Value::take);
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

pub(crate) fn view_schema(capabilities: &CapabilitySet) -> Value {
    let mut schema = super::presented_job_schema(capabilities, false);
    let properties = schema["properties"]
        .as_object_mut()
        .expect("envelope properties");
    properties.remove("output");
    for name in ["result", "question"] {
        properties.insert(name.into(), json!({}));
    }
    properties.insert("notice".into(), json!({"type":"string"}));
    properties.insert(
        "captures".into(),
        json!({"type":"array","items":{
            "type":"object","properties":{
                "field":{"type":"string"},
                "kind":{"type":"string","enum":["text","json","unknown"]},
                "complete":{"type":"boolean"}
            },"required":["field","kind","complete"],"additionalProperties":false
        }}),
    );
    properties.insert(
        "truncated".into(),
        json!({"type":"array","items":{
            "type":"object","properties":{"field":{"type":"string"},"total_lines":{"type":"integer","minimum":0},
                "next_start":{"type":"integer","minimum":1},"next_offset":{"type":"integer","minimum":0}},
            "required":["field","total_lines","next_start"]
        }}),
    );
    properties.insert("preview".into(), json!({"type":"object","properties":{
        "field":{"type":"string"},"lines":{"type":"array","items":{"type":"string"}},"total_lines":{"type":"integer","minimum":0},
        "next_start":{"type":"integer","minimum":1},"next_offset":{"type":"integer","minimum":0}
    },"required":["field","lines"]}));
    if let Some(required) = schema["required"].as_array_mut() {
        required.retain(|v| v != "output");
    }
    schema.as_object_mut().unwrap().remove("allOf");
    schema
}

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
        }))
    })
}

/// Captures a remote worker streams separately from its result frame.
pub(crate) fn transfer_fields(
    output: &Output,
) -> Result<Vec<(String, CaptureKind, Source)>, ToolError> {
    let saved = Saved::load(output)?;
    Ok(captures::available_captures(&saved, true)?
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
mod hydration_tests;

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

    async fn model(manager: &JobManager, args: OutputArgs) -> Value {
        manager
            .present_output(args, &Default::default())
            .await
            .unwrap()
    }

    fn incomplete(field: &str, kind: &str) -> Value {
        json!([{ "field":field, "kind":kind, "complete":false }])
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
        assert!(whole.view().get("preview").is_none());
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
            assert_eq!(product.view()["preview"].is_object(), !whole, "{field}");
            assert_eq!(product.view()["captures"].as_array().unwrap().len(), 1);
            assert!(product.view()["captures"][0].get("output").is_none());
        }
        assert_eq!(manager.images(id).await.unwrap(), images);
    }

    #[tokio::test]
    async fn presentation_omits_null_fields_but_saved_output_stays_lossless() {
        let raw = json!({
            "error": null,
            "nested": {"absent": null, "ok": false},
            "array": [null, {"absent": null, "count": 0}]
        });
        let (_root, manager, id) = fixture(Some(raw.clone())).await;
        let expected = json!({"nested": {"ok": false}, "array": [null, {"count": 0}]});
        assert_eq!(
            model(&manager, OutputArgs::new(id)).await["result"],
            expected
        );
        let host_view = host(&manager, OutputArgs::new(id)).await;
        assert_eq!(host_view["result"], expected);
        let product = manager
            .present_output_with(
                OutputArgs::new(id),
                &Default::default(),
                OutputOptions::HOST,
            )
            .await
            .unwrap();
        // The canonical product preserves established presentation policy; it
        // is not a new raw/lossless payload channel.
        assert_eq!(product.view(), &host_view);
        let saved = manager.output(id).test_document().unwrap();
        assert_eq!(saved["result"], raw);
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
            assert!(
                host(&manager, OutputArgs::new(id))
                    .await
                    .get("captures")
                    .is_none()
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
                assert_eq!(view["captures"], incomplete(field, "json"));
                assert_eq!(page["preview"]["lines"], json!([bytes]), "{outcome}");
                if outcome == "unreferenced" && terminal {
                    assert_eq!(view["notice"], "Output incomplete.");
                    assert_eq!(page["notice"], "Output incomplete.");
                } else {
                    assert!(view.get("result").is_none(), "{outcome}");
                }
            }
            assert_eq!(output.test_bytes(field).unwrap(), bytes.as_bytes());
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
        let lines = |page: &Value| page["preview"]["lines"].as_array().unwrap().len();
        assert_eq!(lines(&model(&manager, query.clone()).await), 50);
        manager.finish(id, JobOutcome::Cancelled).await.unwrap();
        let view = model(&manager, OutputArgs::new(id)).await;
        assert!(view.get("result").is_none());
        assert_eq!(view["captures"], incomplete("/result/console", "text"));
        assert_eq!(view["notice"], "Output incomplete.");
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
                assert_eq!(view.get("notice").is_some(), !complete);
                if !complete {
                    assert_eq!(view["notice"], "Output incomplete.");
                }
            }
        }
    }

    #[tokio::test]
    async fn unicode_continuations_fit_the_complete_job_envelope() {
        // Exhaustive reconstruction and replay live in reader tests. Here retain
        // the integration boundary: correct offsets and the entire view budget.
        let expected = format!("{}\nlast", "🦀\"\\".repeat(4000));
        let (_root, manager, id) = fixture(Some(json!({"content":expected}))).await;
        let mut args = field_args(id, "/result/content");
        for _ in 0..2 {
            let offset = args.offset.unwrap_or(0);
            let view = model(&manager, args.clone()).await;
            assert!(serde_json::to_vec(&view).unwrap().len() <= PAGE_BYTES);
            let page = &view["preview"];
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
            let lines = view["preview"]["lines"].as_array().unwrap();
            numbers.extend(lines.iter().map(|line| line.as_str().unwrap().to_owned()));
            let Some(next) = view["preview"]["next_start"].as_u64() else {
                break;
            };
            assert_eq!(view["preview"]["field"], "/result/stdout");
            start = Some(next as usize);
            offset = Some(view["preview"]["next_offset"].as_u64().unwrap_or(0) as usize);
        }
        assert_eq!(numbers, text.lines().skip(447).take(7).collect::<Vec<_>>());
    }
}
