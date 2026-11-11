//! A configuration document as its files wrote it: every value with the file, line and
//! column it was written at, and never the text around it.

use std::{fmt, path::Path, sync::Arc};

/// Where a value is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// The file it was read from, absent for a document a host held as text.
    pub file: Option<Arc<Path>>,
    pub line: u64,
    pub column: u64,
}

impl Source {
    /// The start of a document read from no file.
    pub const START: Self = Self {
        file: None,
        line: 1,
        column: 1,
    };

    /// The directory a relative path written here resolves against.
    pub fn directory(&self) -> Option<&Path> {
        self.file.as_deref().and_then(Path::parent)
    }
}

/// `file:line:column`, or `line:column` without a file.
impl fmt::Display for Source {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(file) = &self.file {
            write!(formatter, "{}:", file.display())?;
        }
        write!(formatter, "{}:{}", self.line, self.column)
    }
}

/// A written value and where it is written.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub value: Value,
    pub at: Source,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(Number),
    Str(String),
    Seq(Vec<Node>),
    Map(Map),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Number {
    Unsigned(u64),
    Signed(i64),
    Float(f64),
}

/// A mapping's entries in the order they were written.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Map(Vec<Entry>);

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub key: String,
    /// Where the key is written.
    pub at: Source,
    pub value: Node,
}

impl Map {
    pub fn get(&self, key: &str) -> Option<&Node> {
        self.entry(key).map(|entry| &entry.value)
    }

    pub fn entry(&self, key: &str) -> Option<&Entry> {
        self.0.iter().find(|entry| entry.key == key)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.entry(key).is_some()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Entry> {
        self.0.iter()
    }

    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, Entry> {
        self.0.iter_mut()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Add `entry`, or replace the entry of its key where that one was written.
    pub fn insert(&mut self, entry: Entry) {
        match self.0.iter_mut().find(|held| held.key == entry.key) {
            Some(held) => *held = entry,
            None => self.0.push(entry),
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Node> {
        let entry = self.0.iter_mut().find(|entry| entry.key == key);
        entry.map(|entry| &mut entry.value)
    }
}

impl IntoIterator for Map {
    type Item = Entry;
    type IntoIter = std::vec::IntoIter<Entry>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl Node {
    pub fn as_map(&self) -> Option<&Map> {
        match &self.value {
            Value::Map(map) => Some(map),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match &self.value {
            Value::Str(text) => Some(text),
            _ => None,
        }
    }

    /// What the value is, as a refusal names it: never the value itself, which may be
    /// a credential.
    pub fn kind(&self) -> &'static str {
        match self.value {
            Value::Null => "null",
            Value::Bool(_) => "a boolean",
            Value::Number(_) => "a number",
            Value::Str(_) => "a string",
            Value::Seq(_) => "a list",
            Value::Map(_) => "a mapping",
        }
    }

    /// The value `path` leads to, or else the nearest one enclosing it.
    pub fn locate<'a>(&self, path: impl IntoIterator<Item = &'a super::Segment>) -> &Node {
        let mut node = self;
        for segment in path {
            let next = match (segment, &node.value) {
                (super::Segment::Key(key), Value::Map(map)) => map.get(key),
                (super::Segment::Index(index), Value::Seq(items)) => items.get(*index),
                _ => None,
            };
            match next {
                Some(next) => node = next,
                None => break,
            }
        }
        node
    }
}
