//! Saved output presentation, independent of model providers and execution transports.
mod truncation;
use super::{JobError, JobManager, JobState};
use crate::{
    identity::JobId,
    tool::{ToolError, policy::CapabilitySet},
};
use base64::Engine as _;
use grep_matcher::Matcher as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Duration,
};

pub(crate) const PAGE_BYTES: usize = 8 * 1024;
pub(crate) const CONTENT_BYTES: usize = PAGE_BYTES - 2048;

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct OutputArgs {
    pub job: JobId,
    /// JSON Pointer in saved content, e.g. /result/stdout, /result/content, /console.
    pub field: Option<String>,
    /// One-based first source line.
    #[schemars(range(min = 1))]
    pub start: Option<usize>,
    /// Maximum returned lines, including match context.
    #[schemars(range(min = 1, max = 1000), extend("default" = 100))]
    pub limit: Option<usize>,
    /// Case-sensitive line regex; use (?i) for case-insensitive matching.
    pub pattern: Option<String>,
    /// Surrounding lines per match.
    #[schemars(range(min = 0, max = 20))]
    pub context: Option<usize>,
    /// Continue a previous selection. Do not combine with field/start/pattern/context.
    pub cursor: Option<String>,
    /// Seconds to wait for output, a question, or completion.
    #[schemars(range(min = 0, max = 3600), extend("default" = 0))]
    pub wait: Option<u64>,
    #[serde(skip)]
    #[schemars(skip)]
    pub cancellation: Option<super::CancellationToken>,
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
            cursor: None,
            wait: None,
            cancellation: None,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct Cursor {
    session: String,
    job: JobId,
    field: String,
    pattern: Option<String>,
    context: usize,
    start: usize,
    byte: u64,
    line: usize,
    column: usize,
    after: usize,
}
impl Cursor {
    fn encode(&self, directory: &Path) -> Result<String, ToolError> {
        let bytes = serde_json::to_vec(self)?;
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(&bytes));
        std::fs::create_dir_all(directory)?;
        let path = directory.join(format!("cursor-{token}.json"));
        if !path.exists() {
            let mut file = tempfile::NamedTempFile::new_in(directory)?;
            file.write_all(&bytes)?;
            file.flush()?;
            file.persist(path).map_err(|e| ToolError::Io(e.error))?;
        }
        Ok(token)
    }
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
    if value.is_array() && field_file(directory, field).exists() {
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

    pub(crate) async fn present_output(
        &self,
        args: OutputArgs,
        capabilities: &CapabilitySet,
    ) -> Result<Value, ToolError> {
        let limit = args.limit.unwrap_or(100);
        if !(1..=1000).contains(&limit)
            || args.start == Some(0)
            || args.context.unwrap_or(0) > 20
            || args.wait.unwrap_or(0) > 3600
        {
            return Err(ToolError::InvalidArguments(
                "limit must be 1-1000, start positive, context 0-20, and wait 0-3600".into(),
            ));
        }
        if args.context.is_some() && args.pattern.is_none() && args.cursor.is_none() {
            return Err(ToolError::InvalidArguments(
                "context requires pattern".into(),
            ));
        }
        let explicit = args.field.is_some()
            || args.start.is_some()
            || args.pattern.is_some()
            || args.cursor.is_some();
        let session = self.inner.store.id().to_string();
        let mut cursor = if let Some(encoded) = &args.cursor {
            if args.field.is_some()
                || args.start.is_some()
                || args.pattern.is_some()
                || args.context.is_some()
            {
                return Err(ToolError::InvalidArguments(
                    "cursor cannot be combined with field/start/pattern/context".into(),
                ));
            }
            if encoded.len() != 43
                || !encoded
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(ToolError::InvalidArguments("invalid output cursor".into()));
            }
            let path = self
                .output_directory(args.job)
                .join(format!("cursor-{encoded}.json"));
            let decoded = tokio::fs::read(path).await.map_err(|_| {
                ToolError::InvalidArguments("cursor does not exist for this job".into())
            })?;
            let c: Cursor = serde_json::from_slice(&decoded)
                .map_err(|_| ToolError::InvalidArguments("invalid output cursor".into()))?;
            if c.session != session || c.job != args.job {
                return Err(ToolError::InvalidArguments(
                    "cursor belongs to another session or job".into(),
                ));
            }
            c
        } else {
            Cursor {
                session,
                job: args.job,
                field: args.field.clone().unwrap_or_else(|| "/result".into()),
                pattern: args.pattern.clone(),
                context: args.context.unwrap_or(0),
                start: args.start.unwrap_or(1),
                byte: 0,
                line: 1,
                column: 0,
                after: 0,
            }
        };
        if !cursor.field.is_empty() && !cursor.field.starts_with('/') {
            return Err(ToolError::InvalidArguments(
                "field must be a JSON Pointer".into(),
            ));
        }
        // Compile before waiting, including when no output exists yet.
        if let Some(pattern) = &cursor.pattern {
            crate::tool::builtins::search::output_matcher(pattern)?;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(args.wait.unwrap_or(0));
        loop {
            let (mut envelope, notify, output_schema) = {
                let jobs = self.inner.jobs.lock().await;
                let entry = jobs
                    .get(&args.job)
                    .ok_or_else(|| ToolError::Failed(format!("unknown job {}", args.job)))?;
                (
                    entry.envelope(args.job),
                    entry.notify.clone().notified_owned(),
                    entry.output_schema.clone().unwrap_or(Value::Bool(true)),
                )
            };
            let terminal = envelope.state.is_terminal();
            let question = envelope.state == JobState::WaitingInput;
            let live_question = envelope.output.take();
            let mut view = envelope.presented(capabilities)?;
            let map = view.as_object_mut().expect("job envelope is an object");
            map.remove("output");
            map.remove("console_output");
            let directory = self.output_directory(args.job);
            let document_path = directory.join("document.json");
            let structured = !explicit && terminal && document_path.exists();
            let mut presented_question = false;
            if structured {
                let directory = directory.clone();
                let c = cursor.clone();
                let cancellation = args.cancellation.clone().unwrap_or_default();
                let projection = tokio::task::spawn_blocking(move || {
                    truncation::project(&directory, c, &output_schema, &cancellation)
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
                    let key = base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .encode(Sha256::digest(&bytes));
                    let field = format!("/questions/{key}");
                    let path = field_file(&directory, &field);
                    tokio::fs::create_dir_all(&directory).await?;
                    if !path.exists() {
                        tokio::fs::write(path, bytes).await?;
                    }
                    if !explicit || (args.cursor.is_none() && args.field.is_none()) {
                        cursor.field = field.clone();
                    }
                    presented_question = cursor.field == field;
                }
            }
            if !structured && !map.contains_key("question") {
                if !explicit && !question && document_path.exists() {
                    cursor.field = String::new();
                }
                let directory = directory.clone();
                let c = cursor.clone();
                let cancellation = args.cancellation.clone().unwrap_or_default();
                let unavailable = !terminal
                    && !question
                    && (!explicit
                        || envelope.location.target != "root"
                        || c.field == "/result"
                        || c.field.is_empty());
                let page = tokio::task::spawn_blocking(move || {
                    if unavailable { return Ok(json!({"field":c.field,"lines":[],"next":c.encode(&directory)?,"capture_complete":false})); }
                    let closed = terminal || c.field.starts_with("/questions/");
                    page(&directory, c, limit, closed, &cancellation)
                })
                .await
                .map_err(|e| ToolError::Failed(e.to_string()))??;
                let mut page = page;
                if terminal {
                    let complete = tokio::fs::read(&document_path)
                        .await
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                        .and_then(|v| v.get("capture_complete").and_then(Value::as_bool))
                        .unwrap_or(false);
                    page["capture_complete"] = json!(complete);
                }
                map.insert("preview".into(), page);
            }
            let has_content = structured
                || question
                || map
                    .get("preview")
                    .and_then(|p| p.get("lines"))
                    .and_then(Value::as_array)
                    .is_some_and(|lines| !lines.is_empty());
            if terminal || question || has_content || tokio::time::Instant::now() >= deadline {
                if (terminal && !cursor.field.starts_with("/questions/")) || presented_question {
                    self.claim(args.job)
                        .await
                        .map_err(|e| ToolError::Failed(e.to_string()))?;
                }
                return Ok(view);
            }
            if let Some(cancellation) = &args.cancellation {
                tokio::select! { () = cancellation.cancelled() => return Err(ToolError::Cancelled), _ = tokio::time::timeout_at(deadline, notify) => {} }
            } else {
                let _ = tokio::time::timeout_at(deadline, notify).await;
            }
        }
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
                envelope.console_output = value
                    .get("console")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
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
    mut cursor: Cursor,
    limit: usize,
    terminal: bool,
    cancellation: &super::CancellationToken,
) -> Result<Value, ToolError> {
    let path = ensure_field_file(directory, &cursor.field, cancellation)?;
    page_file(directory, &path, &mut cursor, limit, terminal, cancellation)
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

fn page_file(
    directory: &Path,
    path: &Path,
    cursor: &mut Cursor,
    limit: usize,
    terminal: bool,
    cancellation: &super::CancellationToken,
) -> Result<Value, ToolError> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !terminal => {
            return Ok(
                json!({"field":cursor.field,"lines":[],"next":cursor.encode(directory)?,"capture_complete":false}),
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(
                json!({"field":cursor.field,"lines":[],"next":null,"capture_complete":false}),
            );
        }
        Err(error) => return Err(error.into()),
    };
    let matcher = cursor
        .pattern
        .as_deref()
        .map(crate::tool::builtins::search::output_matcher)
        .transpose()?;
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(cursor.byte))?;
    while cursor.line < cursor.start {
        if cancellation.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let bytes = reader.fill_buf()?;
        if bytes.is_empty() {
            break;
        }
        let length = if let Some(index) = bytes.iter().position(|byte| *byte == b'\n') {
            cursor.line += 1;
            cursor.column = 0;
            index + 1
        } else {
            cursor.column += bytes.len();
            bytes.len()
        };
        reader.consume(length);
    }
    cursor.byte = reader.stream_position()?;
    let mut lines = Vec::new();
    let mut bytes = 0;
    let mut exhausted = false;
    let mut lookahead = VecDeque::<(u64, String, bool)>::new();
    loop {
        if cancellation.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        if lines.len() >= limit || bytes + 256 >= CONTENT_BYTES {
            break;
        }
        if let Some(matcher) = &matcher {
            while lookahead.len() <= cursor.context {
                let mut raw = Vec::new();
                // A matcher resource limit, not a capture limit: never silently skip a long line.
                (&mut reader)
                    .take(4 * 1024 * 1024 + 1)
                    .read_until(b'\n', &mut raw)?;
                if raw.len() > 4 * 1024 * 1024 {
                    return Err(ToolError::Failed(
                        "regex source line exceeds 4 MiB; read this field without a pattern".into(),
                    ));
                }
                if raw.is_empty() || (!terminal && !raw.ends_with(b"\n")) {
                    break;
                }
                let text = String::from_utf8(raw)
                    .map_err(|_| ToolError::Failed("saved output is not UTF-8".into()))?;
                let matched = matcher
                    .is_match(text.trim_end_matches(['\r', '\n']).as_bytes())
                    .map_err(|e| ToolError::Failed(e.to_string()))?;
                lookahead.push_back((reader.stream_position()?, text, matched));
            }
            if lookahead.is_empty() {
                exhausted = true;
                break;
            }
            if !terminal && lookahead.len() <= cursor.context {
                break;
            }
            let selected = cursor.after > 0 || lookahead.iter().any(|(_, _, matched)| *matched);
            let (end, raw, matched) = lookahead.front().expect("nonempty lookahead");
            if selected {
                let text = raw.trim_end_matches(['\r', '\n']);
                if cursor.column > text.len() || !text.is_char_boundary(cursor.column) {
                    return Err(ToolError::InvalidArguments(
                        "invalid output cursor position".into(),
                    ));
                }
                let mut stop = (cursor.column + (CONTENT_BYTES - bytes - 256) / 6).min(text.len());
                while !text.is_char_boundary(stop) {
                    stop -= 1;
                }
                if stop == cursor.column && stop < text.len() {
                    break;
                }
                let row = json!({"line":cursor.line,"offset":cursor.column,"text":&text[cursor.column..stop],"matched":matched});
                bytes += serde_json::to_vec(&row)?.len();
                lines.push(row);
                if stop < text.len() {
                    cursor.column = stop;
                    break;
                }
            }
            cursor.after = if *matched {
                cursor.context
            } else {
                cursor.after.saturating_sub(1)
            };
            cursor.byte = *end;
            cursor.line += 1;
            cursor.column = 0;
            lookahead.pop_front();
        } else {
            let maximum = (CONTENT_BYTES - bytes - 256) / 6;
            if maximum < 4 {
                break;
            }
            let mut raw = Vec::new();
            (&mut reader)
                .take(maximum as u64)
                .read_until(b'\n', &mut raw)?;
            if raw.is_empty() {
                exhausted = true;
                break;
            }
            let newline = raw.ends_with(b"\n");
            let valid = match std::str::from_utf8(&raw) {
                Ok(_) => raw.len(),
                Err(error) if error.error_len().is_none() => error.valid_up_to(),
                Err(_) => return Err(ToolError::Failed("saved output is not UTF-8".into())),
            };
            if valid < raw.len() {
                reader.seek(SeekFrom::Current(
                    -i64::try_from(raw.len() - valid).unwrap(),
                ))?;
                raw.truncate(valid);
            }
            if raw.is_empty() {
                break;
            }
            let text = std::str::from_utf8(&raw).expect("validated UTF-8");
            let row = json!({"line":cursor.line,"offset":cursor.column,"text":if newline { text.strip_suffix('\n').unwrap_or(text).strip_suffix('\r').unwrap_or(text.strip_suffix('\n').unwrap_or(text)) } else { text },"matched":false});
            bytes += serde_json::to_vec(&row)?.len();
            lines.push(row);
            cursor.byte = reader.stream_position()?;
            if newline {
                cursor.line += 1;
                cursor.column = 0;
            } else {
                cursor.column += raw.len();
            }
        }
    }
    if matcher.is_none() && reader.fill_buf()?.is_empty() {
        exhausted = true;
    }
    Ok(
        json!({"field":cursor.field,"lines":lines,"next":if !exhausted || !terminal {Some(cursor.encode(directory)?)} else {None},"capture_complete":terminal}),
    )
}

pub(crate) fn view_schema(capabilities: &CapabilitySet) -> Value {
    let mut schema = super::presented_job_schema(capabilities, false);
    let properties = schema["properties"]
        .as_object_mut()
        .expect("envelope properties");
    properties.remove("output");
    properties.remove("console_output");
    for name in ["result", "question"] {
        properties.insert(name.into(), json!({}));
    }
    properties.insert("console".into(), json!({"type":"string"}));
    properties.insert("capture_complete".into(), json!({"type":"boolean"}));
    properties.insert(
        "truncated".into(),
        json!({"type":"array","items":{
            "type":"object","properties":{"field":{"type":"string"},"next":{"type":"string"}},
            "required":["field","next"]
        }}),
    );
    properties.insert("preview".into(), json!({"type":"object","properties":{
        "field":{"type":"string"},"lines":{"type":"array","items":{"type":"object","properties":{
            "line":{"type":"integer"},"offset":{"type":"integer"},"text":{"type":"string"},"matched":{"type":"boolean"}
        },"required":["line","offset","text","matched"]}},"next":{"type":["string","null"]},"capture_complete":{"type":"boolean"}
    },"required":["field","lines","next","capture_complete"]}));
    if let Some(required) = schema["required"].as_array_mut() {
        required.retain(|v| v != "output" && v != "console_output");
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
            .finish(lease.id, JobOutcome::Completed(ToolOutput::new(value)))
            .await
            .unwrap();
        (root, manager, lease.id)
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
        let old_cursor = first["preview"]["next"].as_str().unwrap().to_owned();
        manager.resume_input(job).await.unwrap();
        manager
            .request_input(job, json!({"question_id":"second","text":"new question"}))
            .await
            .unwrap();
        let mut args = OutputArgs::new(job);
        args.cursor = Some(old_cursor);
        let page = manager
            .present_output(args, &Default::default())
            .await
            .unwrap();
        assert!(
            page["preview"]["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| line["text"].as_str().unwrap().contains("aaaa"))
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
    async fn saved_cursors_survive_session_resume() {
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
        args.cursor = Some(first["preview"]["next"].as_str().unwrap().into());
        let next = restored
            .present_output(args, &Default::default())
            .await
            .unwrap();
        assert_eq!(next["preview"]["lines"][0]["text"], "second");
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
    async fn unicode_long_lines_and_replayed_cursors_preserve_every_byte() {
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
            for line in view["preview"]["lines"].as_array().unwrap() {
                let number = line["line"].as_u64().unwrap();
                if number > previous_line {
                    reconstructed.push('\n');
                }
                reconstructed.push_str(line["text"].as_str().unwrap());
                previous_line = number;
            }
            let Some(cursor) = view["preview"]["next"].as_str() else {
                break;
            };
            args = OutputArgs::new(id);
            args.cursor = Some(cursor.into());
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
                    .map(|line| line["line"].as_u64().unwrap()),
            );
            let Some(cursor) = view["preview"]["next"].as_str() else {
                break;
            };
            args = OutputArgs::new(id);
            args.cursor = Some(cursor.into());
            args.limit = Some(2);
        }
        assert_eq!(numbers, (448..=454).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn invalid_and_foreign_cursors_are_rejected() {
        let (_root, manager, id) = fixture(json!({"x":"a\nb\nc"})).await;
        let mut args = OutputArgs::new(id);
        args.field = Some("/result/x".into());
        args.limit = Some(1);
        let page = manager
            .present_output(args, &Default::default())
            .await
            .unwrap();
        let cursor = page["preview"]["next"].as_str().unwrap().to_owned();
        let mut args = OutputArgs::new(id);
        args.cursor = Some(cursor.clone());
        args.start = Some(1);
        assert!(
            manager
                .present_output(args, &Default::default())
                .await
                .is_err()
        );
        let (_root2, manager2, id2) = fixture(json!(null)).await;
        let mut args = OutputArgs::new(id2);
        args.cursor = Some(cursor);
        assert!(
            manager2
                .present_output(args, &Default::default())
                .await
                .is_err()
        );
    }
}
