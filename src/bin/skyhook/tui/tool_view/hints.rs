//! Presentation hints for builtin tools; every other tool renders generically.
use super::Role;
use serde_json::Value;
use skyhook::job::FieldPointer;
use skyhook::tool::builtins::names::{AGENT, EXEC, READ, REPLACE, SCRIPT, WRITE};
use {Hint::*, Syntax::*};

#[derive(Clone, Copy)]
pub(super) enum Syntax {
    /// Shown as written, without highlighting.
    Verbatim,
    /// Formatted when it holds a JSON container, otherwise plain.
    Detect,
    /// A JSON document: formatted when it parses, highlighted as JSON either way.
    Json,
    Named(&'static str),
    /// The file type of the call's `path` argument.
    Path,
}

impl Syntax {
    pub(super) fn detects_json(self) -> bool {
        matches!(self, Detect | Json)
    }
}

enum Hint {
    /// An argument whose value summarizes the job in its header.
    Summary(&'static str),
    /// A fixed header summary.
    Label(&'static str),
    /// An argument holding literal source, never prose.
    Source(&'static str, Syntax, Role),
    /// A result text field shown as its own labelled block.
    Text(&'static str, &'static str, Syntax),
    /// The call starts a child agent on its `target` argument's target.
    ChildTarget,
}

/// Rows for a specific tool precede the rows for every tool, so they win.
const HINTS: &[(Option<&str>, Hint)] = &[
    (Some(AGENT), ChildTarget),
    (Some(SCRIPT), Label("JavaScript workflow")),
    (Some(SCRIPT), Source("source", Named("js"), Role::Plain)),
    (Some(EXEC), Source("command", Named("sh"), Role::Plain)),
    (Some(WRITE), Source("content", Path, Role::Plain)),
    (Some(REPLACE), Source("old", Path, Role::Removed)),
    (Some(REPLACE), Source("new", Path, Role::Added)),
    (Some(READ), Text("content", "File content", Path)),
    (None, Summary("command")),
    (None, Summary("path")),
    (None, Summary("pattern")),
    (None, Source("command", Verbatim, Role::Plain)),
    (None, Source("commands", Verbatim, Role::Plain)),
    (None, Source("source", Verbatim, Role::Plain)),
    (None, Source("script", Verbatim, Role::Plain)),
    (None, Source("code", Verbatim, Role::Plain)),
    (None, Source("content", Verbatim, Role::Plain)),
    (None, Source("old", Verbatim, Role::Removed)),
    (None, Source("new", Verbatim, Role::Added)),
    (None, Text("content", "File content", Verbatim)),
    (None, Text("stdout", "stdout", Detect)),
    (None, Text("stderr", "stderr", Detect)),
    (None, Text("console", "Console", Detect)),
];

fn rows(tool: &str) -> impl Iterator<Item = &'static Hint> {
    let rows = HINTS
        .iter()
        .filter(move |(only, _)| only.is_none_or(|only| only == tool));
    rows.map(|(_, hint)| hint)
}

/// Whether a call of `tool`, even before admission, starts a child agent.
pub fn starts_child(tool: &str) -> bool {
    rows(tool).any(|hint| matches!(hint, Hint::ChildTarget))
}

/// A call's tool and arguments, which select its hints.
#[derive(Clone, Copy)]
pub struct Hints<'a> {
    tool: &'a str,
    pub(super) args: &'a Value,
}

impl<'a> Hints<'a> {
    pub fn new(tool: &'a str, args: &'a Value) -> Self {
        Self { tool, args }
    }

    pub fn summary(self) -> String {
        let summary = rows(self.tool).find_map(|hint| match hint {
            Hint::Label(label) => Some((*label).into()),
            Hint::Summary(argument) => match self.args.get(argument)? {
                Value::String(text) => Some(text.clone()),
                Value::Array(words) => {
                    let words = words.iter().filter_map(Value::as_str);
                    Some(words.collect::<Vec<_>>().join(" "))
                }
                _ => None,
            },
            _ => None,
        });
        summary.unwrap_or_default()
    }

    /// The language and change role of a literal source argument.
    pub(super) fn source(self, argument: &str) -> Option<(String, Role)> {
        rows(self.tool).find_map(|hint| match hint {
            Hint::Source(name, syntax, role) if *name == argument => {
                Some((self.language(*syntax), *role))
            }
            _ => None,
        })
    }

    /// Result text fields as `(field, label, syntax)`.
    pub(super) fn texts(self) -> impl Iterator<Item = (&'static str, &'static str, Syntax)> {
        rows(self.tool).filter_map(|hint| match hint {
            Hint::Text(field, label, syntax) => Some((*field, *label, *syntax)),
            _ => None,
        })
    }

    /// A saved output field's syntax; the whole document and its result are JSON.
    pub(super) fn field_syntax(self, field: &FieldPointer) -> Syntax {
        let result = FieldPointer::result();
        let mut texts = self.texts();
        match texts.find(|(name, ..)| result.property(name) == *field) {
            Some((.., syntax)) => syntax,
            None if field.is_root() || *field == result => Json,
            None => Detect,
        }
    }

    /// The highlighting language, empty for plain text.
    pub(super) fn language(self, syntax: Syntax) -> String {
        match syntax {
            Verbatim | Detect => String::new(),
            Json => "json".into(),
            Named(language) => language.into(),
            Path => {
                let path = self.args["path"].as_str().unwrap_or_default();
                let name = std::path::Path::new(path).file_name();
                match name.and_then(|name| name.to_str()).unwrap_or_default() {
                    name @ ("Dockerfile" | "Makefile") => name.into(),
                    name => name
                        .rsplit_once('.')
                        .map_or("", |(_, extension)| extension)
                        .to_ascii_lowercase(),
                }
            }
        }
    }
}
