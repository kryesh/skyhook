//! Stateless line reads over immutable or append-only saved fields.
use super::*;
use super::{json::saved_json, render::TextField};
use grep_matcher::Matcher as _;
use std::{
    collections::VecDeque,
    io::{BufRead, Cursor},
};
use struson::reader::JsonReader as _;

const READ_AHEAD: usize = 256 * 1024;
/// The longest source line a pattern is matched against.
const REGEX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// A pageable field: a stored capture, or a value held in memory.
pub(super) enum Source {
    Capture(CaptureReader),
    Memory(Cursor<Vec<u8>>),
}

/// The readable extent observed when a page starts; later appends are left for
/// the next page.
#[derive(Clone, Copy)]
struct LineIndex {
    bytes: u64,
    total_lines: usize,
}

impl Read for Source {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Capture(capture) => capture.read(buffer),
            Self::Memory(cursor) => cursor.read(buffer),
        }
    }
}

/// Reads one capture's chunks on demand, holding the connection only per fetch.
pub(super) struct CaptureReader {
    db: crate::session::SharedDb,
    capture: i64,
    position: u64,
    cached: (u64, Vec<u8>),
}

impl CaptureReader {
    pub(super) fn new(db: crate::session::SharedDb, capture: i64) -> Self {
        Self {
            db,
            capture,
            position: 0,
            cached: (0, Vec::new()),
        }
    }

    fn index(&self) -> Result<LineIndex, ToolError> {
        let extent = (self.db.capture_extent(self.capture)).map_err(std::io::Error::other)?;
        Ok(LineIndex {
            bytes: extent.bytes,
            total_lines: usize::try_from(extent.newlines).unwrap_or(usize::MAX)
                + usize::from(extent.bytes > 0 && !extent.ends_line),
        })
    }

    /// Position the reader at or before the start of one-based `line`, returning
    /// that offset and its line number.
    fn checkpoint(&mut self, line: usize) -> Result<(u64, usize), ToolError> {
        let (offset, line) =
            (self.db.capture_line(self.capture, line as u64)).map_err(std::io::Error::other)?;
        self.position = offset;
        Ok((offset, usize::try_from(line).unwrap_or(usize::MAX)))
    }
}

impl Read for CaptureReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let (start, data) = &self.cached;
        if self.position < *start || self.position >= start + data.len() as u64 {
            let chunks = self
                .db
                .capture_chunks(self.capture, self.position, READ_AHEAD)
                .map_err(std::io::Error::other)?;
            let Some(first) = chunks.first() else {
                return Ok(0);
            };
            let start = first.0;
            let mut data = Vec::new();
            for (offset, chunk) in chunks {
                if offset != start + data.len() as u64 {
                    return Err(std::io::Error::other("saved output changed while reading"));
                }
                data.extend_from_slice(&chunk);
            }
            self.cached = (start, data);
        }
        let (start, data) = &self.cached;
        if self.position < *start {
            return Err(std::io::Error::other("saved output changed while reading"));
        }
        let available = data
            .get((self.position - start) as usize..)
            .unwrap_or_default();
        let count = available.len().min(buffer.len());
        buffer[..count].copy_from_slice(&available[..count]);
        self.position += count as u64;
        Ok(count)
    }
}

fn check_cancelled(cancellation: &super::super::CancellationToken) -> Result<(), ToolError> {
    if cancellation.is_cancelled() {
        Err(ToolError::cancelled())
    } else {
        Ok(())
    }
}

pub(super) fn not_utf8() -> ToolError {
    ToolError::failed("saved output is not UTF-8")
}

/// A line's text without its LF or CRLF terminator.
fn without_terminator(line: &str) -> &str {
    line.strip_suffix('\n')
        .map_or(line, |line| line.strip_suffix('\r').unwrap_or(line))
}

pub(super) fn empty(selection: &Selection, total: Option<usize>, terminal: bool) -> LinePage {
    response(
        selection,
        total,
        PageLines::Text(Vec::new()),
        if terminal {
            None
        } else {
            Some((selection.start, selection.offset))
        },
    )
}

fn response(
    selection: &Selection,
    total: Option<usize>,
    lines: PageLines,
    next: Option<(usize, usize)>,
) -> LinePage {
    LinePage {
        field: Some(selection.field.clone()),
        lines,
        total_lines: total,
        next_start: next.map(|(start, _)| start),
        next_offset: next.map(|(_, offset)| offset).filter(|offset| *offset != 0),
    }
}

fn invalid_offset() -> ToolError {
    ToolError::invalid_arguments(
        "offset must be within the starting line at a UTF-8 character boundary",
    )
}

/// A text field read forward from a checkpoint, counting the lines it passes.
struct Text<R> {
    reader: R,
    /// The line the next byte belongs to.
    line: usize,
    /// Whether the next byte starts a line. A checkpoint may fall inside its line,
    /// but only before the line a page skips to, so this is exact once it matters.
    line_start: bool,
    /// The field's line count, when its extent was indexed up front.
    total: Option<usize>,
}

impl<R: BufRead> Text<R> {
    fn new(reader: R, line: usize, total: Option<usize>) -> Self {
        Self {
            reader,
            line,
            line_start: true,
            total,
        }
    }

    /// Append the next piece of at most `maximum` bytes to `out`: up to and
    /// including a newline, or as far as the text goes.
    fn piece(&mut self, maximum: usize, out: &mut Vec<u8>) -> std::io::Result<usize> {
        let start = out.len();
        (&mut self.reader)
            .take(maximum as u64)
            .read_until(b'\n', out)?;
        if let Some(&last) = out[start..].last() {
            self.line_start = last == b'\n';
            self.line += usize::from(self.line_start);
        }
        Ok(out.len() - start)
    }

    fn at_end(&mut self) -> std::io::Result<bool> {
        Ok(self.reader.fill_buf()?.is_empty())
    }

    fn skip_to(
        &mut self,
        line: usize,
        cancellation: &super::super::CancellationToken,
    ) -> Result<(), ToolError> {
        let mut skipped = Vec::new();
        while self.line < line {
            check_cancelled(cancellation)?;
            skipped.clear();
            if self.piece(IO_BUFFER_BYTES, &mut skipped)? == 0 {
                break;
            }
        }
        Ok(())
    }

    /// Whether `line`, which this text was just skipped to, does not exist.
    fn beyond(&mut self, line: usize) -> std::io::Result<bool> {
        Ok(self.line < line || (self.line_start && self.at_end()?))
    }

    /// The field's line count: as indexed, or read through to the end.
    fn total(
        &mut self,
        cancellation: &super::super::CancellationToken,
    ) -> Result<usize, ToolError> {
        if let Some(total) = self.total {
            return Ok(total);
        }
        let mut skipped = Vec::new();
        loop {
            check_cancelled(cancellation)?;
            skipped.clear();
            if self.piece(IO_BUFFER_BYTES, &mut skipped)? == 0 {
                return Ok(self.line - usize::from(self.line_start));
            }
        }
    }

    fn read_piece(&mut self, maximum: usize) -> std::io::Result<Vec<u8>> {
        let mut raw = Vec::new();
        self.piece(maximum, &mut raw)?;
        Ok(raw)
    }

    fn seek_offset(
        &mut self,
        offset: usize,
        cancellation: &super::super::CancellationToken,
    ) -> Result<(), ToolError> {
        let mut remaining = offset;
        let mut previous = None;
        while remaining > 0 {
            check_cancelled(cancellation)?;
            let buffer = self.reader.fill_buf()?;
            let count = remaining.min(buffer.len());
            if count == 0 || buffer[..count].contains(&b'\n') {
                return Err(invalid_offset());
            }
            previous = Some(buffer[count - 1]);
            self.reader.consume(count);
            self.line_start = false;
            remaining -= count;
        }
        check_offset_end(previous, self.reader.fill_buf()?.first().copied())
    }
}

/// An offset may not split a character or a CRLF terminator.
fn check_offset_end(previous: Option<u8>, next: Option<u8>) -> Result<(), ToolError> {
    if next.is_some_and(|byte| byte & 0xc0 == 0x80)
        || (previous == Some(b'\r') && next == Some(b'\n'))
    {
        return Err(invalid_offset());
    }
    Ok(())
}

/// Validate `offset` against a starting line read whole, or as far as it exists.
fn check_offset(raw: &[u8], offset: usize) -> Result<(), ToolError> {
    if offset > raw.len() || raw[..offset].contains(&b'\n') {
        return Err(invalid_offset());
    }
    let previous = offset.checked_sub(1).map(|index| raw[index]);
    check_offset_end(previous, raw.get(offset).copied())
}

// Prefer a whole line, and only fragment on an otherwise empty page. Account for
// actual JSON escaping, rather than assuming six output bytes per source byte.
// `budget` is the page's content budget less any per-line framing.
fn fit_line(
    text: &str,
    used: usize,
    budget: usize,
    full_line: bool,
) -> Result<Option<(String, usize, usize)>, ToolError> {
    let mut maximum = text.len().min(budget);
    while !text.is_char_boundary(maximum) {
        maximum -= 1;
    }
    let full_line = full_line && maximum == text.len();
    let text = &text[..maximum];
    if full_line {
        let size = serde_json::to_vec(text)?.len() + 1;
        if used + size <= budget {
            return Ok(Some((text.to_owned(), text.len(), size)));
        }
    }
    if used != 0 {
        return Ok(None);
    }
    let mut low = 0;
    let boundaries: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(text.len()))
        .collect();
    let mut high = boundaries.len() - 1;
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        let size = serde_json::to_vec(&text[..boundaries[middle]])?.len() + 1;
        if size <= budget {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    let stop = boundaries[low];
    let value = text[..stop].to_owned();
    let size = serde_json::to_vec(&value)?.len() + 1;
    Ok(Some((value, stop, size)))
}

pub(super) fn page(
    text: Option<TextField>,
    selection: &Selection,
    terminal: bool,
    cancellation: &super::super::CancellationToken,
) -> Result<LinePage, ToolError> {
    match text {
        None => Ok(empty(
            selection,
            if terminal { Some(0) } else { None },
            terminal,
        )),
        Some(TextField::Bytes(Source::Capture(capture))) => {
            let index = capture.index()?;
            observed(capture, index, selection, terminal, cancellation)
        }
        // Finished text is read from its start, and counted by reading to its end.
        Some(TextField::Bytes(Source::Memory(text))) => {
            read(Text::new(text, 1, None), selection, terminal, cancellation)
        }
        Some(TextField::Stored(mut json)) => {
            let string = json.reader.next_string_reader().map_err(saved_json)?;
            let reader = BufReader::with_capacity(IO_BUFFER_BYTES, string);
            read(
                Text::new(reader, 1, None),
                selection,
                terminal,
                cancellation,
            )
        }
    }
}

/// A page of saved bytes, read no further than the extent `index` observed as the
/// page started.
fn observed(
    mut capture: CaptureReader,
    index: LineIndex,
    selection: &Selection,
    terminal: bool,
    cancellation: &super::super::CancellationToken,
) -> Result<LinePage, ToolError> {
    // Checked against the observed extent before checkpointing: chunks a producer
    // appends later are chosen only for lines past that extent.
    if selection.start > index.total_lines {
        return beyond(selection, index.total_lines, terminal);
    }
    let (offset, line) = capture.checkpoint(first_line(selection))?;
    // A producer that truncates and rewrites since the index can move a checkpoint
    // past it; that page reads nothing, and the next one indexes afresh.
    let observed = index.bytes.saturating_sub(offset);
    let reader = BufReader::with_capacity(IO_BUFFER_BYTES, capture.take(observed));
    let input = Text::new(reader, line, Some(index.total_lines));
    read(input, selection, terminal, cancellation)
}

fn read<R: BufRead>(
    mut input: Text<R>,
    selection: &Selection,
    terminal: bool,
    cancellation: &super::super::CancellationToken,
) -> Result<LinePage, ToolError> {
    if selection.matcher.is_some() {
        return search(input, selection, terminal, cancellation);
    }
    input.skip_to(selection.start, cancellation)?;
    if input.beyond(selection.start)? {
        return beyond(selection, input.total(cancellation)?, terminal);
    }
    input.seek_offset(selection.offset, cancellation)?;
    let mut line = selection.start;
    let mut offset = selection.offset;
    let mut lines = Vec::new();
    let mut used = 0;
    let exhausted = loop {
        if input.at_end()? {
            break true;
        }
        if lines.len() == selection.limit {
            break false;
        }
        check_cancelled(cancellation)?;
        let mut raw = input.read_piece(CONTENT_BYTES + 4)?;
        let newline = raw.ends_with(b"\n");
        let full = newline || input.at_end()?;
        let valid = match std::str::from_utf8(&raw) {
            Ok(_) => raw.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => return Err(not_utf8()),
        };
        if terminal && full && valid < raw.len() {
            return Err(not_utf8());
        }
        // A live trailing CR may become part of CRLF. Keep the returned offset
        // valid if the LF arrives between requests.
        let valid = if !terminal && full && raw.last() == Some(&b'\r') {
            valid.saturating_sub(1)
        } else {
            valid
        };
        let deferred = valid < raw.len();
        raw.truncate(valid);
        if raw.is_empty() {
            break false;
        }
        let piece = without_terminator(std::str::from_utf8(&raw).expect("validated UTF-8"));
        let Some((value, count, size)) = fit_line(piece, used, CONTENT_BYTES, full && !deferred)?
        else {
            break false;
        };
        used += size;
        lines.push(value);
        offset += count;
        if count < piece.len() || !full || deferred {
            break false;
        }
        if newline {
            line += 1;
            offset = 0;
        }
    };
    let total = input.total(cancellation)?;
    Ok(response(
        selection,
        Some(total),
        PageLines::Text(lines),
        if exhausted && terminal {
            None
        } else {
            Some((line, offset))
        },
    ))
}

/// The empty page of a starting line past the end, where only offset 0 is valid.
fn beyond(selection: &Selection, total: usize, terminal: bool) -> Result<LinePage, ToolError> {
    if selection.offset != 0 {
        return Err(invalid_offset());
    }
    Ok(empty(selection, Some(total), terminal))
}

/// The first line a page reads: its start, or with a pattern, its leading context.
fn first_line(selection: &Selection) -> usize {
    match selection.matcher {
        Some(_) => selection.start.saturating_sub(selection.context).max(1),
        None => selection.start,
    }
}

fn search<R: BufRead>(
    mut input: Text<R>,
    selection: &Selection,
    terminal: bool,
    cancellation: &super::super::CancellationToken,
) -> Result<LinePage, ToolError> {
    let matcher = selection.matcher.as_ref().expect("search matcher");
    let mut line = first_line(selection);
    input.skip_to(line, cancellation)?;
    // The starting line is validated when it is read, whether or not it matches.
    let mut reached = false;
    let mut lookahead = VecDeque::<(String, bool)>::new();
    let mut after = 0;
    let mut lines = Vec::new();
    let mut used = 0;
    let mut offset = if line == selection.start {
        selection.offset
    } else {
        0
    };
    let mut exhausted = false;
    loop {
        check_cancelled(cancellation)?;
        if lines.len() >= selection.limit {
            break;
        }
        while lookahead.len() <= selection.context {
            let starting = line + lookahead.len() == selection.start;
            if starting && input.beyond(selection.start)? {
                return beyond(selection, input.total(cancellation)?, terminal);
            }
            let raw = input.read_piece(REGEX_LINE_BYTES + 1)?;
            if raw.len() > REGEX_LINE_BYTES {
                // Leading context is only read for a starting line that exists; a
                // stored string learns whether it does by reading on to it.
                if line + lookahead.len() < selection.start {
                    input.skip_to(selection.start, cancellation)?;
                    if input.beyond(selection.start)? {
                        return beyond(selection, input.total(cancellation)?, terminal);
                    }
                }
                return Err(ToolError::failed(format!(
                    "regex source line exceeds {} MiB; read this field without a pattern",
                    REGEX_LINE_BYTES >> 20
                )));
            }
            if starting {
                check_offset(&raw, selection.offset)?;
                reached = true;
            }
            if raw.is_empty() || (!terminal && !raw.ends_with(b"\n")) {
                break;
            }
            let raw = String::from_utf8(raw).map_err(|_| not_utf8())?;
            let text = without_terminator(&raw).to_owned();
            let matched = matcher
                .is_match(text.as_bytes())
                .map_err(ToolError::failed)?;
            lookahead.push_back((text, matched));
        }
        if lookahead.is_empty() {
            exhausted = terminal;
            break;
        }
        if !terminal && lookahead.len() <= selection.context {
            break;
        }
        let selected = after > 0 || lookahead.iter().any(|(_, matched)| *matched);
        let (text, matched) = lookahead.front().expect("lookahead");
        offset = if line == selection.start {
            selection.offset
        } else {
            0
        };
        if line >= selection.start && selected {
            let framing = serde_json::to_vec(&NumberedLine {
                line,
                text: String::new(),
            })?
            .len();
            let budget = CONTENT_BYTES - framing;
            let Some((fitted, count, size)) = fit_line(&text[offset..], used, budget, true)? else {
                break;
            };
            used += size + framing;
            lines.push(NumberedLine { line, text: fitted });
            if count < text.len() - offset {
                offset += count;
                break;
            }
        }
        after = if *matched {
            selection.context
        } else {
            after.saturating_sub(1)
        };
        lookahead.pop_front();
        line += 1;
        offset = if line == selection.start {
            selection.offset
        } else {
            0
        };
        if terminal && lookahead.is_empty() && input.at_end()? {
            exhausted = true;
            break;
        }
    }
    if !reached {
        return beyond(selection, input.total(cancellation)?, terminal);
    }
    if line < selection.start {
        line = selection.start;
        offset = selection.offset;
    }
    let total = input.total(cancellation)?;
    Ok(response(
        selection,
        Some(total),
        PageLines::Numbered(lines),
        if exhausted {
            None
        } else {
            Some((line, offset))
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(start: usize, offset: usize) -> Selection {
        Selection {
            field: "/result/stdout".parse().unwrap(),
            matcher: None,
            context: 0,
            start,
            offset,
            limit: DEFAULT_LIMIT,
        }
    }

    /// `text` as a field's own bytes, or as a string inside a stored container.
    fn saved(text: &str, stored: bool) -> Option<TextField> {
        Some(if stored {
            let json = Box::new(Cursor::new(serde_json::to_vec(text).unwrap()));
            TextField::Stored(super::super::render::JsonField::new(
                json,
                Default::default(),
            ))
        } else {
            TextField::Bytes(Source::Memory(Cursor::new(text.as_bytes().to_vec())))
        })
    }

    /// A string streamed from a stored container pages exactly as its own bytes.
    #[test]
    fn whole_lines_are_preferred_and_large_lines_are_fully_accessible() {
        let text = format!(
            "{}\n{}\n{}",
            "a".repeat(CONTENT_BYTES / 2),
            "b".repeat(CONTENT_BYTES / 2),
            "🦀\"\\".repeat(CONTENT_BYTES)
        );
        let read = |query: &Selection, stored| {
            page(saved(&text, stored), query, true, &Default::default()).unwrap()
        };
        let first = read(&selection(1, 0), false);
        assert_eq!(first.lines(), [text.lines().next().unwrap()]);
        assert_eq!((first.next_start, first.next_offset), (Some(2), None));
        let mut query = selection(1, 0);
        let mut reconstructed = String::new();
        let mut previous = 1;
        for _ in 0..100 {
            let view = read(&query, false);
            assert!(serde_json::to_vec(&view).unwrap().len() <= PAGE_BYTES);
            assert_eq!(view, read(&query, true));
            for (index, row) in view.lines().into_iter().enumerate() {
                let number = (query.start + index) as u64;
                if number != previous {
                    reconstructed.push('\n');
                }
                reconstructed.push_str(row);
                previous = number;
            }
            let Some(start) = view.next_start else {
                break;
            };
            query = selection(start, view.next_offset.unwrap_or(0));
        }
        assert_eq!(reconstructed, text);
    }

    #[test]
    fn live_search_defers_unfinished_context_and_resumes_without_duplicates() {
        let mut query = selection(1, 0);
        query.matcher = Some(super::super::args::pattern_matcher("ERROR").unwrap());
        query.context = 1;
        let live = "before\nERROR\npar";
        let first = page(saved(live, false), &query, false, &Default::default()).unwrap();
        let numbered = |line, text: &str| NumberedLine {
            line,
            text: text.into(),
        };
        assert_eq!(
            first.lines,
            PageLines::Numbered(vec![numbered(1, "before")])
        );
        assert_eq!(first.next_start.unwrap(), 2);
        // The producer appends before the next page is read.
        let text = format!("{live}tial\nlast\n");
        query.start = 2;
        let rest = page(saved(&text, false), &query, true, &Default::default()).unwrap();
        assert_eq!(
            rest.lines,
            PageLines::Numbered(vec![numbered(2, "ERROR"), numbered(3, "partial")])
        );
        assert!(rest.next_start.is_none());
        query.start = 3;
        query.offset = 1;
        let rest = page(saved(&text, false), &query, true, &Default::default()).unwrap();
        assert_eq!(rest.lines, PageLines::Numbered(vec![numbered(3, "artial")]));
    }

    /// Offsets are validated at the starting line, whether or not a search
    /// matches it, and a start past the end pages nothing.
    #[test]
    fn offsets_stay_within_the_starting_line_and_past_the_end_is_empty() {
        let text = "é\r\nab\n";
        let matcher = super::super::args::pattern_matcher("zzz").unwrap();
        for (stored, matcher) in [false, true]
            .into_iter()
            .flat_map(|stored| [None, Some(matcher.clone())].map(|matcher| (stored, matcher)))
        {
            let read = |start, offset| {
                let query = Selection {
                    matcher: matcher.clone(),
                    ..selection(start, offset)
                };
                page(saved(text, stored), &query, true, &Default::default())
            };
            for (start, offset) in [(1, 1), (1, 3), (2, 3), (3, 1)] {
                let error = read(start, offset).unwrap_err();
                assert_eq!(error.to_string(), invalid_offset().to_string());
            }
            for (start, offset) in [(1, 2), (2, 2)] {
                assert!(read(start, offset).is_ok(), "{start}:{offset}");
            }
            let past = read(3, 0).unwrap();
            assert_eq!((past.total_lines, past.next_start), (Some(2), None));
            assert!(past.lines().is_empty());
        }
        // Past the end stays empty even behind context too long to search; the
        // limit applies once the starting line exists.
        let long = "x".repeat(REGEX_LINE_BYTES + 1);
        for stored in [false, true] {
            let read = |start| {
                let query = Selection {
                    matcher: Some(super::super::args::pattern_matcher(".").unwrap()),
                    context: 1,
                    ..selection(start, 0)
                };
                page(saved(&long, stored), &query, true, &Default::default())
            };
            let past = read(2).unwrap();
            assert_eq!((past.total_lines, past.next_start), (Some(1), None));
            assert!(read(1).is_err());
        }
    }

    /// A running job's capture, appended one chunk per string.
    async fn captured(
        chunks: &[&str],
    ) -> (tempfile::TempDir, crate::job::JobManager, SharedDb, i64) {
        let (root, manager, id) = super::super::tests::fixture(None).await;
        let db = manager.output(id).db;
        let field = FieldPointer::result().property("stdout");
        let capture = db.create_capture(id.get(), &field, CaptureKind::Text);
        let capture = capture.unwrap().unwrap();
        append(&db, capture, chunks);
        (root, manager, db, capture)
    }

    fn append(db: &SharedDb, capture: i64, chunks: &[&str]) {
        let mut extent = db.capture_extent(capture).unwrap();
        for chunk in chunks {
            extent = db
                .append_capture(capture, extent, chunk.as_bytes())
                .unwrap();
        }
    }

    /// A capture page starts from the checkpoint of the first line it reads, a
    /// search's leading context included, whether chunks end at lines or split them.
    #[tokio::test]
    async fn capture_pages_checkpoint_at_their_first_line() {
        let numbered = |line, text: &str| NumberedLine {
            line,
            text: text.into(),
        };
        for chunks in [
            &["one\n", "two\n", "three\n", "four\n"][..],
            &["one\nt", "wo\nthr", "ee\nfour\n"],
        ] {
            let (_root, _manager, db, capture) = captured(chunks).await;
            let read = |start, context, limit, pattern: bool| {
                let query = Selection {
                    matcher: pattern.then(|| super::super::args::pattern_matcher(".").unwrap()),
                    context,
                    limit,
                    ..selection(start, 0)
                };
                let source = Source::Capture(CaptureReader::new(db.clone(), capture));
                page(
                    Some(TextField::Bytes(source)),
                    &query,
                    true,
                    &Default::default(),
                )
                .unwrap()
            };
            let plain = read(3, 0, DEFAULT_LIMIT, false);
            assert_eq!(plain.lines(), ["three", "four"]);
            assert_eq!(plain.total_lines, Some(4));
            let found = read(3, 2, DEFAULT_LIMIT, true);
            let expected = vec![numbered(3, "three"), numbered(4, "four")];
            assert_eq!(found.lines, PageLines::Numbered(expected));
            let first = read(3, 2, 1, true);
            assert_eq!(first.lines, PageLines::Numbered(vec![numbered(3, "three")]));
            assert_eq!(first.next_start, Some(4));
            let rest = read(4, 2, 1, true);
            assert_eq!(rest.lines, PageLines::Numbered(vec![numbered(4, "four")]));
            assert_eq!(rest.next_start, None);
        }
    }

    /// A page reads the extent observed as it started; chunks appended since, even
    /// ones holding lines it asks for, are left for the next page.
    #[tokio::test]
    async fn pages_stop_at_the_extent_observed_as_they_start() {
        let (_root, _manager, db, capture) = captured(&[]).await;
        let source = || CaptureReader::new(db.clone(), capture);
        let cancellation = Default::default();
        let before = source().index().unwrap();
        append(&db, capture, &["one\n", "tw"]);
        let partial = source().index().unwrap();
        append(&db, capture, &["o\n", "three\n", "four\n"]);
        let read = |index, start, offset| {
            observed(
                source(),
                index,
                &selection(start, offset),
                false,
                &cancellation,
            )
        };
        let error = read(before, 4, 1).unwrap_err();
        assert_eq!(error.to_string(), invalid_offset().to_string());
        let past = read(before, 4, 0).unwrap();
        assert_eq!((past.total_lines, past.next_start), (Some(0), Some(4)));
        let past = read(partial, 4, 0).unwrap();
        assert!(past.lines().is_empty());
        let live = read(partial, 1, 0).unwrap();
        assert_eq!(live.lines(), ["one", "tw"]);
        assert_eq!((live.total_lines, live.next_start), (Some(2), Some(2)));
        assert_eq!(live.next_offset, Some(2));
        // A rewrite since the index checkpoints line 2 past the observed extent.
        db.truncate_capture(capture, 0).unwrap();
        append(&db, capture, &["aaaa", "bbbb", "cccc"]);
        let rewritten = read(partial, 2, 0).unwrap();
        assert!(rewritten.lines().is_empty());
        assert_eq!(rewritten.next_start, Some(2));
    }
}
