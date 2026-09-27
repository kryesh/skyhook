//! Build semantic documents from tool arguments and job views.
use super::hints::{Hints, Syntax};
use super::{CodeSource, Document, Role, Run, Section};
use crate::tui::format::{Clean, clean, pretty};
use serde::Deserialize;
use serde_json::Value;
use skyhook::agent::Question;
use skyhook::job::{
    FieldPointer, JobView, OutputPreview, OutputTruncation, Presentation, diagnostic_slot,
    omit_null_fields,
};
use unicode_width::UnicodeWidthStr;

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
        gutters: Option<Clean>,
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
    pub fn arguments(&mut self, hints: Hints<'_>) {
        self.line("Arguments", Role::Heading);
        if hints.args.as_object().is_some_and(|v| v.is_empty()) {
            self.line("  No arguments", Role::Muted);
        } else {
            self.fields(hints.args, 2, hints, false);
        }
    }
    /// A literal ancestor keeps its descendants literal; lack of syntax never
    /// turns literal data into prose.
    fn fields(&mut self, value: &Value, indent: usize, hints: Hints<'_>, literal: bool) {
        let entries: Vec<(String, &Value)> = match value {
            Value::Object(values) => values
                .iter()
                // Clean before measuring, so the label column aligns.
                .map(|(key, value)| (clean(key).replace('\n', " "), value))
                .collect(),
            Value::Array(values) => values
                .iter()
                .enumerate()
                .map(|(index, value)| (format!("{}.", index + 1), value))
                .collect(),
            _ => {
                self.argument_block(value, indent, literal);
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
            let source = hints.source(&name);
            let literal = literal || source.is_some();
            if let Some((language, role)) = source
                && let Some(text) = value.as_str()
            {
                let (label, gutter) = match role {
                    Role::Removed => (format!("{name} (removed)"), Some("− ")),
                    Role::Added => (format!("{name} (added)"), Some("+ ")),
                    _ => (name, None),
                };
                self.line(format!("{prefix}{label}"), Role::Label);
                self.code(text, &language, indent + 2, gutter.map(Clean::from), role);
            } else if let Some((text, role)) = scalar(value) {
                if text.contains('\n') {
                    self.line(format!("{prefix}{name}"), Role::Label);
                    self.argument_block(value, indent + 2, literal);
                } else {
                    let runs = vec![
                        Run::new(format!("{prefix}{name}"), Role::Label),
                        Run::new(
                            " ".repeat(width.saturating_sub(name.width()) + 2),
                            Role::Plain,
                        ),
                        Run::new(text, role),
                    ];
                    self.sections.push(if value.is_string() && !literal {
                        Section::Prose(runs)
                    } else {
                        Section::Line(runs)
                    });
                }
            } else {
                self.line(format!("{prefix}{name}"), Role::Label);
                self.fields(value, indent + 2, hints, literal);
            }
        }
    }
    /// Prose wraps at words, one logical line per source line.
    fn prose(&mut self, text: &str, indent: usize, role: Role) {
        for line in text.split('\n') {
            self.sections.push(Section::Prose(vec![
                Run::new(" ".repeat(indent), Role::Plain),
                Run::new(line, role),
            ]));
        }
    }
    fn argument_block(&mut self, value: &Value, indent: usize, literal: bool) {
        let Some((text, role)) = scalar(value) else {
            return;
        };
        if value.is_string() && !literal {
            self.prose(&text, indent, role);
        } else {
            self.code(&text, "", indent, None, role);
        }
    }
    /// Error summaries belong to the expanded Output section, never the header.
    /// Each distinct error renders once, before the output it describes, unless
    /// the whole result below already shows it.
    pub fn output(&mut self, hints: Hints<'_>, view: Option<&JobView>, summaries: &[&str]) {
        self.line("Output", Role::Heading);
        let whole = view.and_then(whole_diagnostic);
        let mut shown: Vec<_> = whole.as_deref().into_iter().collect();
        for error in summaries
            .iter()
            .copied()
            .chain(view.and_then(JobView::error))
        {
            self.error(hints, error, &mut shown);
        }
        if let Some(view) = view {
            self.job_view(hints, view, &mut shown);
        }
    }
    /// A historical tool result is the JobView the model received; anything
    /// else renders as the raw JSON it is. Every view field is optional and
    /// unknown fields are ignored, so only a view that reproduces the value
    /// exactly is one; `{"stdout": …}` would otherwise parse as an empty view.
    pub fn historical_output(&mut self, hints: Hints<'_>, result: &Value) {
        match JobView::deserialize(result) {
            Ok(view) if serde_json::to_value(&view).is_ok_and(|value| value == *result) => {
                self.output(hints, Some(&view), &[]);
            }
            _ => {
                self.line("Output", Role::Heading);
                self.code(&pretty(result), "json", 2, None, Role::Plain);
            }
        }
    }
    fn error<'a>(&mut self, hints: Hints<'_>, error: &'a str, shown: &mut Vec<&'a str>) {
        if !shown.contains(&error) {
            self.text(error, hints, Syntax::Detect, Role::Error);
            shown.push(error);
        }
    }
    fn job_view<'a>(&mut self, hints: Hints<'_>, view: &'a JobView, shown: &mut Vec<&'a str>) {
        let presentation = view.presentation();
        // Envelope notices describe the capture, not the selected payload.
        // Reaching the final preview page does not imply that the original
        // output was captured completely.
        if let Some(notice) = presentation.and_then(Presentation::notice) {
            self.line("Notice", Role::Label);
            self.code(notice.as_str(), "", 2, None, Role::Muted);
        }
        self.questions(presentation.map_or(&[], Presentation::questions));
        let captures = presentation.map_or(&[][..], Presentation::captures);
        let pages = captures.iter().filter_map(|capture| {
            let page = capture.output()?;
            Some((
                capture.field(),
                page,
                page.presentation().and_then(Presentation::preview),
            ))
        });
        match presentation.and_then(Presentation::preview) {
            // Live output may have no saved whole document yet. The capture
            // pages are more useful than an empty complete result pane.
            Some(preview @ OutputPreview::Lines(page))
                if preview.field().is_some_and(FieldPointer::is_root)
                    && page.lines().is_empty()
                    && pages.clone().any(|(.., page)| page.is_some()) => {}
            Some(preview) => self.preview(hints, preview),
            None => {
                if let Some(result) = view.result() {
                    self.result(hints, result);
                }
                if let Some(presentation) = presentation {
                    self.cuts(presentation.truncated(), presentation.shape());
                    if !presentation.truncated().is_empty() {
                        self.line("More saved output available", Role::Muted);
                    }
                }
            }
        }
        for (field, page, preview) in pages {
            if let Some(error) = page.error().filter(|error| !shown.contains(error)) {
                self.line(field.as_str(), Role::Label);
                self.error(hints, error, shown);
            }
            if let Some(preview) = preview {
                self.preview(hints, preview);
            }
        }
        if let Some(envelope) = view.envelope() {
            self.code(&pretty(&envelope), "", 2, None, Role::Muted);
        }
    }
    /// A waiting child's question batch stands in for its result.
    fn questions(&mut self, questions: &[Question]) {
        for question in questions {
            self.line("Question", Role::Label);
            self.prose(&question.prompt, 2, Role::Plain);
            // As in the prompt panel, a description starts on its own line.
            for option in &question.options {
                self.prose(&format!("• {}", option.label), 2, Role::Label);
                if !option.description.is_empty() {
                    self.prose(&option.description, 4, Role::Muted);
                }
            }
        }
    }
    /// Literal text fields are split out of the display copy of the result.
    fn result(&mut self, hints: Hints<'_>, result: &Value) {
        if let Some(text) = result.as_str() {
            return self.text(text, hints, Syntax::Detect, Role::Plain);
        }
        let mut rest = result.clone();
        omit_null_fields(&mut rest);
        let mut texts = Vec::new();
        if let Some(object) = rest.as_object_mut() {
            for (field, label, syntax) in hints.texts() {
                if let Some(Value::String(text)) = object.get(field) {
                    texts.push((label, syntax, text.clone()));
                    object.shift_remove(field);
                }
            }
        }
        if !rest.as_object().is_some_and(serde_json::Map::is_empty) {
            self.code(&pretty(&rest), "json", 2, None, Role::Plain);
        }
        for (label, syntax, text) in texts {
            self.line(label, Role::Label);
            self.text(&text, hints, syntax, Role::Plain);
        }
    }
    fn preview(&mut self, hints: Hints<'_>, preview: &OutputPreview) {
        // A model page omits the field it was asked for.
        let field = preview.field();
        let root = field.is_some_and(FieldPointer::is_root);
        let heading = match field {
            Some(_) if root => "Complete result",
            Some(field) => field.as_str(),
            None => "Requested field",
        };
        self.line(heading, Role::Muted);
        let more = preview.continuation().is_some();
        match preview {
            OutputPreview::Lines(page) => {
                let source = page.lines().join("\n");
                let syntax = field.map_or(Syntax::Detect, |field| hints.field_syntax(field));
                let formatted = syntax
                    .detects_json()
                    .then(|| pretty_json_preview(&source, more, root));
                match formatted.flatten() {
                    Some(formatted) => self.code(&formatted, "json", 2, None, Role::Plain),
                    None => self.code(&source, &hints.language(syntax), 2, None, Role::Plain),
                }
            }
            OutputPreview::Elements(page) => {
                let elements = Value::Array(page.elements().to_vec());
                self.code(&pretty(&elements), "json", 2, None, Role::Plain);
            }
            OutputPreview::Members(page) => {
                let mut members = Value::Object(page.members().clone());
                if root {
                    omit_null_fields(&mut members);
                }
                self.code(&pretty(&members), "json", 2, None, Role::Plain);
            }
            OutputPreview::Matches(page) => {
                if page.matches().is_empty() {
                    self.line("No matches", Role::Muted);
                }
                for found in page.matches() {
                    self.line(found.at().as_str(), Role::Label);
                    self.code(&pretty(found.value()), "json", 2, None, Role::Plain);
                }
            }
        }
        let sampled = !preview.truncated().is_empty();
        self.cuts(preview.truncated(), preview.shape());
        self.line(
            match (more, sampled) {
                (true, _) => "More saved output available",
                (false, true) => "End of this field; follow the cuts above for the rest",
                (false, false) => "End of available output",
            },
            Role::Muted,
        );
    }
    /// What a presentation left out, and the shape of the whole value.
    fn cuts(&mut self, truncated: &[OutputTruncation], shape: Option<&Value>) {
        if !truncated.is_empty() {
            self.line("Left out", Role::Muted);
            let cuts = serde_json::to_value(truncated).unwrap_or_default();
            self.code(&pretty(&cuts), "json", 2, None, Role::Muted);
        }
        if let Some(shape) = shape {
            self.line("Shape", Role::Muted);
            self.code(&pretty(shape), "json", 2, None, Role::Muted);
        }
    }
    fn text(&mut self, text: &str, hints: Hints<'_>, syntax: Syntax, role: Role) {
        if syntax.detects_json()
            && let Some(value) = json_container(text)
        {
            self.code(&pretty(&value), "json", 2, None, role);
        } else {
            self.code(text, &hints.language(syntax), 2, None, role);
        }
    }
}
/// The diagnostic a whole result shows in place: the result itself, or a
/// complete preview of the saved `{"result": …}` document.
fn whole_diagnostic(view: &JobView) -> Option<String> {
    let slot = diagnostic_slot();
    // The slot addresses the saved document; a bare result starts below `result`.
    let diagnostic = |result: &Value| {
        let message = (slot.segments().skip(1)).try_fold(result, |value, key| value.get(key))?;
        Some(message.as_str()?.to_owned())
    };
    match view.presentation().and_then(Presentation::preview) {
        None => diagnostic(view.result()?),
        Some(preview @ OutputPreview::Members(page))
            if preview.field().is_some_and(FieldPointer::is_root)
                && preview.continuation().is_none() =>
        {
            diagnostic(page.members().get("result")?)
        }
        Some(_) => None,
    }
}
/// Saved-output pages can stop inside a JSON container (or even a string).
/// Format valid prefixes too, without completing them or changing saved source
/// offsets. Non-JSON text and continuation pages that start mid-token stay raw.
fn pretty_json_preview(text: &str, more: bool, root: bool) -> Option<String> {
    match serde_json::from_str::<Value>(text) {
        Ok(mut value @ (Value::Object(_) | Value::Array(_))) => {
            if root {
                omit_null_fields(&mut value);
            }
            Some(pretty(&value))
        }
        Err(error) if more && error.is_eof() && text.trim_start().starts_with(['{', '[']) => {
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
#[cfg(test)]
mod tests {
    use super::super::{THEME, Wrap, tests::view};
    use super::*;
    use serde_json::json;

    fn with_error(output: Value, error: Option<&str>) -> Document {
        let mut document = Document::default();
        let hints = Hints::new("exec", &Value::Null);
        document.output(hints, Some(&view(output)), &Vec::from_iter(error));
        document
    }

    fn rendered(tool: &str, arguments: &Value, output: Value) -> Document {
        let mut document = Document::default();
        document.output(Hints::new(tool, arguments), Some(&view(output)), &[]);
        document
    }

    fn arguments(tool: &str, arguments: Value) -> Document {
        let mut document = Document::default();
        document.arguments(Hints::new(tool, &arguments));
        document
    }

    fn has_code(document: &Document, test: impl Fn(&str, &str, Role) -> bool) -> bool {
        document.sections.iter().any(|section| {
            matches!(section, Section::Code { source, language, role, .. } if test(source, language, *role))
        })
    }

    /// Sources are stored as painted: cleaned of controls, tabs expanded.
    fn has_source(document: &Document, expected: &str) -> bool {
        let expected = clean(expected);
        has_code(document, |source, _, _| source == expected)
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
                "command": ["printf", "  %s\t%s\n"],
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
                } => Some(&**marker),
                _ => None,
            })
            .collect();
        assert_eq!(markers, ["− ", "+ "]);
    }

    #[test]
    fn errors_render_once_before_output_and_keep_exact_source() {
        let output = json!({"error": "Permission was denied",
            "result": {"stdout": "  exact\toutput\n\n"}});
        let document = with_error(output, Some("Permission was denied"));
        let text = document.plain_text();
        assert!(text.starts_with("Output\n  Permission was denied"));
        assert_eq!(text.matches("Permission was denied").count(), 1);
        assert!(has_source(&document, "  exact\toutput\n\n"));
        let lines = document.lines(None);
        let error = lines
            .iter()
            .find(|line| line.to_string().contains("Permission was denied"));
        let color = error.unwrap().spans.last().unwrap().style.fg;
        assert_eq!(color, Some(THEME.error));
        let error = r#"{"message": "validation failed", "details": ["argv is required"]}"#;
        let text = with_error(json!({"error": error}), None).plain_text();
        assert!(text.contains("\"validation failed\"") && text.contains("\"argv is required\""));
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
                    "field": field, "lines": source.split('\n').collect::<Vec<_>>(),
                    "total_lines": 1000, "next_start": 10
                }}});
                if byte_offset {
                    output["presentation"]["preview"]["next_offset"] = json!(200);
                }
                let document = rendered(tool, &Value::Null, output);
                let text = document.plain_text();
                let json =
                    |source: &str, language: &str, _| source == expected && language == "json";
                assert!(has_code(&document, json), "{tool}: {text}");
                assert!(text.contains("More saved output available"));
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
                let formatted = pretty_json_preview(prefix, true, true);
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
        // Pages of a JSON document stay highlighted as JSON when they cannot be formatted.
        for (tool, field, source, next, language) in [
            ("read", "/result/content", "{\"items\":[1,2,", true, "toml"),
            (
                "read",
                "/result/content",
                "{\"items\":[1,2]}",
                false,
                "toml",
            ),
            (
                "exec",
                "/result/stdout",
                "  ordinary output\n  [not JSON",
                true,
                "",
            ),
            ("script", "", "\"continuation\": [1, 2,", true, "json"),
            ("script", "/result", "{\"malformed\": nope,", true, "json"),
            ("script", "", "{\"unexpected EOF\": [", false, "json"),
        ] {
            let lines: Vec<_> = source.split('\n').collect();
            let mut output = json!({"presentation": {"preview": {"field": field, "lines": lines}}});
            if next {
                output["presentation"]["preview"]["next_start"] = json!(2);
            }
            let document = rendered(tool, &json!({"path": "data.toml"}), output);
            let source = clean(source);
            let page = |text: &str, highlight: &str, _| text == source && highlight == language;
            assert!(has_code(&document, page), "{tool}: {source}");
        }
    }

    #[test]
    fn a_whole_result_showing_the_error_takes_the_place_of_its_summary() {
        let error = "Permission was denied";
        let result = json!({"error": {"message": error}});
        let page = json!({"field": "", "members": {"result": result}, "total_members": 1});
        for output in [
            json!({"result": result}),
            json!({"presentation": {"preview": page}}),
        ] {
            let text = with_error(output, Some(error)).plain_text();
            assert_eq!(text.matches(error).count(), 1, "{text}");
        }
    }

    /// The document painted 40 columns wide, one trimmed string per row.
    fn rows(document: &Document) -> Vec<String> {
        use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};
        let lines = document.lines(None);
        let area = Rect::new(0, 0, 40, lines.len() as u16);
        let mut buffer = Buffer::empty(area);
        ratatui::widgets::Paragraph::new(lines).render(area, &mut buffer);
        (0..area.height)
            .map(|y| {
                let row: String = (0..area.width).map(|x| buffer[(x, y)].symbol()).collect();
                row.trim_end().to_owned()
            })
            .collect()
    }

    #[test]
    fn a_settled_result_renders_no_envelope_and_a_non_view_renders_raw() {
        for (result, expected) in [
            (json!({}), vec!["Output"]),
            (json!({"result": "done"}), vec!["Output", "  done"]),
            (
                json!({"stdout": "done"}),
                vec!["Output", "  {", r#"    "stdout": "done""#, "  }"],
            ),
            (
                json!({"state": "completed", "stdout": "done"}),
                vec![
                    "Output",
                    "  {",
                    r#"    "state": "completed","#,
                    r#"    "stdout": "done""#,
                    "  }",
                ],
            ),
        ] {
            let mut document = Document::default();
            document.historical_output(Hints::new("replace", &Value::Null), &result);
            assert_eq!(rows(&document), expected);
        }
    }

    #[test]
    fn a_waiting_childs_question_batch_renders_in_place_of_a_result() {
        let question = json!({"id": "pick", "prompt": "Which one?",
            "options": [{"label": "First", "description": "First step\nSecond step"}]});
        let output = json!({"state": "waiting_input",
            "presentation": {"question": {"questions": [question]}}});
        let expected = [
            "Output",
            "Question",
            "  Which one?",
            "  • First",
            "    First step",
            "    Second step",
            "  {",
            "    \"state\": \"waiting_input\"",
            "  }",
        ];
        assert_eq!(rows(&rendered("agent", &Value::Null, output)), expected);
    }

    #[test]
    fn output_notices_render_once_for_structured_and_paged_payloads() {
        for (preview, footer) in [
            (Value::Null, None),
            (
                json!({"field": "/result/stdout", "lines": ["payload"]}),
                Some("End of available output"),
            ),
            (
                json!({"field": "/result/stdout", "lines": ["payload"], "next_start": 2}),
                Some("More saved output available"),
            ),
        ] {
            let output = json!({"result": {"stdout": "payload"},
                "presentation": {"notice": "Output incomplete.", "preview": preview}});
            let text = rendered("exec", &Value::Null, output).plain_text();
            assert_eq!(text.matches("Output incomplete.").count(), 1);
            assert_eq!(text.matches("payload").count(), 1);
            assert!(footer.is_none_or(|footer| text.contains(footer)));
        }
    }

    #[test]
    fn live_captures_keep_unique_read_errors_without_duplicate_envelopes_or_empty_panes() {
        let incomplete = |preview: Value, captures: Value| json!({"notice": "Output incomplete.", "preview": preview, "captures": captures});
        let capture = |field: &str, error: &str, presentation: Value| {
            json!({"field": field, "complete": false, "output": {"state": "completed",
                "error": error, "presentation": presentation}})
        };
        let page =
            json!({"field": "/result/custom", "lines": ["  live payload\t"], "next_start": 2});
        let captures = json!([
            capture(
                "/result/custom",
                "parent failure",
                incomplete(page, json!([]))
            ),
            capture("/result/missing", "field read failed", Value::Null),
            capture("/result/duplicate", "field read failed", Value::Null),
        ]);
        for preview in [
            Value::Null,
            json!({"field": "", "lines": [], "total_lines": 0}),
        ] {
            let presentation = incomplete(preview, captures.clone());
            let output = json!({"state": "running", "error": "parent failure", "presentation": presentation});
            let document = with_error(output, Some("parent failure"));
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
                    && &*runs[0].text == "/result/missing" && runs[0].role == Role::Label)
            }));
            assert!(has_source(&document, "  live payload\t"));
        }
    }
}
