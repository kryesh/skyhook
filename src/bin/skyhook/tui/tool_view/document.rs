//! Build semantic documents from tool arguments and result envelopes.
use super::{CodeSource, Document, Role, Run, Section, model};
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
    pub(super) fn code(
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
        let summary = error.map(|error| Value::String(error.into()));
        let shown_summary = summary.as_ref().filter(|summary| {
            !output.is_some_and(|output| contains_error(output, summary.as_str().unwrap()))
                && !whole_preview
                    .as_ref()
                    .is_some_and(|preview| document_has_error(preview, summary))
        });
        if let Some(error) = shown_summary {
            self.error(error);
        }
        if let Some(output) = output {
            self.output_body(tool, args, output, whole_preview.as_ref(), shown_summary);
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
            let mut error = error.clone();
            omit_null_fields(&mut error);
            self.code(&model::pretty(&error), "json", 2, vec![], Role::Error);
        }
    }
    fn output_body(
        &mut self,
        tool: &str,
        args: &Value,
        output: &Value,
        whole_preview: Option<&Value>,
        shown_summary: Option<&Value>,
    ) {
        // Envelope notices describe the capture, not the selected payload.
        // In particular, reaching the final preview page does not imply that
        // the original output was captured completely.
        let notice = output
            .get("notice")
            .and_then(Value::as_str)
            .filter(|notice| !notice.trim().is_empty());
        if let Some(notice) = notice {
            self.line("Notice", Role::Label);
            self.code(notice, "", 2, vec![], Role::Muted);
        }
        let mut shown_errors: Vec<_> = shown_summary.into_iter().collect();
        for pointer in ["/error", "/result/error"] {
            if let Some(error) = output.pointer(pointer).filter(|value| !value.is_null())
                && !shown_errors.contains(&error)
                && !whole_preview.is_some_and(|preview| document_has_error(preview, error))
            {
                self.error(error);
                shown_errors.push(error);
            }
        }
        let captures = output.get("captures").and_then(Value::as_array);
        let has_capture_previews = captures.is_some_and(|captures| {
            captures.iter().any(|capture| {
                capture
                    .pointer("/output/preview")
                    .is_some_and(|value| !value.is_null())
            })
        });
        if let Some(preview) = output.get("preview").filter(|value| !value.is_null()) {
            // Live output may have no saved whole document yet. The selected
            // capture views are more useful than an empty Complete result pane.
            let empty_whole_preview = preview["field"].as_str() == Some("")
                && preview["lines"].as_array().is_some_and(Vec::is_empty);
            if !has_capture_previews || !empty_whole_preview {
                self.output_preview(tool, args, preview);
            }
        } else {
            // Split literal text fields out of the display copy; the original Value is untouched.
            let mut metadata = output.clone();
            omit_null_fields(&mut metadata);
            // `output` is a TUI-only selected-field view, not capture metadata.
            if let Some(captures) = metadata.get_mut("captures").and_then(Value::as_array_mut) {
                for capture in captures {
                    if let Some(capture) = capture.as_object_mut() {
                        capture.remove("output");
                    }
                }
            }
            if notice.is_some()
                && let Some(object) = metadata.as_object_mut()
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
        if let Some(captures) = captures {
            for capture in captures {
                let mut labeled_error = false;
                for pointer in ["/output/error", "/output/result/error"] {
                    if let Some(error) = capture.pointer(pointer).filter(|value| !value.is_null())
                        && !shown_errors.contains(&error)
                        && !whole_preview.is_some_and(|preview| document_has_error(preview, error))
                    {
                        if !labeled_error {
                            self.line(capture["field"].as_str().unwrap_or("Capture"), Role::Label);
                            labeled_error = true;
                        }
                        self.error(error);
                        shown_errors.push(error);
                    }
                }
                if let Some(preview) = capture
                    .pointer("/output/preview")
                    .filter(|value| !value.is_null())
                {
                    // The parent owns capture notices; only independent read
                    // failures above need additional envelope rendering.
                    self.output_preview(tool, args, preview);
                }
            }
        }
    }
    fn output_preview(&mut self, tool: &str, args: &Value, preview: &Value) {
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
        let incomplete = !preview["next_start"].is_null() || !preview["next_offset"].is_null();
        let formatted = (field != "/result/content"
            && (language.is_empty() || language == "json")
            && lines.iter().all(Value::is_string))
        // Only the whole saved document is known to be structured JSON.
        // Selected fields can be literal text containing JSON, so retain
        // their null fields just as we do for stdout and file content.
        .then(|| pretty_json_preview(&source, incomplete, field.is_empty()))
        .flatten();
        let source = if let Some(formatted) = formatted {
            language = "json".into();
            formatted
        } else {
            source
        };
        self.code(&source, &language, 2, vec![], Role::Plain);
        self.line(
            if incomplete {
                "More saved output available"
            } else {
                "End of available output"
            },
            Role::Muted,
        );
    }

    fn result_text(&mut self, text: &str, language: &str) {
        if let Some(value) = json_container(text) {
            self.code(&model::pretty(&value), "json", 2, vec![], Role::Plain);
        } else {
            self.code(text, language, 2, vec![], Role::Plain);
        }
    }
}
/// Saved-output pages can stop inside a JSON container (or even a string).
/// Format valid prefixes too, without completing them or changing saved source
/// offsets. Non-JSON text and continuation pages that start mid-token stay raw.
fn pretty_json_preview(text: &str, incomplete: bool, structured: bool) -> Option<String> {
    match serde_json::from_str::<Value>(text) {
        Ok(mut value @ (Value::Object(_) | Value::Array(_))) => {
            if structured {
                omit_null_fields(&mut value);
            }
            Some(model::pretty(&value))
        }
        Err(error) if incomplete && error.is_eof() && text.trim_start().starts_with(['{', '[']) => {
            Some(pretty_json_prefix(text))
        }
        _ => None,
    }
}

/// The parser has already verified that this is a container prefix. Preserve
/// every token, including escapes and unfinished strings; only whitespace
/// outside strings is replaced. Indentation is emitted lazily so a page ending
/// just after an opening delimiter does not acquire fabricated content.
fn pretty_json_prefix(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut depth = 0usize;
    let mut string = false;
    let mut escaped = false;
    let mut newline = false;
    let mut previous = None;
    for ch in text.chars() {
        if string {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                string = false;
            }
            continue;
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
            '"' => string = true,
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

#[cfg(test)]
mod tests {
    use super::super::{ContentTheme, Wrap};
    use super::*;
    use serde_json::json;

    #[test]
    fn argument_wrapping_is_semantic_and_retains_nested_context() {
        let mut document = Document::default();
        document.arguments(
            "exec",
            &json!({
                "prompt": "Keep **literal** Markdown.\n\n  Next paragraph  ",
                "nested": {"text": "Nested prose", "items": ["array prose"]},
                "argv": ["printf", "  %s\t%s\n"],
                "commands": [{"value": "  raw command  "}],
                "raw": {"source": "  let value = 42;\n"},
                "content": "  file contents  \n\n",
                "patch": "@@ -1 +1 @@\n-old\n+new\n"
            }),
        );
        let lines = document.layout_lines(None);
        for (markers, expected) in [
            (
                &[
                    "**literal**",
                    "Next paragraph",
                    "Nested prose",
                    "array prose",
                ][..],
                Wrap::Words,
            ),
            (
                &[
                    "printf",
                    "%s",
                    "raw command",
                    "let value",
                    "file contents",
                    "@@",
                ][..],
                Wrap::Hard,
            ),
        ] {
            for marker in markers {
                let (_, wrapping) = lines
                    .iter()
                    .find(|(line, _)| line.to_string().contains(marker))
                    .unwrap();
                assert_eq!(*wrapping, expected, "{marker}");
            }
        }
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
        let lines = document.lines(None);
        let error = lines
            .iter()
            .find(|line| line.to_string().contains("Permission was denied"))
            .unwrap();
        assert_eq!(
            error.spans.last().unwrap().style.fg,
            Some(ContentTheme::new().error)
        );
        let mut structured = Document::default();
        structured.output(
            "exec",
            &Value::Null,
            &json!({"error": {"message": "validation failed", "details": ["argv is required"]}}),
        );
        assert!(structured.plain_text().contains("validation failed"));
        assert!(structured.plain_text().contains("argv is required"));
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
                let mut output = json!({"preview": {
                    "field": field, "lines": source.split('\n').collect::<Vec<_>>(),
                    "total_lines": 1000, "next_start": 10
                }});
                if byte_offset {
                    output["preview"]["next_offset"] = json!(200);
                }
                let original = output.clone();
                let mut document = Document::default();
                document.output(tool, &Value::Null, &output);
                assert!(
                    document.sections.iter().any(|section| {
                        matches!(section, Section::Code { source, language, .. }
                        if &**source == expected && language == "json")
                    }),
                    "{tool}: {}",
                    document.plain_text()
                );
                assert!(
                    document
                        .plain_text()
                        .contains("More saved output available")
                );
                assert_eq!(output, original);
            }
        }
    }

    #[test]
    fn json_prefix_formatting_preserves_tokens_at_every_character_boundary() {
        let value = json!({"items": [null, true, false, -12.5e20, {}, [], {
            "text": "  spaces\t\n\"escaped\" \\ braces {},[] and unicode é雪"
        }]});
        let source = serde_json::to_string(&value).unwrap();
        for (end, _) in source.char_indices().skip(1) {
            let prefix = &source[..end];
            let formatted = pretty_json_preview(prefix, true, true).unwrap();
            let restored = formatted + &source[end..];
            assert_eq!(
                serde_json::from_str::<Value>(&restored).unwrap(),
                value,
                "{end}"
            );
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
            let mut output = json!({"preview": {
                "field": field, "lines": source.split('\n').collect::<Vec<_>>()
            }});
            if next {
                output["preview"]["next_start"] = json!(2);
            }
            let mut document = Document::default();
            document.output(tool, &json!({"path": "data.json"}), &output);
            assert!(
                document.sections.iter().any(|section| {
                    matches!(section, Section::Code { source: shown, .. } if &**shown == source)
                }),
                "{tool}: {source}"
            );
        }
    }

    fn whole_output_preview(saved: &Value) -> Value {
        let source = serde_json::to_string_pretty(saved).unwrap();
        let lines: Vec<_> = source.lines().collect();
        json!({"field": "", "total_lines": lines.len(), "lines": lines})
    }

    #[test]
    fn output_notices_render_once_for_structured_and_paged_payloads() {
        for preview in [
            Value::Null,
            json!({"field": "/result/stdout", "lines": ["payload"]}),
            json!({"field": "/result/stdout", "lines": ["payload"], "next_start": 2}),
            json!({"field": "/result/stdout", "lines": ["payload"], "next_offset": 200}),
        ] {
            let output = json!({"notice": "Output incomplete.", "preview": preview,
                                "result": {"stdout": "payload"}});
            let mut document = Document::default();
            document.output("exec", &Value::Null, &output);
            let text = document.plain_text();
            assert_eq!(text.matches("Output incomplete.").count(), 1);
            assert_eq!(text.matches("payload").count(), 1);
            if !preview.is_null() {
                let continuing =
                    !preview["next_start"].is_null() || !preview["next_offset"].is_null();
                assert!(text.contains(if continuing {
                    "More saved output available"
                } else {
                    "End of available output"
                }));
            }
        }
    }

    #[test]
    fn live_captures_keep_unique_read_errors_without_duplicate_envelopes_or_empty_panes() {
        for preview in [
            Value::Null,
            json!({"field": "", "lines": [], "total_lines": 0}),
        ] {
            let output = json!({
                "state": "running", "result": null, "preview": preview,
                "notice": "Output incomplete.", "error": "parent failure",
                "captures": [
                    {"field": "/result/custom", "kind": "text", "complete": false, "output": {
                        "notice": "Output incomplete.", "error": "parent failure",
                        "preview": {"field": "/result/custom", "lines": ["  live payload\t"], "next_start": 2}
                    }},
                    {"field": "/result/missing", "output": {"error": "field read failed"}},
                    {"field": "/result/duplicate", "output": {"error": "field read failed"}}
                ]
            });
            let mut document = Document::default();
            document.output_with_error(
                "custom_tool",
                &Value::Null,
                Some(&output),
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
            assert!(document.sections.iter().any(|section| {
                matches!(section, Section::Code { source, .. } if &**source == "  live payload\t")
            }));
        }
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
                let mut display = saved.clone();
                omit_null_fields(&mut display);
                let source = serde_json::to_string_pretty(&display).unwrap();
                assert!(document.sections.iter().any(|section| {
                    matches!(section, Section::Code { source: shown, .. } if **shown == source)
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
            let mut display = saved.clone();
            omit_null_fields(&mut display);
            let source = serde_json::to_string_pretty(&display).unwrap();
            assert!(document.sections.iter().any(|section| {
                matches!(section, Section::Code { source: shown, role: Role::Plain, .. } if **shown == source)
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
}
