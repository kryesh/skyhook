//! The markdown written so far and what is owed before the next content.

use std::borrow::Cow;
use std::collections::HashMap;
use std::mem;
use std::ops::Range;

use dom_query::{NodeId, NodeRef};

use super::super::budget::{Budget, Spent};

#[derive(Clone, Copy)]
pub(super) enum HeadingLevel {
    H1,
    H2,
    H3,
    H4,
    H5,
    H6,
}

impl HeadingLevel {
    fn mark(self) -> &'static str {
        match self {
            Self::H1 => "# ",
            Self::H2 => "## ",
            Self::H3 => "### ",
            Self::H4 => "#### ",
            Self::H5 => "##### ",
            Self::H6 => "###### ",
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Emphasis {
    Em,
    Strong,
}

impl Emphasis {
    fn mark(self) -> &'static str {
        match self {
            Self::Em => "*",
            Self::Strong => "**",
        }
    }
}

/// Inline markup, opened only once content follows it.
#[derive(Clone, Copy)]
pub(super) enum Mark<'a> {
    Emphasis(Emphasis),
    Link(NodeRef<'a>),
    Item(Marker),
}

#[derive(Clone, Copy)]
pub(super) enum Marker {
    Bullet,
    Number(usize),
}

impl Marker {
    pub(super) fn width(self) -> usize {
        match self {
            Self::Bullet => 2,
            Self::Number(number) => number.to_string().len() + 2,
        }
    }

    fn text(self) -> Cow<'static, str> {
        match self {
            Self::Bullet => "- ".into(),
            Self::Number(number) => format!("{number}. ").into(),
        }
    }
}

/// What a container writes at the start of each line it holds.
pub(super) enum Prefix {
    Quote,
    /// A list item's marker on its first line, then the indent under it.
    Item {
        marker: Option<Marker>,
        width: usize,
    },
}

impl Prefix {
    /// How the container continues each line after its first.
    fn continue_line(&self, line: &mut String) {
        match self {
            Self::Quote => line.push_str("> "),
            Self::Item { width, .. } => line.extend(std::iter::repeat_n(' ', *width)),
        }
    }
}

/// What separates the next content from what was written, ordered so the wider
/// of two owed separators wins.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Pending {
    Nothing,
    Space,
    /// A line end; a hard one is a line break within its paragraph.
    Line {
        hard: bool,
    },
    Blank,
}

/// The markdown written so far; every byte is charged before it is written.
struct Out<'a> {
    text: String,
    budget: &'a Budget,
}

impl Out<'_> {
    fn push(&mut self, text: &str) -> Result<(), Spent> {
        self.budget.charge(text.len())?;
        self.text.push_str(text);
        Ok(())
    }

    fn repeat(&mut self, ch: char, count: usize) -> Result<(), Spent> {
        self.budget.charge(count * ch.len_utf8())?;
        self.text.extend(std::iter::repeat_n(ch, count));
        Ok(())
    }
}

/// A table cell being written: its element, unless it pads its row, and where
/// its content starts.
struct OpenCell {
    id: Option<NodeId>,
    start: usize,
}

pub(super) struct Writer<'a> {
    out: Out<'a>,
    base: &'a reqwest::Url,
    /// Open containers, innermost last.
    prefixes: Vec<Prefix>,
    /// How the containers a line has started in continue each line after it, so
    /// a line writes them at once whatever their depth.
    continued: String,
    /// How many of the prefixes `continued` holds: an outer run, as a container
    /// opens inside them.
    started: usize,
    pending: Pending,
    /// A heading whose mark is owed at the start of its text.
    heading: Option<HeadingLevel>,
    /// Marks owed before the next inline content, so empty ones vanish.
    owed: Vec<Mark<'a>>,
    /// Marks written and not yet closed, outermost first; all of them are outside
    /// every owed mark.
    open: Vec<Mark<'a>>,
    /// Marks opened inside the word being read, owed once content follows them,
    /// so an empty one leaves the word whole.
    deferred: Vec<Mark<'a>>,
    /// The word being read, written once it ends: what reads as markdown syntax
    /// depends on the whole word, which can span text nodes.
    word: String,
    /// The table cell being written, which keeps its content on one line.
    cell: Option<OpenCell>,
    /// Where each cell written so far holds its content, so a cell spanning
    /// several is written once and copied to the others.
    written: HashMap<NodeId, Range<usize>>,
    /// The text of the code element being read, written whole at its end.
    code: Option<String>,
}

impl<'a> Writer<'a> {
    pub(super) fn new(budget: &'a Budget, base: &'a reqwest::Url) -> Self {
        Self {
            out: Out {
                text: String::new(),
                budget,
            },
            base,
            prefixes: Vec::new(),
            continued: String::new(),
            started: 0,
            pending: Pending::Nothing,
            heading: None,
            owed: Vec::new(),
            open: Vec::new(),
            deferred: Vec::new(),
            word: String::new(),
            cell: None,
            written: HashMap::new(),
            code: None,
        }
    }

    pub(super) fn finish(mut self) -> Result<String, Spent> {
        self.flush()?;
        Ok(self.out.text)
    }

    pub(super) fn in_cell(&self) -> bool {
        self.cell.is_some()
    }

    pub(super) fn push_prefix(&mut self, prefix: Prefix) {
        self.prefixes.push(prefix);
    }

    pub(super) fn pop_prefix(&mut self) {
        self.prefixes.pop();
        if self.prefixes.len() < self.started {
            self.started = self.prefixes.len();
            self.continued.clear();
            for prefix in &self.prefixes {
                prefix.continue_line(&mut self.continued);
            }
        }
    }

    pub(super) fn code(&mut self) -> Option<&mut String> {
        self.code.as_mut()
    }

    fn separate(&mut self, gap: Pending) -> Result<(), Spent> {
        self.flush()?;
        let gap = if self.in_cell() { Pending::Space } else { gap };
        self.pending = self.pending.max(gap);
        Ok(())
    }

    pub(super) fn block(&mut self) -> Result<(), Spent> {
        self.separate(Pending::Blank)
    }

    pub(super) fn line(&mut self) -> Result<(), Spent> {
        self.separate(Pending::Line { hard: false })
    }

    pub(super) fn hard_break(&mut self) -> Result<(), Spent> {
        self.flush()?;
        let hard = self.pending <= Pending::Space && !self.out.text.is_empty();
        self.separate(Pending::Line { hard })
    }

    fn space(&mut self) {
        self.pending = self.pending.max(Pending::Space);
    }

    pub(super) fn heading(&mut self, level: HeadingLevel) {
        if !self.in_cell() {
            self.heading = Some(level);
        }
    }

    pub(super) fn end_heading(&mut self) -> Result<(), Spent> {
        self.heading = None;
        self.block()
    }

    pub(super) fn open(&mut self, mark: Mark<'a>) {
        match self.word.is_empty() {
            true => self.owed.push(mark),
            false => self.deferred.push(mark),
        }
    }

    /// Withdraws the innermost mark if it is still owed, else ends the word in it
    /// and closes it.
    pub(super) fn end_span(&mut self) -> Result<(), Spent> {
        if self.deferred.pop().is_some() {
            return Ok(());
        }
        self.flush()?;
        if self.owed.pop().is_none()
            && let Some(mark) = self.open.pop()
        {
            self.close(mark)?;
        }
        Ok(())
    }

    fn close(&mut self, mark: Mark) -> Result<(), Spent> {
        match mark {
            Mark::Emphasis(emphasis) => self.out.push(emphasis.mark()),
            Mark::Link(node) => {
                self.out.push("](")?;
                self.destination(&node.attr("href").unwrap_or_default())?;
                self.out.push(")")
            }
            Mark::Item(_) => Ok(()),
        }
    }

    /// Closes the written marks where a line or cell ends, which no inline markup
    /// spans, owing them again so they reopen around the content that follows.
    fn suspend(&mut self) -> Result<(), Spent> {
        let open = mem::take(&mut self.open);
        for mark in open.iter().rev() {
            self.close(*mark)?;
        }
        self.owed.splice(0..0, open);
        Ok(())
    }

    /// Starts a cell with `cell`'s content, if any, and whether that content is
    /// to be written: what a cell wrote before is copied, and the cell ends.
    pub(super) fn cell_start(&mut self, cell: Option<NodeRef>) -> Result<bool, Spent> {
        self.flush()?;
        self.pending = Pending::Nothing;
        self.out.push(" ")?;
        let id = cell.map(|cell| cell.id);
        if let Some(content) = id.and_then(|id| self.written.get(&id)).cloned() {
            self.out.budget.charge(content.len())?;
            self.out.text.extend_from_within(content);
            self.out.push(" |")?;
            return Ok(false);
        }
        let start = self.out.text.len();
        self.cell = Some(OpenCell { id, start });
        Ok(true)
    }

    pub(super) fn cell_end(&mut self) -> Result<(), Spent> {
        self.suspend()?;
        self.pending = Pending::Nothing;
        if let Some(OpenCell {
            id: Some(id),
            start,
        }) = self.cell.take()
        {
            self.written.insert(id, start..self.out.text.len());
        }
        self.out.push(" |")
    }

    /// `href` resolved against the base, with the characters that would escape,
    /// end or break a destination or a table cell percent-encoded.
    fn destination(&mut self, href: &str) -> Result<(), Spent> {
        // The join is charged before it is built, as the base can be long; what
        // encoding adds is charged after.
        let joined = self.base.as_str().len() + href.len();
        self.out.budget.charge(joined)?;
        let url = self.base.join(href);
        let url = url.as_ref().map_or(href, reqwest::Url::as_str);
        let mut encoded = String::with_capacity(url.len());
        for (index, ch) in url.char_indices() {
            match ch {
                '(' | ')' | '|' | '\\' | '<' | '>' => {
                    encoded.push_str(&format!("%{:02X}", ch as u32))
                }
                // Markdown reads an entity reference in a destination too.
                '&' if entity(&url[index + 1..]) => encoded.push_str("\\&"),
                ch if ch.is_whitespace() => encoded.push_str("%20"),
                ch => encoded.push(ch),
            }
        }
        self.out
            .budget
            .charge(encoded.len().saturating_sub(joined))?;
        self.out.text.push_str(&encoded);
        Ok(())
    }

    fn starts_line(&self) -> bool {
        (self.out.text.is_empty() || self.pending >= Pending::Line { hard: false })
            && self.heading.is_none()
            && self.owed.is_empty()
    }

    /// Writes the line breaks and prefixes owed before content.
    fn begin_line(&mut self) -> Result<(), Spent> {
        let start = self.out.text.is_empty();
        let breaks = match self.pending {
            Pending::Nothing | Pending::Space => 0,
            Pending::Line { .. } => 1,
            Pending::Blank => 2,
        };
        if !start && breaks > 0 {
            self.suspend()?;
            if self.pending == (Pending::Line { hard: true }) {
                self.out.push("  ")?;
            }
            for blank in (0..breaks).rev() {
                self.out.push("\n")?;
                if blank > 0 {
                    self.blank_prefix()?;
                }
            }
        }
        if start || breaks > 0 {
            self.pending = Pending::Nothing;
            self.out.push(&self.continued)?;
            let mut first = String::new();
            for prefix in &mut self.prefixes[self.started..] {
                match prefix {
                    Prefix::Quote => first.push_str("> "),
                    Prefix::Item { marker, width } => match marker.take() {
                        Some(marker) => first.push_str(&marker.text()),
                        None => first.extend(std::iter::repeat_n(' ', *width)),
                    },
                }
                prefix.continue_line(&mut self.continued);
            }
            self.started = self.prefixes.len();
            self.out.push(&first)?;
        }
        Ok(())
    }

    /// The prefixes of a blank line: its quotes, without trailing whitespace.
    fn blank_prefix(&mut self) -> Result<(), Spent> {
        let quotes = self
            .prefixes
            .iter()
            .rposition(|prefix| matches!(prefix, Prefix::Quote));
        let Some(last) = quotes else {
            return Ok(());
        };
        let mut line = String::new();
        for prefix in &self.prefixes[..last] {
            prefix.continue_line(&mut line);
        }
        line.push('>');
        self.out.push(&line)
    }

    /// Writes what is owed before inline content: line breaks and prefixes, a
    /// heading's mark or a space, then the owed opening marks.
    fn begin_inline(&mut self) -> Result<(), Spent> {
        self.begin_line()?;
        if let Some(level) = self.heading.take() {
            self.out.push(level.mark())?;
        } else if self.pending == Pending::Space && !self.out.text.ends_with(' ') {
            self.out.push(" ")?;
        }
        self.pending = Pending::Nothing;
        // A `!` before a link's bracket would read as an image.
        if let Some(Mark::Link(_)) = self.owed.first()
            && self.out.text.ends_with('!')
        {
            self.out.text.pop();
            self.out.push("\\!")?;
        }
        let owed = mem::take(&mut self.owed);
        for mark in &owed {
            match mark {
                Mark::Emphasis(emphasis) => self.out.push(emphasis.mark())?,
                Mark::Link(_) => self.out.push("[")?,
                Mark::Item(marker) => self.out.push(&marker.text())?,
            }
        }
        self.open.extend(owed);
        Ok(())
    }

    /// Block syntax at the start of a line, before any owed inline marks.
    pub(super) fn syntax(&mut self, text: &str) -> Result<(), Spent> {
        self.flush()?;
        self.begin_line()?;
        self.out.push(text)
    }

    pub(super) fn text(&mut self, text: &str) -> Result<(), Spent> {
        if let Some(code) = &mut self.code {
            code.push_str(text);
            return Ok(());
        }
        if !text.is_empty() && !self.deferred.is_empty() {
            self.flush()?;
        }
        for (index, word) in text.split(is_space).enumerate() {
            if index > 0 {
                self.flush()?;
                self.space();
            }
            self.word.push_str(word);
        }
        Ok(())
    }

    /// Writes the word read so far, owing the marks opened inside it.
    pub(super) fn flush(&mut self) -> Result<(), Spent> {
        if self.word.is_empty() {
            return Ok(());
        }
        let mut word = mem::take(&mut self.word);
        let escaped = escape(&word, self.starts_line());
        self.begin_inline()?;
        self.out.push(&escaped)?;
        word.clear();
        self.word = word;
        self.owed.append(&mut self.deferred);
        Ok(())
    }

    pub(super) fn image(&mut self, image: NodeRef) -> Result<(), Spent> {
        let Some(src) = image.attr("src").filter(|src| !super::inline_data(src)) else {
            return Ok(());
        };
        let alt = image.attr("alt").unwrap_or_default();
        self.flush()?;
        self.begin_inline()?;
        self.out.push("![")?;
        self.text(alt.trim_matches(is_space))?;
        self.flush()?;
        self.pending = Pending::Nothing;
        self.out.push("](")?;
        self.destination(&src)?;
        self.out.push(")")
    }

    pub(super) fn begin_code(&mut self) {
        self.code = Some(String::new());
    }

    pub(super) fn end_code_block(&mut self, language: &str) -> Result<(), Spent> {
        let Some(code) = self.code.take() else {
            return Ok(());
        };
        let code = code.trim_end_matches('\n');
        if code.trim().is_empty() {
            return Ok(());
        }
        let fence = "`".repeat(longest_run(code, '`').max(2) + 1);
        self.block()?;
        self.syntax(&fence)?;
        self.out.push(language)?;
        for line in code.lines() {
            self.line()?;
            self.syntax(line)?;
        }
        self.line()?;
        self.syntax(&fence)?;
        self.block()
    }

    /// Code inline, with its whitespace collapsed as in other inline text, since a
    /// code span holds no line break. An empty one leaves the word around it whole.
    pub(super) fn end_code_span(&mut self) -> Result<(), Spent> {
        let Some(code) = self.code.take() else {
            return Ok(());
        };
        if code.is_empty() {
            return Ok(());
        }
        self.flush()?;
        if code.starts_with(is_space) {
            self.space();
        }
        let mut text = String::with_capacity(code.len());
        for word in code.split(is_space).filter(|word| !word.is_empty()) {
            if !text.is_empty() {
                text.push(' ');
            }
            match self.in_cell() {
                false => text.push_str(word),
                true => text.push_str(&word.replace('|', "\\|")),
            }
        }
        if !text.is_empty() {
            let ticks = longest_run(&text, '`') + 1;
            let pad = if text.starts_with('`') || text.ends_with('`') {
                " "
            } else {
                ""
            };
            self.begin_inline()?;
            self.out.repeat('`', ticks)?;
            self.out.push(pad)?;
            self.out.push(&text)?;
            self.out.push(pad)?;
            self.out.repeat('`', ticks)?;
        }
        if code.ends_with(is_space) {
            self.space();
        }
        Ok(())
    }
}

fn is_space(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '\r' | '\x0c')
}

fn longest_run(text: &str, of: char) -> usize {
    let runs = text.split(|ch| ch != of);
    runs.map(str::len).max().unwrap_or(0)
}

/// `word` with the characters escaped that would read as inline syntax or a
/// table cell's end, and as block syntax where it starts a line.
fn escape(word: &str, line_start: bool) -> Cow<'_, str> {
    let ordinal = line_start && ordinal(word);
    // Quote openers, and setext, thematic-break or list marker lines, where a
    // line starts; a run of `#` opens a heading there and closes one anywhere.
    let repeated = word.bytes().all(|byte| byte == word.as_bytes()[0]);
    let lead = (line_start
        && (word.starts_with('>') || (word.starts_with(['=', '-', '+']) && repeated)))
        || (word.starts_with('#') && repeated);
    let inline = |index: usize, ch: char| match ch {
        '\\' | '*' | '`' | '[' | ']' | '|' | '~' => true,
        // `_` between letters or digits never delimits emphasis; `*` and `~` can.
        '_' => {
            let alphanumeric = |ch: Option<char>| ch.is_some_and(char::is_alphanumeric);
            !(alphanumeric(word[..index].chars().next_back())
                && alphanumeric(word[index + 1..].chars().next()))
        }
        '&' => entity(&word[index + 1..]),
        '<' => word[index + 1..]
            .starts_with(|next: char| next.is_ascii_alphabetic() || "/!?".contains(next)),
        _ => false,
    };
    if !ordinal && !lead && !word.char_indices().any(|(index, ch)| inline(index, ch)) {
        return Cow::Borrowed(word);
    }
    let mut escaped = String::with_capacity(word.len() + 4);
    for (index, ch) in word.char_indices() {
        if (lead && index == 0) || (ordinal && index == word.len() - 1) || inline(index, ch) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    Cow::Owned(escaped)
}

/// Whether text after a `&` reads as an entity reference, such as `amp;` or `#38;`.
fn entity(rest: &str) -> bool {
    let rest = rest.strip_prefix('#').unwrap_or(rest);
    let name = rest.bytes().take_while(u8::is_ascii_alphanumeric).count();
    name > 0 && rest.as_bytes().get(name) == Some(&b';')
}

/// Whether a word reads as an ordered list marker, such as `1.` or `2)`.
fn ordinal(word: &str) -> bool {
    word.strip_suffix(['.', ')']).is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}
