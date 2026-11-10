//! Reading, writing, editing and removing a target's files.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Extent, Input, TargetPath};

/// Read a resolved path, naming it relative to `workspace`; a file's bytes from
/// `from` on.
#[derive(Debug, Deserialize, Serialize)]
pub struct Read {
    pub path: TargetPath,
    pub workspace: TargetPath,
    pub mode: ReadMode,
    pub from: ReadFrom,
}

/// Where in a file a read starts, found on its target so nothing before it is sent.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum ReadFrom {
    #[default]
    Start,
    /// The start of this one-based line, or the file's end when it has fewer.
    Line(u64),
}

#[derive(Debug, Deserialize, Serialize)]
pub enum ReadMode {
    /// A directory as its listing, or a file's bytes whole. A file longer than
    /// `counts` bytes is counted where it is before any of it is sent.
    Entry { counts: u64 },
    /// A regular file's bytes.
    File,
}

/// What a read sends as it runs, in order.
#[derive(Debug, Deserialize, Serialize)]
pub enum ReadEvent {
    /// The file opened, `length` bytes long; its bytes from `offset`, where the read
    /// starts, follow. `counted` is all that a file past its read's limit holds: none
    /// unless all of it is text, UTF-8 with no NUL byte.
    Opened {
        length: u64,
        offset: u64,
        counted: Option<Extent>,
    },
    /// A file's next bytes.
    Bytes(#[serde(with = "serde_bytes")] Vec<u8>),
    /// A directory's next entries.
    Entries(Vec<Entry>),
}

#[derive(Debug, Deserialize, Serialize)]
pub enum ReadEnd {
    /// The directory's entries were all sent.
    Directory,
    /// The file's bytes were all sent; `shown` names it relative to the workspace.
    File { shown: String },
}

/// A directory entry, classified by its own metadata: a link is never followed.
#[derive(Debug, Deserialize, Serialize)]
pub struct Entry {
    /// For display: a name that is not text is shown lossily.
    pub name: String,
    pub kind: EntryKind,
}

#[derive(Debug, Deserialize, Serialize)]
pub enum EntryKind {
    File { bytes: u64 },
    Directory,
    Symlink,
    Other,
}

/// Create or replace a file atomically, creating its missing parents.
#[derive(Debug, Deserialize, Serialize)]
pub struct Write {
    pub path: TargetPath,
    pub content: Content,
}

#[derive(Debug, Deserialize, Serialize)]
pub enum Content {
    Inline(#[serde(with = "serde_bytes")] Vec<u8>),
    /// Streamed by the host as the write runs.
    Streamed(#[serde(with = "super::input")] Input),
    /// Another regular file on the same target.
    Copy(TargetPath),
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Written {
    pub bytes: u64,
}

/// Whether a call created directories to hold what it makes.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum Directories {
    #[default]
    Existed,
    Created,
}

/// The most match lines a mismatch shows; one more is listed when there are more.
pub const SHOWN_LINES: usize = 10;

// Replace exact text in a UTF-8 file, matching a uniformly CRLF file's line endings,
// and commit the result atomically. The replace tool's input: a doc comment would
// describe its schema.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Replace {
    /// UTF-8 text file to edit
    pub path: TargetPath,
    /// Exact text to find
    #[schemars(length(min = 1))]
    pub old: String,
    /// Replacement text
    pub new: String,
    /// Required number of matches. The edit fails if the actual count differs
    #[serde(default = "one")]
    pub count: usize,
}

const fn one() -> usize {
    1
}

// Remove a file, link or directory, a tree only when `recursive`. The remove tool's
// input: a doc comment would describe its schema.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Remove {
    /// File or directory to remove
    pub path: TargetPath,
    /// Required for non-empty directory trees
    #[serde(default)]
    pub recursive: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemoveKind {
    File,
    Directory,
    Symlink,
}

/// Create a directory and its missing parents.
#[derive(Debug, Deserialize, Serialize)]
pub struct CreateDirs {
    pub path: TargetPath,
}
