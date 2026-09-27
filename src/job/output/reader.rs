//! Stateless line reads over immutable or append-only saved fields.
use super::*;
use grep_matcher::Matcher as _;
use std::{
    collections::VecDeque,
    io::{BufRead, Cursor, Seek, SeekFrom},
};

const READ_AHEAD: usize = 256 * 1024;
/// The longest source line a pattern is matched against.
const REGEX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// A pageable field: a stored capture, or a value rendered on demand.
pub(crate) enum Source {
    Capture(CaptureReader),
    /// Private renderings never enter shared storage; closing the file discards them.
    Temporary(std::fs::File),
    Memory(Cursor<Vec<u8>>),
}

/// The readable extent observed when a page starts; later appends are left for
/// the next page.
pub(super) struct LineIndex {
    bytes: u64,
    pub(super) total_lines: usize,
}

impl Source {
    pub(super) fn index(
        &mut self,
        cancellation: &super::super::CancellationToken,
    ) -> Result<LineIndex, ToolError> {
        check_cancelled(cancellation)?;
        Ok(match self {
            Self::Capture(capture) => {
                let extent = capture
                    .db
                    .capture_extent(capture.capture)
                    .map_err(std::io::Error::other)?;
                LineIndex {
                    bytes: extent.bytes,
                    total_lines: usize::try_from(extent.newlines).unwrap_or(usize::MAX)
                        + usize::from(extent.bytes > 0 && !extent.ends_line),
                }
            }
            Self::Temporary(file) => {
                let position = file.stream_position()?;
                file.rewind()?;
                let mut reader = BufReader::new(file);
                let mut index = LineIndex {
                    bytes: 0,
                    total_lines: 0,
                };
                let mut ends_line = true;
                loop {
                    check_cancelled(cancellation)?;
                    let bytes = reader.fill_buf()?;
                    if bytes.is_empty() {
                        break;
                    }
                    index.bytes += bytes.len() as u64;
                    index.total_lines += bytes.iter().filter(|&&byte| byte == b'\n').count();
                    ends_line = bytes.last() == Some(&b'\n');
                    let count = bytes.len();
                    reader.consume(count);
                }
                index.total_lines += usize::from(!ends_line);
                reader.seek(SeekFrom::Start(position))?;
                index
            }
            Self::Memory(cursor) => {
                let bytes = cursor.get_ref();
                LineIndex {
                    bytes: bytes.len() as u64,
                    total_lines: bytes.iter().filter(|&&byte| byte == b'\n').count()
                        + usize::from(bytes.last().is_some_and(|&byte| byte != b'\n')),
                }
            }
        })
    }

    /// A position at or before the start of one-based `line`, and its line number.
    fn checkpoint(&self, line: usize) -> Result<(u64, usize), ToolError> {
        match self {
            Self::Capture(capture) => {
                let (offset, line) = capture
                    .db
                    .capture_line(capture.capture, line as u64)
                    .map_err(std::io::Error::other)?;
                Ok((offset, usize::try_from(line).unwrap_or(usize::MAX)))
            }
            Self::Temporary(_) | Self::Memory(_) => Ok((0, 1)),
        }
    }

    fn seek_line(
        reader: &mut BufReader<Self>,
        index: &LineIndex,
        line: usize,
        cancellation: &super::super::CancellationToken,
    ) -> Result<(), ToolError> {
        let (offset, mut current) = reader.get_ref().checkpoint(line)?;
        reader.seek(SeekFrom::Start(offset))?;
        while current < line && reader.stream_position()? < index.bytes {
            check_cancelled(cancellation)?;
            let remaining =
                (index.bytes - reader.stream_position()?).min(usize::MAX as u64) as usize;
            let buffer = reader.fill_buf()?;
            let buffer = &buffer[..buffer.len().min(remaining)];
            if buffer.is_empty() {
                break;
            }
            let count = buffer
                .iter()
                .position(|&b| b == b'\n')
                .map_or(buffer.len(), |n| {
                    current += 1;
                    n + 1
                });
            reader.consume(count);
        }
        Ok(())
    }
}

impl Read for Source {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Capture(capture) => capture.read(buffer),
            Self::Temporary(file) => file.read(buffer),
            Self::Memory(cursor) => cursor.read(buffer),
        }
    }
}

impl Seek for Source {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        match self {
            Self::Capture(capture) => capture.seek(position),
            Self::Temporary(file) => file.seek(position),
            Self::Memory(cursor) => cursor.seek(position),
        }
    }
}

/// Reads one capture's chunks on demand, holding the connection only per fetch.
pub(crate) struct CaptureReader {
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

impl Seek for CaptureReader {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        let target = match position {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
            SeekFrom::End(delta) => self
                .db
                .capture_extent(self.capture)
                .map_err(std::io::Error::other)?
                .bytes
                .checked_add_signed(delta),
        };
        self.position = target.ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid capture seek")
        })?;
        Ok(self.position)
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

pub(super) fn empty(selection: &Selection, total: Option<usize>, terminal: bool) -> OutputPreview {
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
) -> OutputPreview {
    OutputPreview {
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

fn seek_offset(
    reader: &mut BufReader<Source>,
    offset: usize,
    end: u64,
    cancellation: &super::super::CancellationToken,
) -> Result<(), ToolError> {
    let mut remaining = offset;
    let mut previous = None;
    while remaining > 0 {
        check_cancelled(cancellation)?;
        let available = (end - reader.stream_position()?).min(usize::MAX as u64) as usize;
        let buffer = reader.fill_buf()?;
        let count = remaining.min(buffer.len()).min(available);
        if count == 0 || buffer[..count].contains(&b'\n') {
            return Err(invalid_offset());
        }
        previous = Some(buffer[count - 1]);
        reader.consume(count);
        remaining -= count;
    }
    if reader.stream_position()? < end {
        let next = reader.fill_buf()?.first().copied();
        if next.is_some_and(|b| b & 0xc0 == 0x80)
            || (previous == Some(b'\r') && next == Some(b'\n'))
        {
            return Err(invalid_offset());
        }
    }
    Ok(())
}

fn read_piece(
    reader: &mut BufReader<Source>,
    end: u64,
    maximum: usize,
) -> Result<Vec<u8>, ToolError> {
    let count = (end - reader.stream_position()?).min(maximum as u64);
    let mut raw = Vec::new();
    reader.take(count).read_until(b'\n', &mut raw)?;
    Ok(raw)
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
    source: Option<Source>,
    selection: &Selection,
    terminal: bool,
    cancellation: &super::super::CancellationToken,
) -> Result<OutputPreview, ToolError> {
    let Some(mut source) = source else {
        return Ok(empty(
            selection,
            if terminal { Some(0) } else { None },
            terminal,
        ));
    };
    let index = source.index(cancellation)?;
    let mut reader = BufReader::with_capacity(IO_BUFFER_BYTES, source);
    if selection.start > index.total_lines {
        if selection.offset != 0 {
            return Err(invalid_offset());
        }
        return Ok(empty(selection, Some(index.total_lines), terminal));
    }
    if selection.matcher.is_some() {
        return search(reader, &index, selection, terminal, cancellation);
    }
    Source::seek_line(&mut reader, &index, selection.start, cancellation)?;
    seek_offset(&mut reader, selection.offset, index.bytes, cancellation)?;
    let mut line = selection.start;
    let mut offset = selection.offset;
    let mut lines = Vec::new();
    let mut used = 0;
    while lines.len() < selection.limit && reader.stream_position()? < index.bytes {
        check_cancelled(cancellation)?;
        let start = reader.stream_position()?;
        let mut raw = read_piece(&mut reader, index.bytes, CONTENT_BYTES + 4)?;
        let newline = raw.ends_with(b"\n");
        let full = newline || reader.stream_position()? == index.bytes;
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
            reader.seek(SeekFrom::Start(start))?;
            break;
        }
        let text = without_terminator(std::str::from_utf8(&raw).expect("validated UTF-8"));
        let Some((value, count, size)) = fit_line(text, used, CONTENT_BYTES, full && !deferred)?
        else {
            reader.seek(SeekFrom::Start(start))?;
            break;
        };
        used += size;
        lines.push(value);
        if count == text.len() && full && !deferred {
            if newline {
                line += 1;
                offset = 0;
            } else {
                offset += count;
            }
        } else {
            reader.seek(SeekFrom::Start(start + count as u64))?;
            offset += count;
            break;
        }
    }
    let exhausted = reader.stream_position()? == index.bytes;
    Ok(response(
        selection,
        Some(index.total_lines),
        PageLines::Text(lines),
        if exhausted && terminal {
            None
        } else {
            Some((line, offset))
        },
    ))
}

fn search(
    mut reader: BufReader<Source>,
    index: &LineIndex,
    selection: &Selection,
    terminal: bool,
    cancellation: &super::super::CancellationToken,
) -> Result<OutputPreview, ToolError> {
    let matcher = selection.matcher.as_ref().expect("search matcher");
    // Validate independently of whether the starting line matches.
    Source::seek_line(&mut reader, index, selection.start, cancellation)?;
    seek_offset(&mut reader, selection.offset, index.bytes, cancellation)?;
    let mut line = selection.start.saturating_sub(selection.context).max(1);
    Source::seek_line(&mut reader, index, line, cancellation)?;
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
            let raw = read_piece(&mut reader, index.bytes, REGEX_LINE_BYTES + 1)?;
            if raw.len() > REGEX_LINE_BYTES {
                return Err(ToolError::failed(format!(
                    "regex source line exceeds {} MiB; read this field without a pattern",
                    REGEX_LINE_BYTES >> 20
                )));
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
        if terminal && lookahead.is_empty() && reader.stream_position()? == index.bytes {
            exhausted = true;
            break;
        }
    }
    if line < selection.start {
        line = selection.start;
        offset = selection.offset;
    }
    Ok(response(
        selection,
        Some(index.total_lines),
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

    fn saved(text: &str) -> Option<Source> {
        Some(Source::Memory(Cursor::new(text.as_bytes().to_vec())))
    }

    #[test]
    fn whole_lines_are_preferred_and_large_lines_are_fully_accessible() {
        let text = format!(
            "{}\n{}\n{}",
            "a".repeat(CONTENT_BYTES / 2),
            "b".repeat(CONTENT_BYTES / 2),
            "🦀\"\\".repeat(CONTENT_BYTES)
        );
        let first = page(saved(&text), &selection(1, 0), true, &Default::default()).unwrap();
        assert_eq!(first.lines(), [text.lines().next().unwrap()]);
        assert_eq!((first.next_start, first.next_offset), (Some(2), None));
        let mut query = selection(1, 0);
        let mut reconstructed = String::new();
        let mut previous = 1;
        for _ in 0..100 {
            let view = page(saved(&text), &query, true, &Default::default()).unwrap();
            assert!(serde_json::to_vec(&view).unwrap().len() <= PAGE_BYTES);
            assert_eq!(
                view,
                page(saved(&text), &query, true, &Default::default()).unwrap()
            );
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
        let first = page(saved(live), &query, false, &Default::default()).unwrap();
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
        let rest = page(saved(&text), &query, true, &Default::default()).unwrap();
        assert_eq!(
            rest.lines,
            PageLines::Numbered(vec![numbered(2, "ERROR"), numbered(3, "partial")])
        );
        assert!(rest.next_start.is_none());
        query.start = 3;
        query.offset = 1;
        let rest = page(saved(&text), &query, true, &Default::default()).unwrap();
        assert_eq!(rest.lines, PageLines::Numbered(vec![numbered(3, "artial")]));
    }
}
