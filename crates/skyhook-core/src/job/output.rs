//! Saved output presentation, independent of model providers and execution transports.
mod reader;
mod truncation;
use super::{JobError, JobManager, JobState};
use crate::{
    identity::JobId,
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
    path::{Path, PathBuf},
};

pub(crate) const PAGE_BYTES: usize = 8 * 1024;
pub(crate) const CONTENT_BYTES: usize = PAGE_BYTES - 2048;

pub(crate) use truncation::annotated_fields;

fn capture_notice(output: &mut serde_json::Map<String, Value>, complete: Option<bool>) {
    if complete == Some(false) {
        output.insert("notice".into(), json!("Output incomplete."));
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
    fn load(directory: &Path) -> Result<Self, ToolError> {
        match std::fs::File::open(directory.join("presentation.json")) {
            Ok(file) => Ok(serde_json::from_reader(BufReader::new(file))?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
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
    pattern: Option<String>,
    context: usize,
    start: usize,
    offset: usize,
}

pub(crate) fn field_file(directory: &Path, field: &str) -> PathBuf {
    directory.join(format!(
        "field-{}.txt",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(field.as_bytes()))
    ))
}

/// Persist a compact structured tree with file-backed large strings.
pub(crate) fn save(directory: &Path, document: &Value) -> std::io::Result<()> {
    std::fs::create_dir_all(directory)?;
    let mut document = document.clone();
    let mut fields = Vec::<String>::new();
    fn visit(
        directory: &Path,
        field: &str,
        value: &mut Value,
        fields: &mut Vec<String>,
    ) -> std::io::Result<()> {
        match value {
            Value::String(text) => {
                let path = field_file(directory, field);
                if path.exists() || text.len() > 4096 {
                    if !path.exists() {
                        let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
                        file.write_all(text.as_bytes())?;
                        file.flush()?;
                    }
                    fields.push(field.to_owned());
                    text.clear();
                }
            }
            Value::Object(map) => {
                if field_file(directory, field).exists() {
                    fields.push(field.to_owned());
                    map.clear();
                    return Ok(());
                }
                for (key, value) in map {
                    visit(
                        directory,
                        &format!("{field}/{}", key.replace('~', "~0").replace('/', "~1")),
                        value,
                        fields,
                    )?;
                }
            }
            Value::Array(items) => {
                if field_file(directory, field).exists() {
                    fields.push(field.to_owned());
                    items.clear();
                } else {
                    for (index, value) in items.iter_mut().enumerate() {
                        visit(directory, &format!("{field}/{index}"), value, fields)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
    visit(directory, "", &mut document, &mut fields)?;
    for (name, value) in [
        ("fields.json", serde_json::to_value(fields)?),
        ("document.json", document),
    ] {
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(&mut file, &value)?;
        file.flush()?;
        file.persist(directory.join(name)).map_err(|e| e.error)?;
    }
    Ok(())
}

fn fields(directory: &Path) -> Result<Vec<String>, ToolError> {
    Ok(serde_json::from_reader(BufReader::new(
        std::fs::File::open(directory.join("fields.json"))?,
    ))?)
}
fn hydrate(directory: &Path, mut value: Value, maximum: u64) -> Result<Value, ToolError> {
    for field in fields(directory)? {
        if std::fs::metadata(field_file(directory, &field))?.len() > maximum {
            continue;
        }
        let target = value
            .pointer_mut(&field)
            .ok_or_else(|| ToolError::Failed("invalid saved output field".into()))?;
        *target = if target.is_string() {
            Value::String(
                String::from_utf8_lossy(&std::fs::read(field_file(directory, &field))?)
                    .into_owned(),
            )
        } else {
            serde_json::from_reader(BufReader::new(std::fs::File::open(field_file(
                directory, &field,
            ))?))?
        };
    }
    Ok(value)
}
fn render(
    directory: &Path,
    field: &str,
    value: &Value,
    out: &mut impl Write,
    cancellation: &super::CancellationToken,
) -> Result<(), ToolError> {
    if cancellation.is_cancelled() {
        return Err(ToolError::Cancelled);
    }
    if (value.is_array() || value.is_object()) && field_file(directory, field).exists() {
        std::io::copy(&mut std::fs::File::open(field_file(directory, field))?, out)?;
    } else if value.is_string() && field_file(directory, field).exists() {
        out.write_all(b"\"")?;
        let mut input = BufReader::new(std::fs::File::open(field_file(directory, field))?);
        // Escape a file-backed string incrementally rather than hydrating it.
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
    } else {
        match value {
            Value::Object(map) => {
                out.write_all(b"{\n")?;
                for (index, (key, value)) in map.iter().enumerate() {
                    if index > 0 {
                        out.write_all(b",\n")?;
                    }
                    serde_json::to_writer(&mut *out, key)?;
                    out.write_all(b": ")?;
                    render(
                        directory,
                        &format!("{field}/{}", key.replace('~', "~0").replace('/', "~1")),
                        value,
                        out,
                        cancellation,
                    )?;
                }
                out.write_all(b"\n}")?;
            }
            Value::Array(items) => {
                out.write_all(b"[\n")?;
                for (index, value) in items.iter().enumerate() {
                    if index > 0 {
                        out.write_all(b",\n")?;
                    }
                    render(
                        directory,
                        &format!("{field}/{index}"),
                        value,
                        out,
                        cancellation,
                    )?;
                }
                out.write_all(b"\n]")?;
            }
            _ => serde_json::to_writer(out, value)?,
        }
    }
    Ok(())
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
        let directory = self.output_directory(job);
        tokio::task::spawn_blocking(move || -> Result<(), ToolError> {
            std::fs::create_dir_all(&directory)?;
            let mut file = tempfile::NamedTempFile::new_in(&directory)?;
            serde_json::to_writer(&mut file, &presentation)?;
            file.flush()?;
            file.persist(directory.join("presentation.json"))
                .map_err(|error| error.error)?;
            Ok(())
        })
        .await
        .map_err(|error| ToolError::Failed(error.to_string()))?
    }

    pub(crate) async fn output_changed(&self, id: JobId) {
        if let Some(entry) = self.inner.jobs.lock().await.get(&id) {
            entry.notify.notify_waiters();
        }
    }

    pub(crate) fn output_directory(&self, id: JobId) -> PathBuf {
        self.inner
            .store
            .directory()
            .join("jobs")
            .join(id.to_string())
    }

    #[cfg(test)]
    pub(crate) async fn present_output(
        &self,
        args: OutputArgs,
        capabilities: &CapabilitySet,
    ) -> Result<Value, ToolError> {
        self.present_output_inner(args, capabilities, true, None, true)
            .await
    }

    /// Host inspection never acknowledges an agent's pending notification.
    pub async fn inspect_output(
        &self,
        args: OutputArgs,
        capabilities: &CapabilitySet,
    ) -> Result<Value, ToolError> {
        self.present_output_inner(args, capabilities, false, None, true)
            .await
    }

    pub(crate) async fn present_output_for(
        &self,
        args: OutputArgs,
        capabilities: &CapabilitySet,
        viewer: &crate::execution::ExecutionLocation,
        detailed: bool,
    ) -> Result<Value, ToolError> {
        self.present_output_inner(args, capabilities, true, Some(viewer), detailed)
            .await
    }

    pub(crate) async fn inspect_output_for(
        &self,
        args: OutputArgs,
        capabilities: &CapabilitySet,
        viewer: &crate::execution::ExecutionLocation,
    ) -> Result<Value, ToolError> {
        self.present_output_inner(args, capabilities, false, Some(viewer), true)
            .await
    }

    async fn present_output_inner(
        &self,
        args: OutputArgs,
        capabilities: &CapabilitySet,
        acknowledge: bool,
        viewer: Option<&crate::execution::ExecutionLocation>,
        detailed: bool,
    ) -> Result<Value, ToolError> {
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
        let explicit = args.field.is_some()
            || args.start.is_some()
            || args.pattern.is_some()
            || args.offset.is_some()
            || args.limit.is_some();
        let mut selection = Selection {
            field: args.field.clone().unwrap_or_else(|| "/result".into()),
            pattern: args.pattern.clone(),
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
        if let Some(pattern) = &selection.pattern {
            crate::tool::builtins::search::output_matcher(pattern)?;
        }
        let (mut envelope, output_schema) = {
            let jobs = self.inner.jobs.lock().await;
            let entry = jobs
                .get(&args.job)
                .ok_or_else(|| ToolError::Failed(format!("unknown job {}", args.job)))?;
            (
                entry.envelope(args.job),
                entry.output_schema.clone().unwrap_or(Value::Bool(true)),
            )
        };
        let terminal = envelope.state.is_terminal();
        let question = envelope.state == JobState::WaitingInput;
        let live_question = envelope.output.take();
        let mut view = envelope.presented_for(capabilities, viewer, detailed)?;
        let map = view.as_object_mut().expect("job envelope is an object");
        map.remove("output");
        let directory = self.output_directory(args.job);
        let document_path = directory.join("document.json");
        let structured = !explicit && terminal && document_path.exists();
        let mut presented_question = false;
        if structured {
            let presentation_directory = directory.clone();
            let presentation = tokio::task::spawn_blocking(move || {
                ScriptPresentation::load(&presentation_directory)
            })
            .await
            .map_err(|error| ToolError::Failed(error.to_string()))??;
            let mut children = BTreeMap::new();
            let mut replacements = BTreeMap::new();
            for (field, child) in presentation.jobs {
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
                    let view = Box::pin(self.present_output_inner(
                        query,
                        capabilities,
                        false,
                        viewer,
                        true,
                    ))
                    .await?;
                    entry.insert(view);
                }
                replacements.insert(field, children[&child].clone());
            }
            let directory = directory.clone();
            let cancellation = args.cancellation.clone().unwrap_or_default();
            let projection = tokio::task::spawn_blocking(move || {
                truncation::project(
                    &directory,
                    &output_schema,
                    &cancellation,
                    &presentation.fields,
                    &replacements,
                )
            })
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))??;
            map.extend(projection);
        } else if question && let Some(value) = live_question {
            if !explicit {
                map.insert("question".into(), value);
                presented_question = true;
            } else {
                let bytes = serde_json::to_vec_pretty(&value)?;
                let key =
                    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(&bytes));
                let field = format!("/questions/{key}");
                let path = field_file(&directory, &field);
                tokio::fs::create_dir_all(&directory).await?;
                if !path.exists() {
                    tokio::fs::write(path, bytes).await?;
                }
                if args.field.is_none() {
                    selection.field = field.clone();
                }
                presented_question = selection.field == field;
            }
        }
        if !structured && !map.contains_key("question") {
            if !explicit && !question && document_path.exists() {
                selection.field = String::new();
            }
            let directory = directory.clone();
            let c = selection.clone();
            let cancellation = args.cancellation.clone().unwrap_or_default();
            let unavailable = !terminal
                && !question
                && (!explicit
                    || envelope.location.target != "root"
                    || c.field == "/result"
                    || c.field.is_empty());
            let page = tokio::task::spawn_blocking(move || {
                if unavailable {
                    return Ok(reader::empty(&c, None, false));
                }
                let closed = terminal || c.field.starts_with("/questions/");
                page(&directory, c, limit, closed, &cancellation)
            })
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))??;
            if terminal {
                let complete = tokio::fs::read(&document_path)
                    .await
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    .and_then(|v| v.get("capture_complete").and_then(Value::as_bool));
                capture_notice(map, complete);
            }
            map.insert("preview".into(), page);
        }
        if acknowledge
            && ((terminal && !selection.field.starts_with("/questions/")) || presented_question)
        {
            self.claim(args.job)
                .await
                .map_err(|e| ToolError::Failed(e.to_string()))?;
        }
        Ok(view)
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
        let path = self.output_directory(envelope.id).join("document.json");
        match tokio::fs::read(path).await {
            Ok(bytes) => {
                let value: Value =
                    serde_json::from_slice(&bytes).map_err(crate::session::SessionError::from)?;
                let directory = self.output_directory(envelope.id);
                let mut value =
                    tokio::task::spawn_blocking(move || hydrate(&directory, value, maximum))
                        .await
                        .map_err(|e| JobError::Internal(e.to_string()))?
                        .map_err(|e| JobError::Internal(e.to_string()))?;
                envelope.output = value.get_mut("result").map(Value::take);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(crate::session::SessionError::from(error).into()),
        }
        Ok(())
    }
}

fn page(
    directory: &Path,
    selection: Selection,
    limit: usize,
    terminal: bool,
    cancellation: &super::CancellationToken,
) -> Result<Value, ToolError> {
    let path = ensure_field_file(directory, &selection.field, cancellation)?;
    reader::page(&path, &selection, limit, terminal, cancellation)
}

fn ensure_field_file(
    directory: &Path,
    field: &str,
    cancellation: &super::CancellationToken,
) -> Result<PathBuf, ToolError> {
    let path = field_file(directory, field);
    if !path.exists() && directory.join("document.json").exists() {
        let mut document: Value = serde_json::from_reader(BufReader::new(std::fs::File::open(
            directory.join("document.json"),
        )?))?;
        if document.pointer(field).is_none() {
            document = hydrate(directory, document, u64::MAX)?;
        }
        if field.is_empty() {
            // Whole-output pages contain the public document, not internal capture metadata.
            let output = document.as_object_mut().expect("saved output document");
            output.remove("capture_complete");
        }
        let value = document.pointer(field).ok_or_else(|| {
            ToolError::InvalidArguments("field does not exist in this result".into())
        })?;
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        {
            let mut writer = std::io::BufWriter::new(file.as_file_mut());
            if let Some(text) = value.as_str() {
                writer.write_all(text.as_bytes())?;
            } else {
                render(directory, field, value, &mut writer, cancellation)?;
            }
            writer.flush()?;
        }
        file.persist(&path).map_err(|e| ToolError::Io(e.error))?;
    }
    Ok(path)
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

pub(crate) fn presentation_size(directory: &Path) -> usize {
    let Ok(metadata) = std::fs::metadata(directory.join("document.json")) else {
        return PAGE_BYTES;
    };
    let Ok(fields) = fields(directory) else {
        return PAGE_BYTES;
    };
    fields.iter().fold(
        usize::try_from(metadata.len()).unwrap_or(PAGE_BYTES),
        |total, field| {
            total.saturating_add(std::fs::metadata(field_file(directory, field)).map_or(
                PAGE_BYTES,
                |m| {
                    usize::try_from(m.len())
                        .unwrap_or(PAGE_BYTES)
                        .saturating_mul(6)
                },
            ))
        },
    )
}

pub(crate) fn transfer_fields(directory: &Path) -> Result<Vec<(String, PathBuf)>, ToolError> {
    fields(directory)?
        .into_iter()
        .filter_map(|field| {
            let path = field_file(directory, &field);
            match std::fs::metadata(&path) {
                Ok(meta) if meta.len() > PAGE_BYTES as u64 => Some(Ok((field, path))),
                Ok(_) => None,
                Err(error) => Some(Err(error.into())),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        identity::AgentId,
        job::{JobOutcome, JobSpec},
        session::SessionStore,
        tool::ToolOutput,
    };

    async fn fixture(value: Value) -> (tempfile::TempDir, JobManager, JobId) {
        fixture_output(ToolOutput::new(value)).await
    }

    async fn fixture_output(output: ToolOutput) -> (tempfile::TempDir, JobManager, JobId) {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let manager = JobManager::new(store.clone());
        let lease = manager
            .create(JobSpec::test(AgentId::root(store.id()), "fixture"))
            .await
            .unwrap();
        manager
            .transition(lease.id, JobState::Running)
            .await
            .unwrap();
        manager
            .finish(lease.id, JobOutcome::Completed(output))
            .await
            .unwrap();
        (root, manager, lease.id)
    }

    fn script_schema() -> Value {
        json!({"type":"object","properties":{
            "value":{}, "console":{"type":"string","x-skyhook-truncatable":true}
        }})
    }

    async fn script_fixture(value: Value, console: &str) -> (tempfile::TempDir, JobManager, JobId) {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let manager = JobManager::new(store.clone());
        let mut spec = JobSpec::test(AgentId::root(store.id()), "script");
        spec.output_schema = Some(script_schema());
        let id = manager.create(spec).await.unwrap().id;
        manager
            .finish(
                id,
                JobOutcome::Completed(ToolOutput::new(json!({"value":value,"console":console}))),
            )
            .await
            .unwrap();
        (root, manager, id)
    }

    fn preview_text(view: &Value) -> String {
        view["preview"]["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|line| line.as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn generic_jobs_have_no_console_envelope_or_annotation() {
        let schema = view_schema(&Default::default());
        assert!(schema["properties"].get("console").is_none());
        assert!(schema["properties"].get("console_output").is_none());
        // Even a native tool's field named console is not implicitly truncated.
        let value = json!({"console":"x".repeat(5000),"ok":true});
        let (_root, manager, job) = fixture(value.clone()).await;
        let view = manager
            .present_output(OutputArgs::new(job), &Default::default())
            .await
            .unwrap();
        assert_eq!(view["result"], value);
        assert!(view.get("console").is_none());
        assert!(view.get("console_output").is_none());
        assert!(view.get("truncated").is_none());
        let envelope = serde_json::to_value(manager.snapshot(job).await.unwrap()).unwrap();
        assert!(envelope.get("console_output").is_none());
        let document: Value = serde_json::from_slice(
            &std::fs::read(manager.output_directory(job).join("document.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(document.as_object().unwrap().len(), 3);
        assert!(document.get("capture_complete").is_some());
        assert!(document.get("result").is_some());
        assert!(document.get("error").is_some());
    }

    #[tokio::test]
    async fn script_console_is_inside_result_bounded_and_readable_even_when_empty() {
        for console in [String::new(), "hello\n".to_owned(), "x".repeat(5000)] {
            let (_root, manager, job) = script_fixture(json!({"ok":true}), &console).await;
            let view = manager
                .present_output(OutputArgs::new(job), &Default::default())
                .await
                .unwrap();
            assert_eq!(view["result"]["value"], json!({"ok":true}));
            assert!(view.get("console").is_none());
            assert!(view.get("console_output").is_none());
            assert_eq!(
                view["result"]["console"],
                &console[..console.len().min(truncation::FIELD_BYTES)]
            );
            if console.len() > truncation::FIELD_BYTES {
                assert_eq!(view["truncated"][0]["field"], "/result/console");
            }
            let mut query = OutputArgs::new(job);
            query.field = Some(String::new());
            let whole = manager
                .present_output(query, &Default::default())
                .await
                .unwrap();
            let document: Value = serde_json::from_str(&preview_text(&whole)).unwrap();
            assert!(document.get("console").is_none());
            assert_eq!(document["result"]["console"], console);

            let mut query = OutputArgs::new(job);
            query.field = Some("/result/console".to_owned());
            let logs = manager
                .present_output(query, &Default::default())
                .await
                .unwrap();
            assert_eq!(logs["preview"]["field"], "/result/console");
            assert_eq!(
                logs["preview"]["lines"],
                json!(console.lines().collect::<Vec<_>>())
            );
            assert_eq!(
                manager.snapshot(job).await.unwrap().output.unwrap(),
                json!({"value":{"ok":true},"console":console})
            );
        }
    }

    #[tokio::test]
    async fn script_console_capture_can_be_paged_while_running_and_after_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let manager = JobManager::new(store.clone());
        let mut spec = JobSpec::test(AgentId::root(store.id()), "script");
        spec.output_schema = Some(script_schema());
        let id = manager.create(spec).await.unwrap().id;
        manager.transition(id, JobState::Running).await.unwrap();
        let directory = manager.output_directory(id);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            field_file(&directory, "/result/console"),
            "console\n".repeat(150),
        )
        .unwrap();
        let mut query = OutputArgs::new(id);
        query.field = Some("/result/console".into());
        query.start = Some(101);
        let live = manager
            .present_output(query.clone(), &Default::default())
            .await
            .unwrap();
        assert_eq!(live["preview"]["lines"].as_array().unwrap().len(), 50);
        manager.finish(id, JobOutcome::Cancelled).await.unwrap();
        let view = manager
            .present_output(OutputArgs::new(id), &Default::default())
            .await
            .unwrap();
        assert_eq!(view["result"]["value"], Value::Null);
        assert_eq!(view["result"]["console"], "console\n".repeat(100));
        assert_eq!(view["truncated"][0]["field"], "/result/console");
        assert_eq!(view["truncated"][0]["next_start"], 101);
        assert_eq!(view["notice"], "Output incomplete.");
        let page = manager
            .present_output(query, &Default::default())
            .await
            .unwrap();
        assert_eq!(page["preview"]["lines"].as_array().unwrap().len(), 50);
    }

    #[tokio::test]
    async fn capture_completeness_is_internal_and_only_partial_finished_jobs_have_a_notice() {
        for complete in [true, false] {
            let (root, manager, job) =
                fixture(json!({"stdout":"line\n".repeat(120), "timed_out":!complete})).await;
            let directory = manager.output_directory(job);
            let saved: Value = serde_json::from_slice(
                &tokio::fs::read(directory.join("document.json"))
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(saved["capture_complete"], complete);
            for field in [None, Some("/result/stdout"), Some("")] {
                let mut query = OutputArgs::new(job);
                query.field = field.map(str::to_owned);
                let view = manager
                    .present_output(query, &Default::default())
                    .await
                    .unwrap();
                assert!(view.get("capture_complete").is_none());
                assert!(view["preview"].get("capture_complete").is_none());
                assert!(!view.to_string().contains("capture_complete"));
                assert_eq!(view.get("notice").is_some(), !complete);
                if !complete {
                    assert_eq!(view["notice"], "Output incomplete.");
                }
            }
            let session = manager.store().id();
            manager.store().close().await.unwrap();
            let (store, records) = SessionStore::open(root.path(), session).await.unwrap();
            let restored = JobManager::restore(store, &records).await.unwrap();
            let view = restored
                .present_output(OutputArgs::new(job), &Default::default())
                .await
                .unwrap();
            assert!(view.get("capture_complete").is_none());
            assert_eq!(view.get("notice").is_some(), !complete);
        }
        let (_root, manager, _) = fixture(Value::Null).await;
        let agent = AgentId::root(manager.store().id());
        let running = manager
            .create(JobSpec::test(agent, "running"))
            .await
            .unwrap()
            .id;
        manager
            .transition(running, JobState::Running)
            .await
            .unwrap();
        let view = manager
            .present_output(OutputArgs::new(running), &Default::default())
            .await
            .unwrap();
        assert!(view.get("notice").is_none());
        assert!(!view.to_string().contains("capture_complete"));
        assert!(
            !view_schema(&Default::default())
                .to_string()
                .contains("capture_complete")
        );
    }

    #[tokio::test]
    async fn host_inspection_does_not_acknowledge_completion_or_question() {
        let (_root, manager, job) = fixture(json!({"content":"saved evidence"})).await;
        let before = manager.store().records().await;
        let value = manager
            .inspect_output(OutputArgs::new(job), &CapabilitySet::default())
            .await
            .unwrap();
        assert!(value.to_string().contains("saved evidence"));
        assert_eq!(manager.store().records().await, before);
        let agent = AgentId::root(manager.store().id());
        let lease = manager.create(JobSpec::test(agent, "ask")).await.unwrap();
        manager
            .transition(lease.id, JobState::Running)
            .await
            .unwrap();
        manager
            .request_input(lease.id, json!({"question_id":"q1","text":"choose"}))
            .await
            .unwrap();
        let before = manager.store().records().await;
        let value = manager
            .inspect_output(OutputArgs::new(lease.id), &CapabilitySet::default())
            .await
            .unwrap();
        assert!(value.to_string().contains("choose"));
        assert_eq!(manager.store().records().await, before);
    }

    #[tokio::test]
    async fn reading_an_older_question_does_not_acknowledge_its_replacement() {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let manager = JobManager::new(store);
        let mut spec = JobSpec::test(agent.clone(), "agent");
        spec.background = true;
        let job = manager.create(spec).await.unwrap().id;
        manager.transition(job, JobState::Running).await.unwrap();
        manager
            .request_input(job, json!({"question_id":"first","text":"a".repeat(12000)}))
            .await
            .unwrap();
        let mut selection = OutputArgs::new(job);
        selection.start = Some(1);
        let first = manager
            .present_output(selection, &Default::default())
            .await
            .unwrap();
        let old_page = &first["preview"];
        manager.resume_input(job).await.unwrap();
        manager
            .request_input(job, json!({"question_id":"second","text":"new question"}))
            .await
            .unwrap();
        let mut args = OutputArgs::new(job);
        args.field = Some(old_page["field"].as_str().unwrap().into());
        args.start = Some(old_page["next_start"].as_u64().unwrap() as usize);
        args.offset = Some(old_page["next_offset"].as_u64().unwrap_or(0) as usize);
        let page = manager
            .present_output(args, &Default::default())
            .await
            .unwrap();
        assert!(
            page["preview"]["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| line.as_str().unwrap().contains("aaaa"))
        );
        let pending = manager.take_pending(&agent).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].output.as_ref().unwrap()["question_id"], "second");
        let current = manager
            .present_output(OutputArgs::new(job), &Default::default())
            .await
            .unwrap();
        assert_eq!(current["question"]["question_id"], "second");
    }

    #[tokio::test]
    async fn saved_ranges_survive_session_resume() {
        let (root, manager, id) = fixture(json!({"content":"first\nsecond\nthird"})).await;
        let mut args = OutputArgs::new(id);
        args.field = Some("/result/content".into());
        args.limit = Some(1);
        let first = manager
            .present_output(args, &Default::default())
            .await
            .unwrap();
        let session = manager.inner.store.id();
        manager.inner.store.close().await.unwrap();
        let (store, records) = SessionStore::open(root.path(), session).await.unwrap();
        let restored = JobManager::restore(store, &records).await.unwrap();
        let mut args = OutputArgs::new(id);
        args.field = Some(first["preview"]["field"].as_str().unwrap().into());
        args.start = Some(first["preview"]["next_start"].as_u64().unwrap() as usize);
        args.offset = Some(first["preview"]["next_offset"].as_u64().unwrap_or(0) as usize);
        let next = restored
            .present_output(args, &Default::default())
            .await
            .unwrap();
        assert_eq!(next["preview"]["lines"][0], "second");
    }

    #[tokio::test]
    async fn unannotated_results_are_never_truncated() {
        let (_root, manager, id) =
            fixture(json!({"stdout":"x".repeat(92_000),"exit_code":0})).await;
        let view = manager
            .present_output(OutputArgs::new(id), &Default::default())
            .await
            .unwrap();
        assert_eq!(view["result"]["stdout"].as_str().unwrap().len(), 92_000);
        assert!(view.get("preview").is_none());
        assert!(view.get("truncated").is_none());
        let raw = manager.snapshot(id).await.unwrap();
        assert_eq!(
            raw.output.unwrap()["stdout"].as_str().unwrap().len(),
            92_000
        );
        let (_root, manager, id) = fixture(json!({"ok":true,"count":3})).await;
        assert_eq!(
            manager
                .present_output(OutputArgs::new(id), &Default::default())
                .await
                .unwrap()["result"],
            json!({"ok":true,"count":3})
        );
    }

    #[tokio::test]
    async fn unicode_long_lines_and_replayed_ranges_preserve_every_byte() {
        let expected = format!("{}\nlast", "🦀\"\\".repeat(4000));
        let (_root, manager, id) = fixture(json!({"content":expected})).await;
        let mut args = OutputArgs::new(id);
        args.field = Some("/result/content".into());
        let mut reconstructed = String::new();
        let mut previous_line = 1;
        loop {
            let view = manager
                .present_output(args.clone(), &Default::default())
                .await
                .unwrap();
            let replay = manager
                .present_output(args.clone(), &Default::default())
                .await
                .unwrap();
            assert_eq!(view, replay);
            assert!(serde_json::to_vec(&view).unwrap().len() <= PAGE_BYTES);
            for (index, line) in view["preview"]["lines"]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
            {
                let number = args.start.unwrap_or(1) + index;
                if number > previous_line {
                    reconstructed.push('\n');
                }
                reconstructed.push_str(line.as_str().unwrap());
                previous_line = number;
            }
            let Some(start) = view["preview"]["next_start"].as_u64() else {
                break;
            };
            args = OutputArgs::new(id);
            args.field = Some(view["preview"]["field"].as_str().unwrap().into());
            args.start = Some(start as usize);
            args.offset = Some(view["preview"]["next_offset"].as_u64().unwrap_or(0) as usize);
        }
        assert_eq!(reconstructed, expected);
    }

    #[tokio::test]
    async fn search_reaches_beyond_preview_and_merges_context_across_pages() {
        let text = (1..=500)
            .map(|n| {
                format!(
                    "{} {n}\n",
                    if n == 450 || n == 452 { "ERROR" } else { "ok" }
                )
            })
            .collect::<String>();
        let (_root, manager, id) = fixture(json!({"stdout":text})).await;
        let mut args = OutputArgs::new(id);
        args.field = Some("/result/stdout".into());
        args.pattern = Some("(?i)error".into());
        args.context = Some(2);
        args.limit = Some(2);
        let mut numbers = Vec::new();
        loop {
            let view = manager
                .present_output(args, &Default::default())
                .await
                .unwrap();
            numbers.extend(
                view["preview"]["lines"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|line| line.as_str().unwrap().to_owned()),
            );
            let Some(start) = view["preview"]["next_start"].as_u64() else {
                break;
            };
            args = OutputArgs::new(id);
            args.field = Some(view["preview"]["field"].as_str().unwrap().into());
            args.start = Some(start as usize);
            args.offset = Some(view["preview"]["next_offset"].as_u64().unwrap_or(0) as usize);
            args.limit = Some(2);
            args.pattern = Some("(?i)error".into());
            args.context = Some(2);
        }
        assert_eq!(numbers, text.lines().skip(447).take(7).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn output_inspection_returns_immediately_for_running_jobs_without_output() {
        let runtime = crate::test_support::TestRuntime::new().await;
        let lease = runtime
            .jobs
            .create(JobSpec::test(runtime.agent.clone(), "pending"))
            .await
            .unwrap();
        runtime
            .jobs
            .transition(lease.id, JobState::Running)
            .await
            .unwrap();
        let view = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            runtime
                .jobs
                .present_output(OutputArgs::new(lease.id), &Default::default()),
        )
        .await
        .expect("inspection must not wait for a job event")
        .unwrap();
        assert_eq!(view["state"], "running");
        assert_eq!(view["preview"]["lines"], json!([]));
        runtime
            .jobs
            .finish(
                lease.id,
                JobOutcome::Completed(ToolOutput::new(Value::Null)),
            )
            .await
            .unwrap();
    }

    #[test]
    fn output_schema_defaults_are_optional_and_obsolete_arguments_are_rejected() {
        let schema = serde_json::to_value(schemars::schema_for!(OutputArgs)).unwrap();
        assert_eq!(schema["required"], json!(["job"]));
        assert!(schema["properties"].get("cursor").is_none());
        assert!(schema["properties"].get("wait").is_none());
        assert_eq!(schema["additionalProperties"], false);
        for wait in [json!(null), json!(0), json!(0.5), json!(5)] {
            assert!(serde_json::from_value::<OutputArgs>(json!({"job":1,"wait":wait})).is_err());
        }
        assert_eq!(
            view_schema(&Default::default())["properties"]["preview"]["properties"]["lines"]["items"],
            json!({"type":"string"})
        );
        for (field, default) in [("start", 1), ("offset", 0), ("limit", 100)] {
            assert_eq!(schema["properties"][field]["default"], default);
        }
        assert!(serde_json::from_value::<OutputArgs>(json!({"job":1,"cursor":"old"})).is_err());
        let args: OutputArgs = serde_json::from_value(json!({"job":1})).unwrap();
        assert_eq!(args.start, None);
    }
}
