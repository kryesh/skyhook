//! Build semantic documents from tool arguments and result envelopes.
use super::output::OutputShape;
use super::preview::{Pagination, PreviewView};
use super::{CodeSource, Document, OutputView, Role, Run, Section, model};
use crate::tui::format::pretty;
use serde_json::Value;
use skyhook::job::omit_null_fields;
use unicode_width::UnicodeWidthStr;

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
    pub(super) fn code(
        &mut self,
        text: &str,
        language: &str,
        indent: usize,
        gutters: Option<String>,
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
            self.fields(args, 2, (tool, args), &ArgumentPolicy::Prose);
        }
    }
    fn fields(
        &mut self,
        value: &Value,
        indent: usize,
        context: (&str, &Value),
        inherited: &ArgumentPolicy,
    ) {
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
                self.argument_block(value, indent, inherited);
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
            let explicit = argument_policy(context.0, &name, context.1);
            let policy = explicit.as_ref().unwrap_or(inherited);
            if let Some(ArgumentPolicy::Literal {
                language,
                block: true,
            }) = &explicit
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
                    _ => &name,
                };
                self.line(format!("{prefix}{label}"), Role::Label);
                let gutter = match role {
                    Role::Removed => "− ",
                    Role::Added => "+ ",
                    _ => "",
                };
                let gutters = (!gutter.is_empty()).then(|| gutter.into());
                self.code(
                    source,
                    language.as_deref().unwrap_or(""),
                    indent + 2,
                    gutters,
                    role,
                );
            } else if let Some((text, role)) = scalar(value) {
                if text.contains('\n') {
                    self.line(format!("{prefix}{name}"), Role::Label);
                    self.argument_block(value, indent + 2, policy);
                } else {
                    let runs = vec![
                        Run::new(format!("{prefix}{name}"), Role::Label),
                        Run::new(
                            " ".repeat(width.saturating_sub(name.width()) + 2),
                            Role::Plain,
                        ),
                        Run::new(text, role),
                    ];
                    self.sections.push(
                        if value.is_string() && matches!(policy, ArgumentPolicy::Prose) {
                            Section::Prose(runs)
                        } else {
                            Section::Line(runs)
                        },
                    );
                }
            } else {
                self.line(format!("{prefix}{name}"), Role::Label);
                self.fields(value, indent + 2, context, policy);
            }
        }
    }
    fn argument_block(&mut self, value: &Value, indent: usize, policy: &ArgumentPolicy) {
        if value.is_string() && matches!(policy, ArgumentPolicy::Prose) {
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
            self.code(&text, "", indent, None, role);
        }
    }
    /// Historical output is borrowed: the shape is validated from the caller's
    /// value without copying the whole result.
    pub fn output(&mut self, tool: &str, args: &Value, output: &Value) {
        let shape = OutputShape::wire(output);
        self.output_section(tool, args, Some((output, &shape)), None);
    }
    /// Error summaries belong to the expanded Output section, never the header.
    /// Keep downloaded results intact and omit an identical summary already
    /// represented by a structured error field in that result.
    pub fn output_with_error(
        &mut self,
        tool: &str,
        args: &Value,
        output: Option<&OutputView>,
        error: Option<&str>,
    ) {
        let output = output.map(|output| (output.value(), output.shape()));
        self.output_section(tool, args, output, error);
    }
    fn output_section(
        &mut self,
        tool: &str,
        args: &Value,
        output: Option<(&Value, &OutputShape)>,
        error: Option<&str>,
    ) {
        self.line("Output", Role::Heading);
        let preview = output.and_then(|(_, shape)| shape.preview.as_ref());
        let whole_preview = preview.and_then(PreviewView::complete_document);
        let value = output.map(|(value, _)| value);
        let shown_summary = error.filter(|summary| {
            !value.is_some_and(|output| contains_error(output, summary))
                && !whole_preview.is_some_and(|preview| {
                    ["/error", "/result/error"].iter().any(|pointer| {
                        preview.pointer(pointer).and_then(Value::as_str) == Some(*summary)
                    })
                })
        });
        if let Some(error) = shown_summary {
            self.error_text(error);
        }
        if let Some((value, shape)) = output {
            self.output_body(tool, args, value, shape, shown_summary);
        }
    }
    fn error(&mut self, error: &Value) {
        if let Some(text) = error.as_str() {
            self.error_text(text);
        } else {
            let mut error = error.clone();
            omit_null_fields(&mut error);
            self.code(&pretty(&error), "json", 2, None, Role::Error);
        }
    }
    fn error_text(&mut self, text: &str) {
        if let Some(value) = json_container(text) {
            self.code(&pretty(&value), "json", 2, None, Role::Error);
        } else {
            self.code(text, "", 2, None, Role::Error);
        }
    }
    fn output_body(
        &mut self,
        tool: &str,
        args: &Value,
        output: &Value,
        shape: &OutputShape,
        shown_summary: Option<&str>,
    ) {
        let preview = shape.preview.as_ref();
        let whole_preview = preview.and_then(PreviewView::complete_document);
        let captures = &shape.captures;
        // Envelope notices describe the capture, not the selected payload.
        // In particular, reaching the final preview page does not imply that
        // the original output was captured completely.
        let notice = output
            .pointer("/presentation/notice")
            .and_then(Value::as_str)
            .filter(|notice| !notice.trim().is_empty());
        if let Some(notice) = notice {
            self.line("Notice", Role::Label);
            self.code(notice, "", 2, None, Role::Muted);
        }
        // Only exact structured equality participates in error deduplication.
        let mut shown_errors: Vec<Value> = shown_summary.map(Value::from).into_iter().collect();
        for pointer in ["/error", "/result/error"] {
            if let Some(error) = output.pointer(pointer).filter(|value| !value.is_null())
                && !shown_errors.contains(error)
                && !whole_preview.is_some_and(|preview| document_has_error(preview, error))
            {
                self.error(error);
                shown_errors.push(error.clone());
            }
        }
        let has_capture_previews = captures.iter().any(|capture| capture.preview.is_some());
        if let Some(preview) = preview {
            // Live output may have no saved whole document yet. The selected
            // capture views are more useful than an empty Complete result pane.
            let empty_whole_preview = preview.empty_whole();
            if !has_capture_previews || !empty_whole_preview {
                self.output_preview(tool, args, preview, whole_preview);
            }
        } else {
            // Split literal text fields out of the display copy; the original Value is untouched.
            let mut metadata = output.clone();
            omit_null_fields(&mut metadata);
            // `output` is a TUI-only selected-field view, not capture metadata.
            if let Some(captures) = metadata
                .pointer_mut("/presentation/captures")
                .and_then(Value::as_array_mut)
            {
                for capture in captures {
                    if let Some(capture) = capture.as_object_mut() {
                        capture.remove("output");
                    }
                }
            }
            if notice.is_some()
                && let Some(object) = metadata
                    .get_mut("presentation")
                    .and_then(Value::as_object_mut)
            {
                object.remove("notice");
            }
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
                self.code(&pretty(&metadata), "json", 2, None, Role::Plain);
            }
            for (pointer, label, text) in text_fields {
                self.line(label, Role::Label);
                let language = output_language(tool, args, pointer);
                if pointer == "/result/content" {
                    self.code(text, &language, 2, None, Role::Plain);
                } else {
                    self.result_text(text, &language);
                }
            }
        }
        {
            for capture in captures {
                let mut labeled_error = false;
                for error in &capture.errors {
                    if !shown_errors.contains(error)
                        && !whole_preview.is_some_and(|preview| document_has_error(preview, error))
                    {
                        if !labeled_error {
                            self.line(&capture.field, Role::Label);
                            labeled_error = true;
                        }
                        self.error(error);
                        shown_errors.push(error.clone());
                    }
                }
                if let Some(preview) = &capture.preview {
                    // The parent owns capture notices; only independent read
                    // failures above need additional envelope rendering.
                    self.output_preview(tool, args, preview, None);
                }
            }
        }
    }
    fn output_preview(
        &mut self,
        tool: &str,
        args: &Value,
        preview: &PreviewView,
        whole: Option<&Value>,
    ) {
        let field = preview.field();
        self.line(
            if field.is_empty() {
                "Complete result"
            } else {
                field
            },
            Role::Muted,
        );
        let mut language = output_language(tool, args, field);
        let formatted = if let Some(whole) = whole {
            let mut value = whole.clone();
            omit_null_fields(&mut value);
            Some(pretty(&value))
        } else if field != "/result/content" && (language.is_empty() || language == "json") {
            pretty_json_preview(preview.source(), preview.pagination(), preview.field())
        } else {
            None
        };
        let source = if let Some(formatted) = &formatted {
            language = "json".into();
            formatted
        } else {
            preview.source()
        };
        self.code(source, &language, 2, None, Role::Plain);
        self.line(
            if preview.pagination() != Pagination::End {
                "More saved output available"
            } else {
                "End of available output"
            },
            Role::Muted,
        );
    }

    fn result_text(&mut self, text: &str, language: &str) {
        if let Some(value) = json_container(text) {
            self.code(&pretty(&value), "json", 2, None, Role::Plain);
        } else {
            self.code(text, language, 2, None, Role::Plain);
        }
    }
}
/// Saved-output pages can stop inside a JSON container (or even a string).
/// Format valid prefixes too, without completing them or changing saved source
/// offsets. Non-JSON text and continuation pages that start mid-token stay raw.
fn pretty_json_preview(text: &str, pagination: Pagination, field: &str) -> Option<String> {
    match serde_json::from_str::<Value>(text) {
        Ok(mut value @ (Value::Object(_) | Value::Array(_))) => {
            if field.is_empty() {
                omit_null_fields(&mut value);
            }
            Some(pretty(&value))
        }
        Err(error)
            if pagination != Pagination::End
                && error.is_eof()
                && text.trim_start().starts_with(['{', '[']) =>
        {
            Some(format_json_container_prefix(text))
        }
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum LexState {
    Outside,
    String,
    Escape,
}

/// The parser has already verified that this is a container prefix. Preserve
/// every token, including escapes and unfinished strings; only whitespace
/// outside strings is replaced. Indentation is emitted lazily so a page ending
/// just after an opening delimiter does not acquire fabricated content.
fn format_json_container_prefix(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut depth = 0usize;
    let mut lexical = LexState::Outside;
    let mut newline = false;
    let mut previous = None;
    for ch in text.chars() {
        match lexical {
            LexState::Escape => {
                output.push(ch);
                lexical = LexState::String;
                continue;
            }
            LexState::String => {
                output.push(ch);
                lexical = match ch {
                    '\\' => LexState::Escape,
                    '"' => LexState::Outside,
                    _ => LexState::String,
                };
                continue;
            }
            LexState::Outside => {}
        }
        if ch.is_ascii_whitespace() {
            continue;
        }
        let closing = matches!(ch, '}' | ']');
        let empty = closing && matches!(previous, Some('{' | '['));
        if closing {
            depth = depth.saturating_sub(1);
            newline = !empty;
        }
        if newline && !empty {
            output.push('\n');
            for _ in 0..depth {
                output.push_str("  ");
            }
        }
        newline = false;
        output.push(ch);
        match ch {
            '{' | '[' => {
                depth += 1;
                newline = true;
            }
            ',' => newline = true,
            ':' => output.push(' '),
            '"' => lexical = LexState::String,
            _ => {}
        }
        previous = Some(ch);
    }
    output
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
/// Recursive argument intent; lack of syntax never turns literal data into prose.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ArgumentPolicy {
    Prose,
    /// `block` renders a string value as a labelled code block; argv/commands
    /// strings stay inline like other scalars.
    Literal {
        language: Option<String>,
        block: bool,
    },
}
/// No field override means inherit the ancestor policy, not reset to prose.
fn argument_policy(tool: &str, field: &str, args: &Value) -> Option<ArgumentPolicy> {
    let language = match (tool, field) {
        ("script", "source") => Some("js".into()),
        ("shell", "command") => Some("sh".into()),
        ("write", "content") | ("replace", "old" | "new") => {
            let language = file_language(args);
            (!language.is_empty()).then_some(language)
        }
        (_, "argv" | "commands") => {
            return Some(ArgumentPolicy::Literal {
                language: None,
                block: false,
            });
        }
        (_, "command" | "source" | "script" | "code" | "content" | "old" | "new") => None,
        _ => return None,
    };
    Some(ArgumentPolicy::Literal {
        language,
        block: true,
    })
}
fn output_language(tool: &str, args: &Value, field: &str) -> String {
    match field {
        "/result/content" if tool == "read" => file_language(args),
        "" | "/result" => "json".into(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ContentTheme, Wrap};
    use super::*;
    use serde_json::json;

    fn with_error(output: &Value, error: Option<&str>) -> Document {
        let mut document = Document::default();
        let output = OutputView::historical(output.clone());
        document.output_with_error("exec", &Value::Null, Some(&output), error);
        document
    }

    fn rendered(tool: &str, arguments: &Value, output: &Value) -> Document {
        let mut document = Document::default();
        document.output(tool, arguments, output);
        document
    }

    /// Borrowed historical output must render exactly like an owned view.
    #[test]
    fn borrowed_historical_output_matches_owned_view() {
        for output in [
            json!({"presentation": {"preview": {"next_offset": 0, "field": "", "lines": ["{\"result\": {\"ok\": true}, \"error\": null}"], "total_lines": 1}}}),
            json!({"result": {"stdout": "out", "content": "text"}, "error": "failed", "presentation": {"notice": "Output incomplete."}}),
            json!({"presentation": {"preview": {"next_offset": 0, "field": "/result", "lines": ["[1,"], "next_start": 2}, "captures": [{"field": "/result/a", "output": {"error": "read failed", "presentation": {"preview": {"field": "/result/a", "lines": ["a"], "total_lines": 1}}}}]}}),
            json!("plain text"),
        ] {
            let tool_args = json!({"path": "file.rs"});
            let mut owned = Document::default();
            let view = OutputView::historical(output.clone());
            owned.output_with_error("read", &tool_args, Some(&view), None);
            assert!(rendered("read", &tool_args, &output) == owned, "{output}");
        }
    }

    fn arguments(tool: &str, arguments: Value) -> Document {
        let mut document = Document::default();
        document.arguments(tool, &arguments);
        document
    }

    fn has_code(document: &Document, test: impl Fn(&str, &str, Role) -> bool) -> bool {
        document.sections.iter().any(|section| {
            matches!(section, Section::Code { source, language, role, .. } if test(source, language, *role))
        })
    }

    fn has_source(document: &Document, expected: &str) -> bool {
        has_code(document, |source, _, _| source == expected)
    }

    fn error_sources(document: &Document) -> Vec<&str> {
        let sections = document.sections.iter();
        sections
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

    fn whole_output_preview(saved: &Value) -> Value {
        let source = serde_json::to_string_pretty(saved).unwrap();
        let lines: Vec<_> = source.lines().collect();
        json!({"next_offset": 0, "field": "", "total_lines": lines.len(), "lines": lines})
    }

    fn displayed(saved: &Value) -> String {
        let mut display = saved.clone();
        omit_null_fields(&mut display);
        serde_json::to_string_pretty(&display).unwrap()
    }

    #[test]
    fn argument_wrapping_is_semantic_and_retains_nested_context() {
        let wrap_of = |document: &Document, needle: &str| {
            let lines = document.layout_lines(None);
            let found = lines
                .iter()
                .find(|(line, _)| line.to_string().contains(needle));
            found.unwrap().1
        };
        let document = arguments(
            "exec",
            json!({
                "prompt": "Keep **literal** Markdown.\n\n  Next paragraph  ",
                "nested": {"text": "Nested prose", "items": ["array prose"]},
                "argv": ["printf", "  %s\t%s\n"],
                "commands": [{"value": "  raw command  "}],
                "raw": {"source": "  let value = 42;\n"},
                "content": "  file contents  \n\n"
            }),
        );
        for (marker, expected) in [
            ("**literal**", Wrap::Words),
            ("array prose", Wrap::Words),
            ("printf", Wrap::Hard),
            ("let value", Wrap::Hard),
        ] {
            assert_eq!(wrap_of(&document, marker), expected, "{marker}");
        }
        // Prose is not sent to the syntax worker, even if it looks like markup.
        let prose = arguments("agent", json!({"prompt": "```js\nconst x = 1;\n```"}));
        assert_eq!(prose.highlight_sources().count(), 0);
        // Literal policy needs no known syntax and survives nested scalars.
        let document = arguments(
            "write",
            json!({
                "path": "example.future-language",
                "content": "literal unknown syntax\n",
                "prompt": "prose remains prose",
                "commands": [{"nested": ["literal descendant", 42, true]}],
                "argv": {"nested": {"value": "literal argv descendant"}},
            }),
        );
        assert!(document.sections.iter().any(|section| matches!(section,
            Section::Code { source, language, gutters: None, .. }
                if source.as_ref() == "literal unknown syntax\n" && language == "future-language"
        )));
        for (needle, expected) in [
            ("literal unknown syntax", Wrap::Hard),
            ("prose remains prose", Wrap::Words),
            ("literal descendant", Wrap::Hard),
            ("literal argv descendant", Wrap::Hard),
            ("42", Wrap::Hard),
            ("true", Wrap::Hard),
        ] {
            assert_eq!(wrap_of(&document, needle), expected, "{needle}");
        }
        let changes = arguments("replace", json!({"old": "\n", "new": ""}));
        let markers: Vec<_> = changes
            .sections
            .iter()
            .filter_map(|section| match section {
                Section::Code {
                    gutters: Some(marker),
                    ..
                } => Some(marker.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(markers, ["− ", "+ "]);
    }

    #[test]
    fn error_outputs_keep_structured_details_and_exact_source_without_duplicate_summaries() {
        let output = json!({"error": "Permission was denied", "result": {"stdout": "  exact\toutput\n\n", "error": "Permission was denied"}, "meta": {"code": "permission_denied", "executed": false}});
        let before = output.clone();
        let document = with_error(&output, Some("Permission was denied"));
        let text = document.plain_text();
        assert!(text.starts_with("Output\n  Permission was denied"));
        assert_eq!(text.matches("Permission was denied").count(), 1);
        assert!(text.contains("permission_denied") && text.contains("executed"));
        assert!(has_source(&document, "  exact\toutput\n\n"));
        assert_eq!(output, before);
        let lines = document.lines(None);
        let error = lines
            .iter()
            .find(|line| line.to_string().contains("Permission was denied"));
        let color = error.unwrap().spans.last().unwrap().style.fg;
        assert_eq!(color, Some(ContentTheme::new().error));
        let output =
            json!({"error": {"message": "validation failed", "details": ["argv is required"]}});
        let text = rendered("exec", &Value::Null, &output).plain_text();
        assert!(text.contains("validation failed") && text.contains("argv is required"));
    }

    #[test]
    fn truncated_json_previews_are_formatted_for_scripts_and_other_tools() {
        // The saved reader emits unindented JSON and can cut either at a line
        // boundary or in the middle of a token. Both must remain readable.
        for (tool, field, source, expected) in [
            (
                "script",
                "",
                "{\n\"result\": {\n\"value\": {\n\"items\": [\n{\n\"id\": 1,",
                "{\n  \"result\": {\n    \"value\": {\n      \"items\": [\n        {\n          \"id\": 1,",
            ),
            (
                "glob",
                "",
                "{\"result\":{\"paths\":[\"first.rs\",\"second",
                "{\n  \"result\": {\n    \"paths\": [\n      \"first.rs\",\n      \"second",
            ),
            (
                "exec",
                "/result/stdout",
                "{\"items\":[{},[],{\"name\":\"unfinished",
                "{\n  \"items\": [\n    {},\n    [],\n    {\n      \"name\": \"unfinished",
            ),
        ] {
            for byte_offset in [false, true] {
                let mut output = json!({"presentation": {"preview": {
                    "next_offset": 0, "field": field, "lines": source.split('\n').collect::<Vec<_>>(),
                    "total_lines": 1000, "next_start": 10
                }}});
                if byte_offset {
                    output["presentation"]["preview"]["next_offset"] = json!(200);
                }
                let original = output.clone();
                let document = rendered(tool, &Value::Null, &output);
                let text = document.plain_text();
                let json =
                    |source: &str, language: &str, _| source == expected && language == "json";
                assert!(has_code(&document, json), "{tool}: {text}");
                assert!(text.contains("More saved output available"));
                assert_eq!(output, original);
            }
        }
    }

    #[test]
    fn json_prefix_formatting_preserves_tokens_at_every_character_boundary() {
        let value = json!({"items": [null, true, false, -12.5e20, {}, [], {
            "text": "  spaces\t\n\"escaped\" \\ braces {},[] and unicode é雪"
        }]});
        for source in [
            serde_json::to_string(&value).unwrap(),
            // Preserve unfinished Unicode escapes and both halves of a UTF-16
            // surrogate pair just like ordinary backslash/string boundaries.
            r#" { "escaped": "é雪😀\\\"", "array": [ {}, [] ] } "#.into(),
        ] {
            let expected: Value = serde_json::from_str(&source).unwrap();
            for (end, _) in source.char_indices().skip(1) {
                let prefix = &source[..end];
                let more = Pagination::More {
                    start: 1,
                    offset: prefix.len(),
                };
                let formatted = pretty_json_preview(prefix, more, "");
                // A whitespace-only prefix is not a container; all prefixes
                // beginning at its opening delimiter retain their exact tokens.
                if prefix.trim().is_empty() {
                    assert!(formatted.is_none());
                    continue;
                }
                let restored = formatted.unwrap() + &source[end..];
                let restored = serde_json::from_str::<Value>(&restored).unwrap();
                assert_eq!(restored, expected, "{end}: {source}");
            }
        }
    }

    #[test]
    fn truncated_literal_source_and_non_json_pages_are_not_reformatted() {
        for (tool, field, source, next) in [
            ("read", "/result/content", "{\"items\":[1,2,", true),
            ("read", "/result/content", "{\"items\":[1,2]}", false),
            (
                "exec",
                "/result/stdout",
                "  ordinary output\n  [not JSON",
                true,
            ),
            ("script", "", "\"continuation\": [1, 2,", true),
            ("script", "", "{\"malformed\": nope,", true),
            ("script", "", "{\"unexpected EOF\": [", false),
        ] {
            let lines: Vec<_> = source.split('\n').collect();
            let mut output = json!({"presentation": {"preview": {"next_offset": 0, "field": field, "lines": lines}}});
            if next {
                output["presentation"]["preview"]["next_start"] = json!(2);
            }
            let document = rendered(tool, &json!({"path": "data.json"}), &output);
            assert!(has_source(&document, source), "{tool}: {source}");
        }
    }

    #[test]
    fn output_notices_render_once_for_structured_and_paged_payloads() {
        for (preview, footer) in [
            (Value::Null, None),
            (
                json!({"next_offset": 0, "field": "/result/stdout", "lines": ["payload"]}),
                Some("End of available output"),
            ),
            (
                json!({"next_offset": 0, "field": "/result/stdout", "lines": ["payload"], "next_start": 2}),
                Some("More saved output available"),
            ),
        ] {
            let output = json!({"result": {"stdout": "payload"}, "presentation": {"notice": "Output incomplete.", "preview": preview}});
            let text = rendered("exec", &Value::Null, &output).plain_text();
            assert_eq!(text.matches("Output incomplete.").count(), 1);
            assert_eq!(text.matches("payload").count(), 1);
            assert!(footer.is_none_or(|footer| text.contains(footer)));
        }
    }

    #[test]
    fn live_captures_keep_unique_read_errors_without_duplicate_envelopes_or_empty_panes() {
        for preview in [
            Value::Null,
            json!({"next_offset": 0, "field": "", "lines": [], "total_lines": 0}),
        ] {
            let output = json!({"state": "running", "result": null, "error": "parent failure", "presentation": {"preview": preview, "notice": "Output incomplete.", "captures": [{"field": "/result/custom", "kind": "text", "complete": false, "output": {"error": "parent failure", "presentation": {"notice": "Output incomplete.", "preview": {"field": "/result/custom", "lines": ["  live payload\t"], "next_start": 2, "next_offset": 0}}}}, {"field": "/result/missing", "output": {"error": "field read failed"}}, {"field": "/result/duplicate", "output": {"error": "field read failed"}}]}});
            let mut document = Document::default();
            let view = OutputView::historical(output);
            document.output_with_error(
                "custom_tool",
                &Value::Null,
                Some(&view),
                Some("parent failure"),
            );
            let text = document.plain_text();
            for marker in [
                "Output incomplete.",
                "parent failure",
                "field read failed",
                "live payload",
            ] {
                assert_eq!(text.matches(marker).count(), 1, "{text}");
            }
            for absent in [
                "Complete result",
                "End of available output",
                "\"output\"",
                "\"preview\"",
            ] {
                assert!(!text.contains(absent), "{text}");
            }
            assert!(text.contains("More saved output available"));
            assert!(document.sections.iter().any(|section| {
                matches!(section, Section::Line(runs) if runs.len() == 1
                    && runs[0].text == "/result/missing" && runs[0].role == Role::Label)
            }));
            assert!(has_source(&document, "  live payload\t"));
        }
    }

    #[test]
    fn complete_whole_output_preview_owns_identical_error_summaries_without_losing_source() {
        for error in [
            json!("failed exactly"),
            json!({"message": "failed exactly", "meta": {"code": 7}}),
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
                let output = json!({"error": error, "result": {"error": error}, "presentation": {"preview": whole_output_preview(&saved)}});
                let before = output.clone();
                let document = with_error(&output, error.as_str());
                assert!(error_sources(&document).is_empty(), "{pointer}: {error}");
                // Repeated fields in the preview itself are source data, not summaries.
                let repeated = if pointer == "both" { 2 } else { 1 };
                assert_eq!(
                    document.plain_text().matches("failed exactly").count(),
                    repeated
                );
                assert!(has_source(&document, &displayed(&saved)));
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
            let output = json!({"error": "failed exactly", "presentation": {"preview": preview}});
            let document = with_error(&output, Some("failed exactly"));
            assert_eq!(error_sources(&document), ["failed exactly"], "{name}");
            // The preview remains visible even when it also contains the error.
            let count = document.plain_text().matches("failed exactly").count();
            assert_eq!(count, 2, "{name}");
        }
        for source in [
            "{\"error\": \"failed exactly\"",      // Malformed JSON.
            "failed exactly",                      // Prose, not a structured error field.
            "[ {\"error\": \"failed exactly\"} ]", // Not a saved document.
            "\"failed exactly\"",
        ] {
            let output = json!({"error": "failed exactly", "presentation": {"preview": {
                "next_offset": 0, "field": "", "total_lines": 1, "lines": [source]
            }}});
            let document = rendered("exec", &Value::Null, &output);
            assert_eq!(error_sources(&document), ["failed exactly"], "{source}");
            assert_eq!(document.plain_text().matches("failed exactly").count(), 2);
        }
        // Without an envelope summary only a complete preview owns the error.
        let mut output = json!({"presentation": {"preview": complete}});
        let document = with_error(&output, Some("failed exactly"));
        assert!(error_sources(&document).is_empty());
        assert_eq!(document.plain_text().matches("failed exactly").count(), 1);
        output["presentation"]["preview"]["next_start"] = json!(2);
        let document = with_error(&output, Some("failed exactly"));
        assert_eq!(error_sources(&document), ["failed exactly"]);
        let mut document = Document::default();
        document.output_with_error("exec", &Value::Null, None, Some("failed exactly"));
        assert_eq!(error_sources(&document), ["failed exactly"]);
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
            let output = json!({"error": "failed exactly", "presentation": {"preview": whole_output_preview(&saved)}});
            let document = rendered("exec", &Value::Null, &output);
            assert_eq!(error_sources(&document), ["failed exactly"], "{saved}");
            let source = displayed(&saved);
            assert!(has_code(&document, |shown, _, role| shown == source
                && role == Role::Plain));
        }
        let output = json!({"error": "outer error", "result": {"error": "inner error"}, "presentation": {"preview": whole_output_preview(&json!({"error": "outer error", "result": null}))}});
        let document = with_error(&output, Some("third error"));
        assert_eq!(error_sources(&document), ["third error", "inner error"]);
        assert_eq!(document.plain_text().matches("outer error").count(), 1);
        // Deduplication requires exact structured values, not similar messages.
        let first = json!({"message": "read failed", "meta": {"code": 1}});
        let second = json!({"message": "read failed", "meta": {"code": 2}});
        let output = json!({"error": first, "result": {"error": first, "stdout": "read failed with extra source text"}, "presentation": {"captures": [{"field": "/result/stdout", "output": {"error": second}}]}});
        let text = with_error(&output, Some("read failed")).plain_text();
        assert_eq!(text.matches("\"code\": 1").count(), 1);
        assert_eq!(text.matches("\"code\": 2").count(), 1);
        assert!(text.contains("read failed with extra source text"));
    }
}
