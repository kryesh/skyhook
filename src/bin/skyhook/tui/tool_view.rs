//! Presentation-only tool documents. Nothing here writes to session or tool state.
use super::{format::push_clean, model, theme::ContentTheme};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    hash::{DefaultHasher, Hash, Hasher},
    ops::Deref,
    sync::{Arc, OnceLock, mpsc},
    thread,
};
use syntect::{
    easy::HighlightLines, highlighting::Theme, parsing::SyntaxSet, util::LinesWithEndings,
};
use unicode_width::UnicodeWidthStr;

const CACHE_SECTIONS: usize = 128;
// Source-byte admission budget (not an accounting of allocated token/span output).
const CACHE_BYTES: usize = 8 * 1024 * 1024;

pub(super) const MAX_SECTION: usize = 256 * 1024;
const MAX_LINE: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Plain,
    ToolName,
    Indicator,
    Target,
    Success,
    Warning,
    Error,
    Heading,
    Label,
    String,
    Number,
    Constant,
    Muted,
    Removed,
    Added,
}
impl Role {
    fn style(self, theme: ContentTheme) -> Style {
        let color = match self {
            Self::Plain | Self::ToolName => theme.fg,
            Self::Indicator => theme.primary,
            Self::Target => theme.accent,
            Self::Success => theme.success,
            Self::Warning => theme.warning,
            Self::Error => theme.error,
            Self::Heading => theme.heading,
            Self::Label => theme.secondary,
            Self::String | Self::Added => theme.success,
            Self::Number => theme.accent,
            Self::Constant => theme.primary,
            Self::Muted => theme.muted,
            Self::Removed => theme.error,
        };
        let style = Style::default().fg(color);
        if matches!(self, Self::Heading | Self::Label | Self::ToolName) {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    text: String,
    role: Role,
}
impl Run {
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    pub(super) fn new(text: impl Into<String>, role: Role) -> Self {
        Self {
            text: text.into(),
            role,
        }
    }
}
/// Render structured presentation metadata without inferring semantics from its text.
pub fn header_line(runs: &[Run], light: bool) -> Line<'static> {
    let theme = ContentTheme::new(light);
    Line::from(
        runs.iter()
            .map(|run| Span::styled(model::clean(&run.text), run.role.style(theme)))
            .collect::<Vec<_>>(),
    )
}

/// Immutable source with content metadata computed once, not on each UI cache lookup.
/// Clones share their allocation; independently rebuilt documents still reuse highlights.
#[derive(Clone, Debug)]
pub struct CodeSource {
    text: Arc<str>,
    digest: u64,
    eligible: bool,
}
impl From<&str> for CodeSource {
    fn from(text: &str) -> Self {
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        Self {
            text: Arc::from(text),
            digest: hasher.finish(),
            eligible: text.len() <= MAX_SECTION
                && text.split('\n').all(|line| line.len() <= MAX_LINE),
        }
    }
}
impl Deref for CodeSource {
    type Target = str;
    fn deref(&self) -> &str {
        &self.text
    }
}
impl AsRef<str> for CodeSource {
    fn as_ref(&self) -> &str {
        &self.text
    }
}
impl PartialEq for CodeSource {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.text, &other.text)
            || (self.digest == other.digest && self.text == other.text)
    }
}
impl Eq for CodeSource {}
impl Hash for CodeSource {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.digest.hash(state);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Section {
    Line(Vec<Run>),
    /// Prose arguments retain their literal text, but wrap at word boundaries.
    Prose(Vec<Run>),
    Code {
        source: CodeSource,
        language: String,
        indent: usize,
        gutters: Vec<String>,
        role: Role,
    },
}
/// Layout intent travels with each logical line; it is never inferred from text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wrap {
    Hard,
    Words,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    pub sections: Vec<Section>,
}
// Restrict deduplication to structured error/message fields, not arbitrary
// stdout or source text that happens to contain the same words.
fn contains_error(value: &Value, error: &str) -> bool {
    value.as_str().is_some_and(|text| text == error)
        || ["error", "message", "result", "output"].iter().any(|key| {
            value
                .get(key)
                .is_some_and(|value| contains_error(value, error))
        })
}

fn complete_document_preview(output: &Value) -> Option<Value> {
    let preview = output.get("preview")?;
    let lines = preview.get("lines")?.as_array()?;
    // Core's reader reports the selected field's total source lines. Reaching
    // EOF alone also describes a final page or filtered read, not a whole read.
    // The empty JSON Pointer selects the public saved {error, result} document.
    if preview["field"].as_str() != Some("")
        || !preview["next_start"].is_null()
        || !preview["next_offset"].is_null()
        || preview["total_lines"].as_u64() != Some(lines.len() as u64)
    {
        return None;
    }
    let source = lines
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<_>>>()?
        .join("\n");
    let document = json_container(&source)?;
    document.is_object().then_some(document)
}

fn document_has_error(document: &Value, error: &Value) -> bool {
    ["/error", "/result/error"]
        .iter()
        .any(|pointer| document.pointer(pointer) == Some(error))
}

impl Document {
    pub fn line(&mut self, text: impl Into<String>, role: Role) {
        for line in text.into().split('\n') {
            self.sections
                .push(Section::Line(vec![Run::new(line, role)]));
        }
    }
    fn code(
        &mut self,
        text: &str,
        language: &str,
        indent: usize,
        gutters: Vec<String>,
        role: Role,
    ) {
        self.sections.push(Section::Code {
            source: CodeSource::from(text),
            language: language.into(),
            indent,
            gutters,
            role,
        });
    }
    pub fn arguments(&mut self, tool: &str, args: &Value) {
        self.line("Arguments", Role::Heading);
        if args.as_object().is_some_and(|v| v.is_empty()) {
            self.line("  No arguments", Role::Muted);
        } else {
            self.fields(args, 2, (tool, args), false);
        }
    }
    fn fields(&mut self, value: &Value, indent: usize, context: (&str, &Value), hard: bool) {
        let entries: Vec<(String, &Value)> = match value {
            Value::Object(values) => values
                .iter()
                .map(|(key, value)| (model::clean(key).replace('\n', " "), value))
                .collect(),
            Value::Array(values) => values
                .iter()
                .enumerate()
                .map(|(index, value)| (format!("{}.", index + 1), value))
                .collect(),
            _ => {
                self.argument_block(value, indent, hard);
                return;
            }
        };
        let width = entries
            .iter()
            .map(|(name, _)| name.width())
            .max()
            .unwrap_or(0)
            .min(24);
        for (name, value) in entries {
            let prefix = " ".repeat(indent);
            let code = argument_language(context.0, &name, context.1);
            let hard = hard || code.is_some() || matches!(name.as_str(), "argv" | "commands");
            if let Some(language) = code
                && let Some(source) = value.as_str()
            {
                let role = match name.as_str() {
                    "old" => Role::Removed,
                    "new" => Role::Added,
                    _ => Role::Plain,
                };
                let label = match name.as_str() {
                    "old" => "old (removed)",
                    "new" => "new (added)",
                    "patch" => "patch (requested)",
                    _ => &name,
                };
                self.line(format!("{prefix}{label}"), Role::Label);
                let gutter = match role {
                    Role::Removed => "− ",
                    Role::Added => "+ ",
                    _ => "",
                };
                let gutters = source.split('\n').map(|_| gutter.into()).collect();
                self.code(source, &language, indent + 2, gutters, role);
            } else if let Some((text, role)) = scalar(value) {
                if text.contains('\n') {
                    self.line(format!("{prefix}{name}"), Role::Label);
                    self.argument_block(value, indent + 2, hard);
                } else {
                    let runs = vec![
                        Run::new(format!("{prefix}{name}"), Role::Label),
                        Run::new(
                            " ".repeat(width.saturating_sub(name.width()) + 2),
                            Role::Plain,
                        ),
                        Run::new(text, role),
                    ];
                    self.sections.push(if value.is_string() && !hard {
                        Section::Prose(runs)
                    } else {
                        Section::Line(runs)
                    });
                }
            } else {
                self.line(format!("{prefix}{name}"), Role::Label);
                self.fields(value, indent + 2, context, hard);
            }
        }
    }
    fn argument_block(&mut self, value: &Value, indent: usize, hard: bool) {
        if value.is_string() && !hard {
            if let Some((text, role)) = scalar(value) {
                for line in text.split('\n') {
                    self.sections.push(Section::Prose(vec![
                        Run::new(" ".repeat(indent), Role::Plain),
                        Run::new(line, role),
                    ]));
                }
            }
        } else {
            self.scalar_block(value, indent);
        }
    }
    fn scalar_block(&mut self, value: &Value, indent: usize) {
        if let Some((text, role)) = scalar(value) {
            self.code(&text, "", indent, vec![], role);
        }
    }
    pub fn output(&mut self, tool: &str, args: &Value, output: &Value) {
        self.output_with_error(tool, args, Some(output), None);
    }
    /// Error summaries belong to the expanded Output section, never the header.
    /// Keep downloaded results intact and omit an identical summary already
    /// represented by a structured error field in that result.
    pub fn output_with_error(
        &mut self,
        tool: &str,
        args: &Value,
        output: Option<&Value>,
        error: Option<&str>,
    ) {
        self.line("Output", Role::Heading);
        let whole_preview = output.and_then(complete_document_preview);
        if let Some(error) = error
            && !output.is_some_and(|output| contains_error(output, error))
            && !whole_preview
                .as_ref()
                .is_some_and(|preview| document_has_error(preview, &Value::String(error.into())))
        {
            self.error(&Value::String(error.into()));
        }
        if let Some(output) = output {
            self.output_body(tool, args, output, whole_preview.as_ref());
        }
    }
    fn error(&mut self, error: &Value) {
        if let Some(text) = error.as_str() {
            if let Some(value) = json_container(text) {
                self.code(&model::pretty(&value), "json", 2, vec![], Role::Error);
            } else {
                self.code(text, "", 2, vec![], Role::Error);
            }
        } else {
            self.code(&model::pretty(error), "json", 2, vec![], Role::Error);
        }
    }
    fn output_body(
        &mut self,
        tool: &str,
        args: &Value,
        output: &Value,
        whole_preview: Option<&Value>,
    ) {
        let mut shown_errors = Vec::new();
        for pointer in ["/error", "/result/error"] {
            if let Some(error) = output.pointer(pointer).filter(|value| !value.is_null())
                && !shown_errors.contains(&error)
                && !whole_preview.is_some_and(|preview| document_has_error(preview, error))
            {
                self.error(error);
                shown_errors.push(error);
            }
        }
        if let Some(preview) = output.get("preview") {
            let field = preview["field"].as_str().unwrap_or_default();
            self.line(
                if field.is_empty() {
                    "Complete result"
                } else {
                    field
                },
                Role::Muted,
            );
            let lines = preview["lines"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            let source = lines
                .iter()
                .map(|line| line.as_str().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n");
            let mut language = output_language(tool, args, field);
            let json = ((language.is_empty() && preview["next_start"].is_null())
                || (language == "json" && field != "/result/content"))
                .then(|| json_container(&source))
                .flatten();
            if language.is_empty() && preview["next_start"].is_null() && json.is_some() {
                language = "json".into();
            }
            let source = if language == "json"
                && field != "/result/content"
                && lines.iter().all(Value::is_string)
            {
                json.as_ref()
                    .and_then(|value| serde_json::to_string_pretty(value).ok())
                    .unwrap_or(source)
            } else {
                source
            };
            self.code(&source, &language, 2, vec![], Role::Plain);
            self.line(
                if preview["next_start"].is_u64() {
                    "More saved output available"
                } else {
                    "End of available output"
                },
                Role::Muted,
            );
        } else {
            // Split literal text fields out of the display copy; the original Value is untouched.
            let mut metadata = output.clone();
            for pointer in ["/error", "/result/error"] {
                if output
                    .pointer(pointer)
                    .is_some_and(|value| !value.is_null())
                {
                    let (parent, key) = pointer.rsplit_once('/').unwrap();
                    if let Some(object) =
                        metadata.pointer_mut(parent).and_then(Value::as_object_mut)
                    {
                        object.remove(key);
                    }
                }
            }
            let mut text_fields = Vec::new();
            for (pointer, label) in [
                ("/result/content", "File content"),
                ("/result/stdout", "stdout"),
                ("/result/stderr", "stderr"),
                ("/result/console", "Console"),
            ] {
                if let Some(text) = output.pointer(pointer).and_then(Value::as_str) {
                    let (parent, key) = pointer.rsplit_once('/').unwrap();
                    if let Some(object) =
                        metadata.pointer_mut(parent).and_then(Value::as_object_mut)
                    {
                        object.remove(key);
                    }
                    text_fields.push((pointer, label, text));
                }
            }
            if let Some(text) = output.as_str() {
                self.result_text(text, "");
            } else if !metadata.as_object().is_some_and(|object| object.is_empty()) {
                self.code(&model::pretty(&metadata), "json", 2, vec![], Role::Plain);
            }
            for (pointer, label, text) in text_fields {
                self.line(label, Role::Label);
                let language = output_language(tool, args, pointer);
                if pointer == "/result/content" {
                    self.code(text, &language, 2, vec![], Role::Plain);
                } else {
                    self.result_text(text, &language);
                }
            }
        }
    }
    fn result_text(&mut self, text: &str, language: &str) {
        if let Some(value) = json_container(text) {
            self.code(&model::pretty(&value), "json", 2, vec![], Role::Plain);
        } else {
            self.code(text, language, 2, vec![], Role::Plain);
        }
    }
    pub fn plain_text(&self) -> String {
        let mut text = String::new();
        let mut first = true;
        for section in &self.sections {
            match section {
                Section::Line(runs) | Section::Prose(runs) => {
                    if !first {
                        text.push('\n');
                    }
                    first = false;
                    for run in runs {
                        push_clean(&mut text, &run.text);
                    }
                }
                Section::Code {
                    source,
                    indent,
                    gutters,
                    ..
                } => {
                    for (index, line) in source.split('\n').enumerate() {
                        if !first {
                            text.push('\n');
                        }
                        first = false;
                        text.extend(std::iter::repeat_n(' ', *indent));
                        if let Some(gutter) = gutters.get(index) {
                            push_clean(&mut text, gutter);
                        }
                        push_clean(&mut text, line);
                    }
                }
            }
        }
        text
    }
    pub fn lines(&self, cache: Option<&HighlightCache>, light: bool) -> Vec<Line<'static>> {
        self.layout_lines(cache, light)
            .into_iter()
            .map(|(line, _)| line)
            .collect()
    }
    /// Styled logical lines and their presentation-only reflow policy.
    pub fn layout_lines(
        &self,
        cache: Option<&HighlightCache>,
        light: bool,
    ) -> Vec<(Line<'static>, Wrap)> {
        let p = ContentTheme::new(light);
        let mut lines = Vec::new();
        for section in &self.sections {
            match section {
                Section::Line(runs) => lines.push((header_line(runs, light), Wrap::Hard)),
                Section::Prose(runs) => lines.push((header_line(runs, light), Wrap::Words)),
                Section::Code {
                    source,
                    language,
                    indent,
                    gutters,
                    role,
                } => {
                    let highlighted = cache.and_then(|cache| {
                        cache.ready(&CodeKey::new(source.clone(), language, light))
                    });
                    // Split without trimming: empty lines and trailing whitespace are significant.
                    for (index, text) in source.split('\n').enumerate() {
                        let mut spans = vec![Span::raw(" ".repeat(*indent))];
                        if let Some(gutter) = gutters.get(index) {
                            spans.push(Span::styled(
                                model::clean(gutter),
                                if matches!(role, Role::Added | Role::Removed) {
                                    role.style(p)
                                } else {
                                    Role::Muted.style(p)
                                },
                            ));
                        }
                        if let Some(line) = highlighted.and_then(|lines| lines.get(index)) {
                            spans.extend(line.spans.iter().cloned().map(|mut span| {
                                // Unsupported syntax completes with raw spans. Keep that
                                // cached completion, but inherit this caller's fallback role
                                // rather than turning coloured pending text neutral.
                                if span.style.fg.is_none() {
                                    span.style = role.style(p).patch(span.style);
                                }
                                span
                            }));
                        } else {
                            spans.push(Span::styled(model::clean(text), role.style(p)));
                        }
                        lines.push((Line::from(spans), Wrap::Hard));
                    }
                }
            }
        }
        lines
    }
    /// Content IDs for indexing highlight completions back to affected documents.
    /// A digest collision only causes an extra invalidation; cache equality verifies text.
    pub fn highlight_sources(&self) -> impl Iterator<Item = u64> + '_ {
        self.sections.iter().filter_map(|section| match section {
            Section::Code {
                source, language, ..
            } if source.eligible && !language.is_empty() => Some(source.digest),
            _ => None,
        })
    }
    fn keys(&self, light: bool) -> impl Iterator<Item = CodeKey> + '_ {
        self.sections
            .iter()
            .filter_map(move |section| match section {
                Section::Code {
                    source, language, ..
                } if source.eligible && !language.is_empty() => {
                    Some(CodeKey::new(source.clone(), language, light))
                }
                _ => None,
            })
    }
}
fn json_container(text: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(text).ok()?;
    matches!(value, Value::Object(_) | Value::Array(_)).then_some(value)
}

fn scalar(value: &Value) -> Option<(String, Role)> {
    Some(match value {
        Value::String(text) => (
            if text.is_empty() {
                "(empty text)".into()
            } else {
                text.clone()
            },
            Role::String,
        ),
        Value::Null => ("none".into(), Role::Constant),
        Value::Bool(value) => (value.to_string(), Role::Constant),
        Value::Number(value) => (value.to_string(), Role::Number),
        Value::Array(values) if values.is_empty() => ("(empty list)".into(), Role::Muted),
        Value::Object(values) if values.is_empty() => ("(empty object)".into(), Role::Muted),
        _ => return None,
    })
}
fn file_language(args: &Value) -> String {
    let path = args["path"].as_str().unwrap_or_default();
    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    match name {
        "Dockerfile" => "Dockerfile".into(),
        "Makefile" => "Makefile".into(),
        _ => name
            .rsplit_once('.')
            .map_or("", |(_, ext)| ext)
            .to_ascii_lowercase(),
    }
}
fn argument_language(tool: &str, field: &str, args: &Value) -> Option<String> {
    match (tool, field) {
        ("script", "source") => Some("js".into()),
        ("shell", "command") => Some("sh".into()),
        ("write", "content") | ("replace", "old" | "new") => Some(file_language(args)),
        (_, "patch" | "diff") => Some("diff".into()),
        // These fields contain literal executable/source/file data, including
        // in nested argument objects. Unknown languages still use hard wrapping.
        (_, "command" | "source" | "script" | "code" | "content" | "old" | "new") => {
            Some(String::new())
        }
        _ => None,
    }
}
fn output_language(tool: &str, args: &Value, field: &str) -> String {
    match field {
        "/result/content" if tool == "read" => file_language(args),
        "" | "/result" => "json".into(),
        _ => String::new(),
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CodeKey {
    source: CodeSource,
    language: String,
    light: bool,
}
impl CodeKey {
    fn new(source: CodeSource, language: &str, light: bool) -> Self {
        Self {
            source,
            language: language.into(),
            light,
        }
    }
}
struct CachedHighlight {
    last_used: u64,
    pending: bool,
    lines: Option<Vec<Line<'static>>>,
}
struct Completion {
    generation: u64,
    key: CodeKey,
    lines: Vec<Line<'static>>,
}
/// A bounded worker queue keeps regex work out of the terminal/event loop.
/// Recent sections survive collapse within a bounded cache; old-session results are discarded.
pub struct HighlightCache {
    entries: HashMap<CodeKey, CachedHighlight>,
    working_set: HashSet<CodeKey>,
    sender: Option<mpsc::SyncSender<(u64, CodeKey)>>,
    receiver: mpsc::Receiver<Completion>,
    generation: u64,
    clock: u64,
    changed_sources: Vec<u64>,
}
#[cfg(test)]
impl Default for HighlightCache {
    fn default() -> Self {
        Self::with_notify(tokio::sync::mpsc::unbounded_channel().0)
    }
}
impl HighlightCache {
    pub fn with_notify(notify: tokio::sync::mpsc::UnboundedSender<super::app::Work>) -> Self {
        let (sender, jobs) = mpsc::sync_channel::<(u64, CodeKey)>(16);
        let (completed, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("tui-highlight".into())
            .spawn(move || {
                // Load grammars while the initial UI is being displayed, not on the first click.
                let _ = syntax_resources();
                while let Ok((generation, key)) = jobs.recv() {
                    let lines = highlight(&key);
                    if completed
                        .send(Completion {
                            generation,
                            key,
                            lines,
                        })
                        .is_err()
                    {
                        break;
                    }
                    let _ = notify.send(super::app::Work::HighlightsReady);
                }
            });
        Self {
            entries: HashMap::new(),
            working_set: HashSet::new(),
            sender: worker.ok().map(|_| sender),
            receiver,
            generation: 0,
            clock: 0,
            changed_sources: Vec::new(),
        }
    }
    pub fn clear(&mut self) {
        self.entries.clear();
        self.working_set.clear();
        self.changed_sources.clear();
        self.generation += 1;
    }
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        for completion in self.receiver.try_iter() {
            if completion.generation == self.generation
                && let Some(entry) = self.entries.get_mut(&completion.key)
            {
                self.changed_sources.push(completion.key.source.digest);
                entry.lines = Some(completion.lines);
                changed = true;
            }
        }
        // In-flight entries from an old theme/working set cannot be evicted.
        // Their completions may release the budget for desired keys which were
        // never admitted by prepare. Retry those keys without requiring another
        // document invalidation; the selected working set is itself bounded.
        if changed {
            let missing: Vec<_> = self
                .working_set
                .iter()
                .filter(|key| !self.entries.contains_key(*key))
                .cloned()
                .collect();
            self.schedule(missing.into_iter());
        }
        // Queue pressure must not strand admitted sections when the renderer
        // subsequently prepares only changed documents. This bounded cache owns
        // retrying its unscheduled work independently of view invalidations.
        if let Some(sender) = &self.sender {
            for (key, entry) in &mut self.entries {
                if !entry.pending && entry.lines.is_none() {
                    if sender.try_send((self.generation, key.clone())).is_err() {
                        break;
                    }
                    entry.pending = true;
                }
            }
        }
        changed
    }
    /// Drain content IDs whose highlights completed since the last drain.
    /// Call after poll/prepare; IDs may repeat or refer to either theme.
    pub fn take_changed_sources(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.changed_sources)
    }
    pub fn prepare<'a>(
        &mut self,
        documents: impl Iterator<Item = &'a Document>,
        light: bool,
    ) -> bool {
        let changed = self.poll();
        // Pick a stable working set before eviction. More open sections than the budget
        // must not evict and requeue one another after every worker completion.
        self.working_set.clear();
        let mut selected = Vec::new();
        let mut bytes = 0;
        for key in documents.flat_map(|document| document.keys(light)) {
            if self.working_set.contains(&key) {
                continue;
            }
            if selected.len() >= CACHE_SECTIONS || bytes + key.source.len() > CACHE_BYTES {
                continue;
            }
            bytes += key.source.len();
            self.working_set.insert(key.clone());
            selected.push(key);
        }
        self.schedule(selected.into_iter());
        changed
    }
    fn schedule(&mut self, keys: impl Iterator<Item = CodeKey>) {
        let mut bytes: usize = self.entries.keys().map(|key| key.source.len()).sum();
        for key in keys {
            self.clock += 1;
            if !self.entries.contains_key(&key) {
                // Retain recent sections across collapse and theme previews, with a fixed budget.
                while self.entries.len() >= CACHE_SECTIONS || bytes + key.source.len() > CACHE_BYTES
                {
                    let oldest = self
                        .entries
                        .iter()
                        .filter(|(key, entry)| {
                            !self.working_set.contains(*key)
                                && (!entry.pending || entry.lines.is_some())
                        })
                        .min_by_key(|(_, entry)| entry.last_used)
                        .map(|(key, _)| key.clone());
                    let Some(oldest) = oldest else {
                        break;
                    };
                    bytes -= oldest.source.len();
                    self.entries.remove(&oldest);
                }
                if self.entries.len() >= CACHE_SECTIONS || bytes + key.source.len() > CACHE_BYTES {
                    continue;
                }
                bytes += key.source.len();
            }
            let entry = self.entries.entry(key.clone()).or_insert(CachedHighlight {
                pending: false,
                lines: None,
                last_used: self.clock,
            });
            entry.last_used = self.clock;
            if !entry.pending
                && let Some(sender) = &self.sender
                && sender.try_send((self.generation, key)).is_ok()
            {
                entry.pending = true;
            }
        }
    }
    fn ready(&self, key: &CodeKey) -> Option<&Vec<Line<'static>>> {
        self.entries.get(key)?.lines.as_ref()
    }
    #[cfg(test)]
    pub fn is_highlighted(&self, document: &Document, light: bool) -> bool {
        document.keys(light).any(|key| self.ready(&key).is_some())
    }
    #[cfg(test)]
    pub fn is_fully_highlighted(&self, document: &Document, light: bool) -> bool {
        document.keys(light).peekable().peek().is_some()
            && document.keys(light).all(|key| self.ready(&key).is_some())
    }
}
struct SyntaxResources {
    syntaxes: SyntaxSet,
    dark: Theme,
    light: Theme,
}
fn syntax_resources() -> &'static SyntaxResources {
    static RESOURCES: OnceLock<SyntaxResources> = OnceLock::new();
    RESOURCES.get_or_init(|| SyntaxResources {
        syntaxes: SyntaxSet::load_defaults_newlines(),
        dark: ContentTheme::new(false).syntax_theme(),
        light: ContentTheme::new(true).syntax_theme(),
    })
}

/// Shared, foreground-only syntax service for code fences and tools.
/// Returns None for unknown languages, oversized sources/lines, or parse failures;
/// callers should render their usual neutral fallback. Like the tool worker, run
/// this off the UI thread: size limits bound input, not regex execution time.
/// Grammars and both themes are initialized once and shared across workers.
pub fn highlight_code(source: &str, language: &str, light: bool) -> Option<Vec<Line<'static>>> {
    if source.len() > MAX_SECTION
        || source.split('\n').any(|line| line.len() > MAX_LINE)
        || language.is_empty()
    {
        return None;
    }
    highlight_source(source, language, light, syntax_resources())
}

fn highlight(key: &CodeKey) -> Vec<Line<'static>> {
    highlight_code(&key.source, &key.language, key.light).unwrap_or_else(|| {
        key.source
            .split('\n')
            .map(|text| Line::from(model::clean(text)))
            .collect()
    })
}

fn highlight_source(
    source: &str,
    language: &str,
    light: bool,
    resources: &SyntaxResources,
) -> Option<Vec<Line<'static>>> {
    if language.is_empty() {
        return None;
    }
    let syntaxes = &resources.syntaxes;
    let syntax = syntaxes
        .find_syntax_by_extension(language)
        .or_else(|| syntaxes.find_syntax_by_token(language))?;
    let theme = if light {
        &resources.light
    } else {
        &resources.dark
    };
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut lines = Vec::new();
    for source_line in LinesWithEndings::from(source) {
        let Ok(tokens) = highlighter.highlight_line(source_line, syntaxes) else {
            return None;
        };
        let mut spans = Vec::new();
        for (style, text) in tokens {
            let mut rendered = Style::default().fg(Color::Rgb(
                style.foreground.r,
                style.foreground.g,
                style.foreground.b,
            ));
            if style
                .font_style
                .contains(syntect::highlighting::FontStyle::BOLD)
            {
                rendered = rendered.add_modifier(Modifier::BOLD);
            }
            if style
                .font_style
                .contains(syntect::highlighting::FontStyle::ITALIC)
            {
                rendered = rendered.add_modifier(Modifier::ITALIC);
            }
            if style
                .font_style
                .contains(syntect::highlighting::FontStyle::UNDERLINE)
            {
                rendered = rendered.add_modifier(Modifier::UNDERLINED);
            }
            spans.push(Span::styled(
                model::clean(text.strip_suffix('\n').unwrap_or(text)),
                rendered,
            ));
        }
        lines.push(Line::from(spans));
    }
    if source.ends_with('\n') || source.is_empty() {
        lines.push(Line::default());
    }
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    fn queued_cache(
        capacity: usize,
    ) -> (
        HighlightCache,
        mpsc::Receiver<(u64, CodeKey)>,
        mpsc::Sender<Completion>,
    ) {
        let (sender, queued) = mpsc::sync_channel(capacity);
        let (completed, receiver) = mpsc::channel();
        (
            HighlightCache {
                entries: HashMap::new(),
                working_set: HashSet::new(),
                sender: Some(sender),
                receiver,
                generation: 0,
                clock: 0,
                changed_sources: Vec::new(),
            },
            queued,
            completed,
        )
    }

    #[test]
    fn argument_wrapping_is_semantic_and_retains_nested_context() {
        let args = json!({
            "prompt": "Keep **literal** Markdown and words intact.\n\n  Next paragraph  ",
            "description": "A short description",
            "nested": {"text": "Ordinary nested text\n  still prose", "items": ["array prose"]},
            "argv": ["printf", "  %s\t%s\n", "one two three"],
            "commands": [{"value": "  raw command arguments  "}],
            "raw": {"source": "  let value = 42;\n", "command": "echo  exact\twords"},
            "content": "  file contents  \n\n",
            "patch": "@@ -1 +1 @@\n-old\n+new\n"
        });
        let original = args.clone();
        let mut document = Document::default();
        document.arguments("exec", &args);
        let original_document = document.clone();
        for light in [false, true] {
            let lines = document.layout_lines(None, light);
            let text = |line: &Line<'_>| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            };
            assert_eq!(
                lines
                    .iter()
                    .map(|(line, _)| text(line))
                    .collect::<Vec<_>>()
                    .join("\n"),
                document.plain_text()
            );
            assert_eq!(
                lines
                    .iter()
                    .map(|(line, _)| line.clone())
                    .collect::<Vec<_>>(),
                document.lines(None, light)
            );
            for marker in [
                "**literal**",
                "Next paragraph",
                "short description",
                "Ordinary nested",
                "still prose",
                "array prose",
            ] {
                let (_, wrapping) = lines
                    .iter()
                    .find(|(line, _)| text(line).contains(marker))
                    .unwrap();
                assert_eq!(*wrapping, Wrap::Words, "{marker}");
            }
            for marker in [
                "printf",
                "%s",
                "one two three",
                "raw command arguments",
                "let value",
                "echo",
                "file contents",
                "@@",
            ] {
                let (_, wrapping) = lines
                    .iter()
                    .find(|(line, _)| text(line).contains(marker))
                    .unwrap();
                assert_eq!(*wrapping, Wrap::Hard, "{marker}");
            }
        }
        assert_eq!(args, original);
        assert_eq!(document, original_document);
        // Prose is not sent to the syntax worker, even if it looks like markup.
        let mut prose = Document::default();
        prose.arguments("agent", &json!({"prompt": "```js\nconst x = 1;\n```"}));
        assert_eq!(prose.highlight_sources().count(), 0);
    }

    #[test]
    fn error_outputs_keep_structured_details_and_exact_source_without_duplicate_summaries() {
        let output = json!({
            "error": "Permission was denied",
            "code": "permission_denied", "executed": false,
            "result": {"stdout": "  exact\toutput\n\n", "error": "Permission was denied"}
        });
        let before = output.clone();
        let mut document = Document::default();
        document.output_with_error(
            "exec",
            &Value::Null,
            Some(&output),
            Some("Permission was denied"),
        );
        let text = document.plain_text();
        assert!(text.starts_with("Output\n  Permission was denied"));
        assert_eq!(text.matches("Permission was denied").count(), 1);
        assert!(text.contains("permission_denied"));
        assert!(text.contains("executed"));
        assert!(document.sections.iter().any(|section| {
            matches!(section, Section::Code { source, .. } if &**source == "  exact\toutput\n\n")
        }));
        assert_eq!(output, before);
        for light in [false, true] {
            let lines = document.lines(None, light);
            let error = lines
                .iter()
                .find(|line| line.to_string().contains("Permission was denied"))
                .unwrap();
            assert_eq!(
                error.spans.last().unwrap().style.fg,
                Some(ContentTheme::new(light).error)
            );
        }
    }

    #[test]
    fn preview_outputs_do_not_hide_the_job_error() {
        let output = json!({
            "error": "failed exactly",
            "preview": {"field": "/result/stdout", "lines": ["partial output"], "next_start": 2}
        });
        let mut document = Document::default();
        document.output_with_error("exec", &Value::Null, Some(&output), Some("failed exactly"));
        let text = document.plain_text();
        assert_eq!(text.matches("failed exactly").count(), 1);
        assert!(text.contains("partial output"));
        let mut structured = Document::default();
        structured.output(
            "exec",
            &Value::Null,
            &json!({"error": {"message": "validation failed", "details": ["argv is required"]}}),
        );
        assert!(structured.plain_text().contains("validation failed"));
        assert!(structured.plain_text().contains("argv is required"));
    }

    fn whole_output_preview(saved: &Value) -> Value {
        let source = serde_json::to_string_pretty(saved).unwrap();
        let lines: Vec<_> = source.lines().collect();
        json!({"field": "", "total_lines": lines.len(), "lines": lines})
    }

    fn error_sources(document: &Document) -> Vec<&str> {
        document
            .sections
            .iter()
            .filter_map(|section| match section {
                Section::Code {
                    source,
                    role: Role::Error,
                    ..
                } => Some(&**source),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn complete_whole_output_preview_owns_identical_error_summaries_without_losing_source() {
        for error in [
            json!("failed exactly"),
            json!({"message": "failed exactly", "code": 7}),
        ] {
            for pointer in ["/error", "/result/error", "both"] {
                let mut saved = json!({"error": null, "result": {
                    "stdout": "  exact\toutput\n\n", "stderr": "retained diagnostics"
                }});
                if pointer != "/result/error" {
                    saved["error"] = error.clone();
                }
                if pointer != "/error" {
                    saved["result"]["error"] = error.clone();
                }
                let output = json!({
                    "error": error, "result": {"error": error},
                    "preview": whole_output_preview(&saved)
                });
                let before = output.clone();
                let mut document = Document::default();
                document.output_with_error("exec", &Value::Null, Some(&output), error.as_str());
                assert!(error_sources(&document).is_empty(), "{pointer}: {error}");
                // Repeated fields in the preview itself are source data, not summaries.
                assert_eq!(
                    document.plain_text().matches("failed exactly").count(),
                    if pointer == "both" { 2 } else { 1 }
                );
                let source = serde_json::to_string_pretty(&saved).unwrap();
                assert!(document.sections.iter().any(|section| {
                    matches!(section, Section::Code { source: shown, .. } if &**shown == source)
                }));
                assert_eq!(output, before);
            }
        }
    }

    #[test]
    fn incomplete_or_unstructured_previews_keep_separate_error_summaries() {
        let complete = whole_output_preview(&json!({"error": "failed exactly", "result": null}));
        for (name, key, value) in [
            ("partial page", "next_start", json!(2)),
            ("byte continuation", "next_offset", json!(10)),
            ("final or filtered page", "total_lines", json!(100)),
            ("unknown total", "total_lines", Value::Null),
            ("selected result", "field", json!("/result")),
            ("selected stdout", "field", json!("/result/stdout")),
            ("unknown field", "field", Value::Null),
        ] {
            let mut preview = complete.clone();
            preview[key] = value;
            let output = json!({"error": "failed exactly", "preview": preview});
            let mut document = Document::default();
            document.output_with_error("exec", &Value::Null, Some(&output), Some("failed exactly"));
            assert_eq!(error_sources(&document), ["failed exactly"], "{name}");
            // The preview remains visible even when it also contains the error.
            assert_eq!(
                document.plain_text().matches("failed exactly").count(),
                2,
                "{name}"
            );
        }
        for source in [
            "{\"error\": \"failed exactly\"",      // Malformed JSON.
            "failed exactly",                      // Prose, not a structured error field.
            "[ {\"error\": \"failed exactly\"} ]", // Not a saved document.
            "\"failed exactly\"",
        ] {
            let output = json!({"error": "failed exactly", "preview": {
                "field": "", "total_lines": 1, "lines": [source]
            }});
            let mut document = Document::default();
            document.output("exec", &Value::Null, &output);
            assert_eq!(error_sources(&document), ["failed exactly"], "{source}");
            assert_eq!(document.plain_text().matches("failed exactly").count(), 2);
            assert_eq!(output["preview"]["lines"][0], source);
        }
    }

    #[test]
    fn whole_output_preview_matches_only_identical_structured_error_fields() {
        for saved in [
            json!({"error": "different error", "result": null}),
            json!({"error": {"message": "failed exactly", "details": "different"}}),
            json!({"result": {"stdout": "failed exactly", "stderr": "failed exactly"}}),
            json!({"result": {"stdout": "{\"error\":\"failed exactly\"}"}}),
            json!({"message": "failed exactly", "result": {"message": "failed exactly"}}),
            json!({"result": "failed exactly"}),
        ] {
            let output =
                json!({"error": "failed exactly", "preview": whole_output_preview(&saved)});
            let mut document = Document::default();
            document.output("exec", &Value::Null, &output);
            assert_eq!(error_sources(&document), ["failed exactly"], "{saved}");
            let source = serde_json::to_string_pretty(&saved).unwrap();
            assert!(document.sections.iter().any(|section| {
                matches!(section, Section::Code { source: shown, role: Role::Plain, .. } if &**shown == source)
            }));
        }
        let output = json!({
            "error": "outer error", "result": {"error": "inner error"},
            "preview": whole_output_preview(&json!({"error": "outer error", "result": null}))
        });
        let mut document = Document::default();
        document.output_with_error("exec", &Value::Null, Some(&output), Some("third error"));
        assert_eq!(error_sources(&document), ["third error", "inner error"]);
        assert_eq!(document.plain_text().matches("outer error").count(), 1);
    }

    #[test]
    fn output_with_error_without_envelope_summary_checks_complete_preview() {
        let mut output = json!({"preview": whole_output_preview(&json!({
            "error": "failed exactly", "result": null
        }))});
        let mut document = Document::default();
        document.output_with_error("exec", &Value::Null, Some(&output), Some("failed exactly"));
        assert!(error_sources(&document).is_empty());
        assert_eq!(document.plain_text().matches("failed exactly").count(), 1);

        output["preview"]["next_start"] = json!(2);
        let mut document = Document::default();
        document.output_with_error("exec", &Value::Null, Some(&output), Some("failed exactly"));
        assert_eq!(error_sources(&document), ["failed exactly"]);

        let mut document = Document::default();
        document.output_with_error("exec", &Value::Null, None, Some("failed exactly"));
        assert_eq!(error_sources(&document), ["failed exactly"]);
    }

    #[test]
    fn unsupported_syntax_completion_preserves_each_callers_fallback_role() {
        let styled_tokens = |lines: &[Line<'_>]| {
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .filter(|span| !span.content.is_empty())
                .map(|span| (span.content.to_string(), span.style))
                .collect::<Vec<_>>()
        };
        for light in [false, true] {
            let (mut cache, queued, completed) = queued_cache(16);
            let mut document = Document::default();
            let original = "unchanged  \n\n";
            document.code(original, "unknown-extension", 0, vec![], Role::Constant);
            let pending = document.lines(None, light);
            cache.prepare(std::iter::once(&document), light);
            let (generation, key) = queued.try_recv().unwrap();
            completed
                .send(Completion {
                    generation,
                    lines: highlight(&key),
                    key,
                })
                .unwrap();
            assert!(cache.poll());
            assert!(cache.is_fully_highlighted(&document, light));
            let rendered = document.lines(Some(&cache), light);
            assert_eq!(text(&rendered), text(&pending));
            assert_eq!(styled_tokens(&rendered), styled_tokens(&pending));

            // The shared key must not bake in the first document's role.
            for role in [Role::Plain, Role::String, Role::Added, Role::Removed] {
                let mut other = Document::default();
                other.code(original, "unknown-extension", 0, vec![], role);
                assert!(!cache.prepare(std::iter::once(&other), light));
                let rendered = other.lines(Some(&cache), light);
                let pending = other.lines(None, light);
                assert_eq!(text(&rendered), text(&pending));
                assert_eq!(styled_tokens(&rendered), styled_tokens(&pending));
                assert!(cache.is_fully_highlighted(&other, light));
            }
            for _ in 0..3 {
                assert!(!cache.prepare(std::iter::once(&document), light));
                assert!(!cache.poll());
                assert!(
                    queued.try_recv().is_err(),
                    "unsupported syntax was requeued"
                );
            }
        }
    }

    #[test]
    fn desired_theme_retries_after_inflight_entries_release_count_and_byte_budgets() {
        for byte_limited in [false, true] {
            let (mut cache, queued, completed) = queued_cache(16);
            let count = if byte_limited {
                CACHE_BYTES / MAX_SECTION
            } else {
                CACHE_SECTIONS
            };
            let documents: Vec<_> = (0..count)
                .map(|index| {
                    let mut document = Document::default();
                    let source = if byte_limited {
                        format!("{index}\n{}", "x\n".repeat((MAX_SECTION - 4) / 2))
                    } else {
                        format!("let value = {index};")
                    };
                    document.code(&source, "rust", 0, vec![], Role::Plain);
                    document
                })
                .collect();
            cache.prepare(documents.iter(), false);
            // Simulate a worker taking all dark jobs, while delaying completions.
            let mut old_jobs = Vec::new();
            for _ in 0..count + 2 {
                old_jobs.extend(queued.try_iter());
                cache.poll();
                if old_jobs.len() == count {
                    break;
                }
            }
            assert_eq!(old_jobs.len(), count);
            assert!(
                cache
                    .entries
                    .values()
                    .all(|entry| entry.pending && entry.lines.is_none())
            );

            cache.prepare(documents.iter(), true);
            assert_eq!(cache.working_set.len(), count);
            assert!(
                cache
                    .working_set
                    .iter()
                    .all(|key| !cache.entries.contains_key(key))
            );
            assert!(queued.try_recv().is_err());

            // Only poll from here: changed-only rendering will not prepare these
            // documents again just because unrelated dark completions arrived.
            for (generation, key) in old_jobs {
                completed
                    .send(Completion {
                        generation,
                        key,
                        lines: vec![Line::from("ready")],
                    })
                    .unwrap();
                assert!(cache.poll());
            }
            for _ in 0..count + 2 {
                for (generation, key) in queued.try_iter() {
                    assert!(key.light);
                    completed
                        .send(Completion {
                            generation,
                            key,
                            lines: vec![Line::from("ready")],
                        })
                        .unwrap();
                }
                cache.poll();
                assert!(cache.entries.len() <= CACHE_SECTIONS);
                assert!(
                    cache
                        .entries
                        .keys()
                        .map(|key| key.source.len())
                        .sum::<usize>()
                        <= CACHE_BYTES
                );
                if documents
                    .iter()
                    .all(|doc| cache.is_fully_highlighted(doc, true))
                {
                    break;
                }
            }
            assert!(
                documents
                    .iter()
                    .all(|doc| cache.is_fully_highlighted(doc, true)),
                "desired theme stranded after old completions (byte_limited={byte_limited})"
            );
            assert!(queued.try_recv().is_err());
            assert!(!cache.poll());
        }
    }

    #[test]
    fn tool_roles_use_explicit_content_theme_in_both_modes() {
        for light in [false, true] {
            let theme = ContentTheme::new(light);
            for (role, color) in [
                (Role::Plain, theme.fg),
                (Role::ToolName, theme.fg),
                (Role::Indicator, theme.primary),
                (Role::Target, theme.accent),
                (Role::Success, theme.success),
                (Role::Warning, theme.warning),
                (Role::Error, theme.error),
                (Role::Heading, theme.heading),
                (Role::Label, theme.secondary),
                (Role::String, theme.success),
                (Role::Number, theme.accent),
                (Role::Constant, theme.primary),
                (Role::Muted, theme.muted),
                (Role::Removed, theme.error),
                (Role::Added, theme.success),
            ] {
                let style = role.style(theme);
                assert_eq!(style.fg, Some(color), "{light}: {role:?}");
                assert_eq!(style.bg, None);
                assert_eq!(
                    style.add_modifier.contains(Modifier::BOLD),
                    matches!(role, Role::Heading | Role::Label | Role::ToolName)
                );
            }
        }
    }

    #[test]
    fn shared_highlighter_maps_tokens_and_preserves_source_in_both_modes() {
        let source = "let value = (true, 42, \"hello\"); // comment\n";
        for light in [false, true] {
            let theme = ContentTheme::new(light);
            let lines = highlight_code(source, "rust", light).unwrap();
            assert_eq!(text(&lines), source);
            let spans: Vec<_> = lines.iter().flat_map(|line| &line.spans).collect();
            assert!(spans.iter().all(|span| span.style.bg.is_none()));
            for (token, color) in [
                ("let", theme.secondary),
                ("true", theme.primary),
                ("42", theme.accent),
                ("hello", theme.success),
                ("comment", theme.muted),
            ] {
                assert!(
                    spans
                        .iter()
                        .any(|span| span.content.contains(token) && span.style.fg == Some(color)),
                    "{light}: {token}: {spans:?}"
                );
            }
        }
    }

    /// Assert source ranges, not just scope names: grammars can classify real
    /// tokens differently than a synthetic scope would suggest.
    fn assert_source_color(lines: &[Line<'_>], needle: &str, expected: Color) {
        let source = text(lines);
        let mut colors = Vec::new();
        for line in lines {
            for span in &line.spans {
                colors.extend(std::iter::repeat_n(span.style.fg, span.content.len()));
                assert_eq!(span.style.bg, None);
            }
            colors.push(None); // line separator
        }
        assert!(source.contains(needle), "missing {needle:?}");
        for (start, _) in source.match_indices(needle) {
            assert!(
                colors[start..start + needle.len()]
                    .iter()
                    .all(|color| *color == Some(expected)),
                "{needle:?} at {start} should be {expected:?}: {lines:?}"
            );
        }
    }

    #[test]
    fn javascript_function_names_use_standard_scopes_in_both_modes() {
        let source = "async function declared() { const result = called(); return object.method(); }\nconst object = { async method() { return declared(); } };\nconst quoted = \"declared() called() method()\"; // declared() called() method()\n";
        for light in [false, true] {
            let theme = ContentTheme::new(light);
            let lines = highlight_code(source, "javascript", light).unwrap();
            assert_eq!(text(&lines), source);
            // Inspect actual source tokens, not only synthetic theme selectors.
            for token in ["declared", "called", "method"] {
                assert_source_color(&lines[..2], token, theme.secondary);
            }
            assert_source_color(&lines[..1], "result", theme.fg);
            let literal_line = &lines[2];
            for (token, color) in [
                ("declared() called() method()", theme.success),
                (" declared() called() method()", theme.muted),
            ] {
                assert!(
                    literal_line
                        .spans
                        .iter()
                        .any(|span| span.content.as_ref() == token && span.style.fg == Some(color)),
                    "{light}: {token}: {literal_line:?}"
                );
            }
        }
    }

    #[test]
    fn shared_highlighter_declines_unrecognised_and_oversized_input() {
        assert!(highlight_code("hello", "not-a-real-language", false).is_none());
        assert!(highlight_code("hello", "", true).is_none());
        assert!(highlight_code(&"x".repeat(MAX_LINE + 1), "rust", false).is_none());
        assert!(highlight_code(&"x\n".repeat(MAX_SECTION / 2 + 1), "rust", true).is_none());
    }

    fn finish(cache: &mut HighlightCache, document: &Document, light: bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            cache.prepare(std::iter::once(document), light);
            if document.keys(light).all(|key| cache.ready(&key).is_some()) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "highlight worker did not complete"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn source(document: &Document, language: &str) -> CodeSource {
        document
            .sections
            .iter()
            .find_map(|section| match section {
                Section::Code {
                    source,
                    language: value,
                    ..
                } if value == language => Some(source.clone()),
                _ => None,
            })
            .unwrap()
    }
    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn plain_text_matches_styled_output_without_losing_empty_lines_or_controls() {
        let mut document = Document::default();
        assert_eq!(document.plain_text(), "");
        document.line("\nheading\t\u{1b}\r", Role::Heading);
        document.sections.push(Section::Line(vec![]));
        document.sections.push(Section::Line(vec![
            Run::new("a\tb", Role::String),
            Run::new("\u{7}界", Role::Number),
        ]));
        document.code(
            "a  \n\t界\r\n",
            "js",
            2,
            vec!["+\t".into(), "\u{1b}− ".into()],
            Role::Added,
        );
        document.code("", "", 0, vec![], Role::Plain);
        for light in [false, true] {
            assert_eq!(document.plain_text(), text(&document.lines(None, light)));
        }
        assert!(document.plain_text().ends_with("\n  \n"));
    }

    /// Run with --release --ignored --nocapture to report routine lookup cost.
    #[test]
    fn admitted_highlights_retry_after_queue_pressure_without_repreparing_documents() {
        let mut cache = HighlightCache::default();
        let mut documents = Vec::new();
        for index in 0..40 {
            let mut doc = Document::default();
            doc.code(
                &format!("let value_{index} = {index};"),
                "rust",
                0,
                Vec::new(),
                Role::Plain,
            );
            documents.push(doc);
        }
        cache.prepare(documents.iter(), false);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < until {
            cache.poll();
            if documents.iter().all(|doc| cache.is_highlighted(doc, false)) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("admitted sections were stranded after the worker queue filled");
    }

    #[test]
    fn highlighted_source_preserves_whitespace_and_never_sets_backgrounds() {
        let original = "const value = {answer: 42};  \n\t// 界 👩‍💻\nreturn value;\n";
        let arguments = json!({"source": original});
        let before = serde_json::to_vec(&arguments).unwrap();
        let mut document = Document::default();
        document.arguments("script", &arguments);
        assert_eq!(&*source(&document, "js"), original);
        let plain = document.plain_text();
        let mut cache = HighlightCache::default();
        let mut dark_colors = None;
        for light in [false, true] {
            finish(&mut cache, &document, light);
            let lines = document.lines(Some(&cache), light);
            assert_eq!(text(&lines), plain);
            let key = document.keys(light).next().unwrap();
            let highlighted = cache.ready(&key).unwrap();
            assert_eq!(text(highlighted), model::clean(original));
            let colors: HashSet<_> = highlighted
                .iter()
                .flat_map(|line| &line.spans)
                .filter_map(|span| span.style.fg)
                .collect();
            assert!(
                colors.len() >= 3,
                "expected syntax colours for code, strings/numbers and comments"
            );
            if light {
                assert_ne!(dark_colors.as_ref().unwrap(), &colors);
            } else {
                dark_colors = Some(colors);
            }
            assert!(
                lines
                    .iter()
                    .flat_map(|line| &line.spans)
                    .all(|span| span.style.bg.is_none())
            );
            let ready = cache.is_highlighted(&document, light);
            assert!(!cache.prepare(std::iter::once(&document), light));
            assert_eq!(cache.is_highlighted(&document, light), ready);
        }
        assert_eq!(serde_json::to_vec(&arguments).unwrap(), before);
    }

    #[test]
    fn oversized_and_unknown_source_falls_back_without_losing_text() {
        let mut cache = HighlightCache::default();
        for original in ["x".repeat(MAX_LINE + 1), "x\n".repeat(MAX_SECTION / 2 + 1)] {
            let mut document = Document::default();
            document.code(&original, "js", 0, vec![], Role::Plain);
            assert!(!cache.prepare(std::iter::once(&document), false));
            assert!(cache.entries.is_empty());
            assert_eq!(text(&document.lines(Some(&cache), false)), original);
        }
        let mut document = Document::default();
        document.code(
            "unchanged  \n\n",
            "unknown-extension",
            0,
            vec![],
            Role::Plain,
        );
        finish(&mut cache, &document, false);
        assert_eq!(
            text(&document.lines(Some(&cache), false)),
            "unchanged  \n\n"
        );
    }

    #[tokio::test]
    async fn worker_wakes_the_ui_and_reopening_reuses_both_theme_variants() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut cache = HighlightCache::with_notify(sender);
        let mut document = Document::default();
        document.code("const value = 42;", "js", 0, vec![], Role::Plain);
        for light in [false, true] {
            cache.prepare(std::iter::once(&document), light);
            let wake = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
                .await
                .unwrap();
            assert!(matches!(
                wake,
                Some(super::super::app::Work::HighlightsReady)
            ));
            assert!(cache.poll());
            assert!(cache.is_highlighted(&document, light));
        }
        cache.prepare(std::iter::empty(), false);
        for light in [false, true] {
            let ready = cache.is_highlighted(&document, light);
            assert!(!cache.prepare(std::iter::once(&document), light));
            assert_eq!(cache.is_highlighted(&document, light), ready);
        }
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn more_open_sections_than_the_cache_budget_do_not_requeue_forever() {
        let (jobs, queued) = mpsc::sync_channel(512);
        let (completed, receiver) = mpsc::channel();
        let mut cache = HighlightCache {
            entries: HashMap::new(),
            working_set: HashSet::new(),
            sender: Some(jobs),
            receiver,
            generation: 0,
            clock: 0,
            changed_sources: Vec::new(),
        };
        let documents: Vec<_> = (0..CACHE_SECTIONS + 10)
            .map(|index| {
                let mut document = Document::default();
                document.code(
                    &format!("const value = {index};"),
                    "js",
                    0,
                    vec![],
                    Role::Plain,
                );
                document
            })
            .collect();
        cache.prepare(documents.iter(), false);
        assert_eq!(cache.entries.len(), CACHE_SECTIONS);
        for (generation, key) in queued.try_iter() {
            completed
                .send(Completion {
                    generation,
                    key,
                    lines: vec![Line::from("ready")],
                })
                .unwrap();
        }
        assert!(cache.prepare(documents.iter(), false));
        for _ in 0..3 {
            assert!(!cache.prepare(documents.iter(), false));
            assert!(queued.try_recv().is_err());
        }
        assert!(
            cache
                .entries
                .keys()
                .map(|key| key.source.len())
                .sum::<usize>()
                <= CACHE_BYTES
        );
        // Switching the working set still admits a newly visible section.
        cache.prepare(std::iter::once(documents.last().unwrap()), false);
        assert_eq!(queued.try_iter().count(), 1);
    }

    #[test]
    fn cache_discards_old_sessions_and_reuses_collapsed_sections() {
        let (sender, receiver) = mpsc::channel();
        let mut cache = HighlightCache {
            entries: HashMap::new(),
            working_set: HashSet::new(),
            sender: None,
            receiver,
            generation: 0,
            clock: 0,
            changed_sources: Vec::new(),
        };
        let mut document = Document::default();
        document.code("return 42;", "js", 0, vec![], Role::Plain);
        let key = document.keys(false).next().unwrap();
        cache.prepare(std::iter::once(&document), false);
        cache.clear();
        cache.prepare(std::iter::once(&document), false);
        sender
            .send(Completion {
                generation: 0,
                key: key.clone(),
                lines: vec![Line::from("stale")],
            })
            .unwrap();
        assert!(!cache.prepare(std::iter::once(&document), false));
        assert!(!cache.is_highlighted(&document, false));
        sender
            .send(Completion {
                generation: 1,
                key,
                lines: vec![Line::from("collapsed")],
            })
            .unwrap();
        assert!(cache.prepare(std::iter::empty(), false));
        let ready = cache.is_highlighted(&document, false);
        assert!(ready);
        assert!(!cache.prepare(std::iter::once(&document), false));
        assert_eq!(cache.is_highlighted(&document, false), ready);
    }
}
