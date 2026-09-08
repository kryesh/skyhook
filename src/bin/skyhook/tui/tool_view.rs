//! Presentation-only tool documents. Nothing here writes to session or tool state.
use super::{format::push_clean, model, render::Palette};
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
    easy::HighlightLines, highlighting::ThemeSet, parsing::SyntaxSet, util::LinesWithEndings,
};
use unicode_width::UnicodeWidthStr;

const CACHE_SECTIONS: usize = 128;
const CACHE_BYTES: usize = 8 * 1024 * 1024;

const MAX_SECTION: usize = 256 * 1024;
const MAX_LINE: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Plain,
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
    fn style(self, p: Palette) -> Style {
        let color = match self {
            Self::String | Self::Added => {
                if p.fg == Palette::new(true).fg {
                    Color::Rgb(55, 105, 45)
                } else {
                    Color::Rgb(163, 190, 140)
                }
            }
            Self::Number => {
                if p.fg == Palette::new(true).fg {
                    Color::Rgb(135, 75, 145)
                } else {
                    Color::Rgb(180, 142, 173)
                }
            }
            Self::Constant => p.accent,
            Self::Muted => p.muted,
            Self::Removed => p.error,
            _ => p.fg,
        };
        let style = Style::default().fg(color);
        if matches!(self, Self::Heading | Self::Label) {
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
    fn new(text: impl Into<String>, role: Role) -> Self {
        Self {
            text: text.into(),
            role,
        }
    }
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
    Code {
        source: CodeSource,
        language: String,
        indent: usize,
        gutters: Vec<String>,
        role: Role,
    },
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    pub sections: Vec<Section>,
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
            self.fields(args, 2, Some((tool, args)));
        }
    }
    fn fields(&mut self, value: &Value, indent: usize, context: Option<(&str, &Value)>) {
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
                self.scalar_block(value, indent);
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
            let code = context.and_then(|(tool, args)| argument_language(tool, &name, args));
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
                    self.code(&text, "", indent + 2, vec![], role);
                } else {
                    self.sections.push(Section::Line(vec![
                        Run::new(format!("{prefix}{name}"), Role::Label),
                        Run::new(
                            " ".repeat(width.saturating_sub(name.width()) + 2),
                            Role::Plain,
                        ),
                        Run::new(text, role),
                    ]));
                }
            } else {
                self.line(format!("{prefix}{name}"), Role::Label);
                self.fields(value, indent + 2, None);
            }
        }
    }
    fn scalar_block(&mut self, value: &Value, indent: usize) {
        if let Some((text, role)) = scalar(value) {
            self.code(&text, "", indent, vec![], role);
        }
    }
    pub fn output(&mut self, tool: &str, args: &Value, output: &Value) {
        self.line("Output", Role::Heading);
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
            } else {
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
                Section::Line(runs) => {
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
        let p = Palette::new(light);
        let mut lines = Vec::new();
        for section in &self.sections {
            match section {
                Section::Line(runs) => lines.push(Line::from(
                    runs.iter()
                        .map(|run| Span::styled(model::clean(&run.text), run.role.style(p)))
                        .collect::<Vec<_>>(),
                )),
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
                            spans.extend(line.spans.iter().cloned());
                        } else {
                            spans.push(Span::styled(model::clean(text), role.style(p)));
                        }
                        lines.push(Line::from(spans));
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
        (_, "patch") => Some("diff".into()),
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
                static RESOURCES: OnceLock<(SyntaxSet, ThemeSet)> = OnceLock::new();
                // Load grammars while the initial UI is being displayed, not on the first click.
                let (syntaxes, themes) = RESOURCES.get_or_init(|| {
                    (
                        SyntaxSet::load_defaults_newlines(),
                        ThemeSet::load_defaults(),
                    )
                });
                while let Ok((generation, key)) = jobs.recv() {
                    let lines = highlight(&key, syntaxes, themes);
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
}
fn highlight(key: &CodeKey, syntaxes: &SyntaxSet, themes: &ThemeSet) -> Vec<Line<'static>> {
    let fallback = || {
        key.source
            .split('\n')
            .map(|text| Line::from(model::clean(text)))
            .collect()
    };
    if !key.source.eligible || key.language.is_empty() {
        return fallback();
    }
    let syntax = syntaxes
        .find_syntax_by_extension(&key.language)
        .or_else(|| syntaxes.find_syntax_by_token(&key.language));
    let Some(syntax) = syntax else {
        return fallback();
    };
    let theme_name = if key.light {
        "base16-ocean.light"
    } else {
        "base16-ocean.dark"
    };
    let Some(theme) = themes.themes.get(theme_name) else {
        return fallback();
    };
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut lines = Vec::new();
    for source_line in LinesWithEndings::from(key.source.as_ref()) {
        let Ok(tokens) = highlighter.highlight_line(source_line, syntaxes) else {
            return fallback();
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
    if key.source.ends_with('\n') || key.source.is_empty() {
        lines.push(Line::default());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

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
