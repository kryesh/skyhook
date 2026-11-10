//! Searching and enumerating a target's files, with ripgrep's matcher, globs
//! and ignore rules, on the target.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::TargetPath;

/// Search the files under a resolved `root`, naming them relative to `workspace`.
#[derive(Debug, Deserialize, Serialize)]
pub struct Search {
    pub root: TargetPath,
    pub workspace: TargetPath,
    pub pattern: String,
    pub fixed: bool,
    pub word: bool,
    pub case: Case,
    pub globs: Vec<String>,
    pub hidden: bool,
    pub no_ignore: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Case {
    #[default]
    Smart,
    Sensitive,
    Insensitive,
}

/// A file's matches, sent once it was read whole, in parts of a bounded size;
/// files finish in any order, and the parts of files searched together interleave.
#[derive(Debug, Deserialize, Serialize)]
pub enum SearchEvent {
    /// More of the file's matches follow.
    Part { path: String, lines: Vec<MatchLine> },
    /// The file's last matches.
    File { path: String, lines: Vec<MatchLine> },
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MatchLine {
    pub number: u64,
    pub excerpt: Excerpt,
}

/// A matched line, cut to a window around its match when it is long.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Excerpt {
    /// The shown bytes as text, invalid sequences replaced.
    pub text: String,
    pub cut_before: bool,
    pub cut_after: bool,
    /// The whole line's length when it was cut.
    pub length: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct Skipped {
    pub path: String,
    pub reason: SkipReason,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// A directory entry could not be listed
    Unlisted,
    /// The file could not be opened
    Unopened,
    /// Reading stopped partway, including at a line beyond the memory guard
    Unread,
}

/// List the files under a resolved directory `root` matching `pattern`, relative
/// to `workspace`, in path order.
#[derive(Debug, Deserialize, Serialize)]
pub struct Glob {
    pub root: TargetPath,
    pub workspace: TargetPath,
    pub pattern: String,
    pub hidden: bool,
    pub no_ignore: bool,
}

/// Bounds what compiling an untrusted pattern may allocate.
pub const REGEX_SIZE_LIMIT: usize = 10 * 1024 * 1024;

/// The line-oriented regex builder the search shares with other matchers of untrusted
/// patterns.
pub fn line_matcher() -> grep_regex::RegexMatcherBuilder {
    let mut builder = grep_regex::RegexMatcherBuilder::new();
    builder
        .line_terminator(Some(b'\n'))
        .ban_byte(Some(b'\0'))
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT);
    builder
}

/// The most of a matched line kept on each side of its match; a longer line is cut
/// to this window and says how long it was.
pub(crate) const MATCH_CONTEXT_BYTES: usize = 256;

/// `line` whole when it is short; otherwise the window of [`MATCH_CONTEXT_BYTES`] on
/// each side of `found`, its match, with how long the line was. Neither outgrows the
/// short-line size or the source bytes it shows, however malformed they are.
pub fn excerpt(line: &[u8], found: Option<std::ops::Range<usize>>) -> Excerpt {
    let context = MATCH_CONTEXT_BYTES;
    let whole = 4 * context;
    if line.len() <= whole {
        return Excerpt {
            text: shown(line, whole),
            cut_before: false,
            cut_after: false,
            length: None,
        };
    }
    let (start, end) = found.map_or((0, 0), |found| (found.start, found.end));
    let start = start.min(line.len());
    let end = end.min(start + 2 * context).min(line.len());
    // Cut between characters: a boundary byte is not a UTF-8 continuation byte, looking at
    // most a character's length away, so malformed input cannot widen the window.
    let continuation = |at: usize| line.get(at).is_some_and(|byte| byte & 0xC0 == 0x80);
    let mut from = start.saturating_sub(context);
    for _ in 0..3 {
        if from > 0 && continuation(from) {
            from -= 1;
        }
    }
    let mut to = (end + context).min(line.len());
    for _ in 0..3 {
        if to < line.len() && continuation(to) {
            to += 1;
        }
    }
    Excerpt {
        text: shown(&line[from..to], whole),
        cut_before: from > 0,
        cut_after: to < line.len(),
        length: Some(line.len()),
    }
}

/// `bytes` as text, invalid sequences replaced. A replacement character is longer than
/// the byte it stands for, so the text is cut to the larger of the source's length and
/// `whole`, the short-line size.
fn shown(bytes: &[u8], whole: usize) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    text.truncate(text.floor_char_boundary(bytes.len().max(whole)));
    text
}
