//! Stateless line reads over immutable or append-only saved fields.
use super::*;
use grep_matcher::Matcher as _;
use std::{
    collections::VecDeque,
    fs::File,
    io::{BufRead, Seek, SeekFrom},
};

const INDEX_STRIDE: usize = 256;

#[derive(Default, Serialize, Deserialize)]
pub(super) struct LineIndex {
    version: u8,
    bytes: u64,
    newlines: usize,
    last_start: u64,
    checkpoints: Vec<u64>,
}

impl LineIndex {
    pub(super) fn load(
        path: &Path,
        cancellation: &super::super::CancellationToken,
    ) -> Result<Self, ToolError> {
        check_cancelled(cancellation)?;
        let mut file = File::open(path)?;
        let length = file.metadata()?.len();
        let index_path = path.with_extension("lines.json");
        let mut index: Self = File::open(&index_path)
            .ok()
            .and_then(|file| serde_json::from_reader(BufReader::new(file)).ok())
            .filter(|index: &Self| {
                index.version == 1
                    && index.bytes <= length
                    && index.checkpoints.len() == index.newlines / INDEX_STRIDE + 1
                    && index.checkpoints.first() == Some(&0)
            })
            .unwrap_or_else(|| Self {
                version: 1,
                checkpoints: vec![0],
                ..Self::default()
            });
        let changed = index.bytes != length || !index_path.exists();
        file.seek(SeekFrom::Start(index.bytes))?;
        let mut buffer = [0; 64 * 1024];
        while index.bytes < length {
            check_cancelled(cancellation)?;
            let maximum = (length - index.bytes).min(buffer.len() as u64) as usize;
            let count = file.read(&mut buffer[..maximum])?;
            if count == 0 {
                return Err(ToolError::Failed(
                    "saved output changed while indexing".into(),
                ));
            }
            for (offset, &byte) in buffer[..count].iter().enumerate() {
                if byte == b'\n' {
                    index.newlines += 1;
                    index.last_start = index.bytes + offset as u64 + 1;
                    if index.newlines.is_multiple_of(INDEX_STRIDE) {
                        index.checkpoints.push(index.last_start);
                    }
                }
            }
            index.bytes += count as u64;
        }
        if changed {
            let mut saved =
                tempfile::NamedTempFile::new_in(path.parent().expect("field directory"))?;
            {
                let mut writer = std::io::BufWriter::new(&mut saved);
                serde_json::to_writer(&mut writer, &index)?;
                writer.flush()?;
            }
            saved.flush()?;
            saved.persist(index_path).map_err(|error| error.error)?;
        }
        Ok(index)
    }

    pub(super) fn total_lines(&self) -> usize {
        self.newlines + usize::from(self.last_start < self.bytes)
    }

    fn seek_line(
        &self,
        reader: &mut BufReader<File>,
        line: usize,
        cancellation: &super::super::CancellationToken,
    ) -> Result<(), ToolError> {
        let checkpoint = ((line - 1) / INDEX_STRIDE).min(self.checkpoints.len() - 1);
        reader.seek(SeekFrom::Start(self.checkpoints[checkpoint]))?;
        let mut current = checkpoint * INDEX_STRIDE + 1;
        while current < line && reader.stream_position()? < self.bytes {
            check_cancelled(cancellation)?;
            let remaining =
                (self.bytes - reader.stream_position()?).min(usize::MAX as u64) as usize;
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

fn check_cancelled(cancellation: &super::super::CancellationToken) -> Result<(), ToolError> {
    if cancellation.is_cancelled() {
        Err(ToolError::Cancelled)
    } else {
        Ok(())
    }
}

pub(super) fn empty(selection: &Selection, total: Option<usize>, terminal: bool) -> OutputPreview {
    response(
        selection,
        total,
        Vec::new(),
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
    lines: Vec<String>,
    next: Option<(usize, usize)>,
) -> OutputPreview {
    OutputPreview {
        field: selection.field.clone(),
        lines,
        total_lines: total,
        next: next.map(|(start, offset)| OutputContinuation { start, offset }),
    }
}

fn invalid_offset() -> ToolError {
    ToolError::InvalidArguments(
        "offset must be within the starting line at a UTF-8 character boundary".into(),
    )
}

fn seek_offset(
    reader: &mut BufReader<File>,
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
    reader: &mut BufReader<File>,
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
fn fit_line(
    text: &str,
    used: usize,
    full_line: bool,
) -> Result<Option<(String, usize, usize)>, ToolError> {
    let mut maximum = text.len().min(CONTENT_BYTES);
    while !text.is_char_boundary(maximum) {
        maximum -= 1;
    }
    let full_line = full_line && maximum == text.len();
    let text = &text[..maximum];
    if full_line {
        let size = serde_json::to_vec(text)?.len() + 1;
        if used + size <= CONTENT_BYTES {
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
        if size <= CONTENT_BYTES {
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
    path: &Path,
    selection: &Selection,
    limit: usize,
    terminal: bool,
    cancellation: &super::super::CancellationToken,
) -> Result<OutputPreview, ToolError> {
    let index = match LineIndex::load(path, cancellation) {
        Ok(index) => index,
        Err(ToolError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(empty(
                selection,
                if terminal { Some(0) } else { None },
                terminal,
            ));
        }
        Err(error) => return Err(error),
    };
    let mut reader = BufReader::new(File::open(path)?);
    if selection.start > index.total_lines() {
        if selection.offset != 0 {
            return Err(invalid_offset());
        }
        return Ok(empty(selection, Some(index.total_lines()), terminal));
    }
    if selection.matcher.is_some() {
        return search(reader, &index, selection, limit, terminal, cancellation);
    }
    index.seek_line(&mut reader, selection.start, cancellation)?;
    seek_offset(&mut reader, selection.offset, index.bytes, cancellation)?;
    let mut line = selection.start;
    let mut offset = selection.offset;
    let mut lines = Vec::new();
    let mut used = 0;
    while lines.len() < limit && reader.stream_position()? < index.bytes {
        check_cancelled(cancellation)?;
        let start = reader.stream_position()?;
        let mut raw = read_piece(&mut reader, index.bytes, CONTENT_BYTES + 4)?;
        let newline = raw.ends_with(b"\n");
        let full = newline || reader.stream_position()? == index.bytes;
        let valid = match std::str::from_utf8(&raw) {
            Ok(_) => raw.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => return Err(ToolError::Failed("saved output is not UTF-8".into())),
        };
        if terminal && full && valid < raw.len() {
            return Err(ToolError::Failed("saved output is not UTF-8".into()));
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
        let text = std::str::from_utf8(&raw).expect("validated UTF-8");
        let text = if newline {
            text.strip_suffix('\n')
                .unwrap()
                .strip_suffix('\r')
                .unwrap_or(text.strip_suffix('\n').unwrap())
        } else {
            text
        };
        let Some((value, count, size)) = fit_line(text, used, full && !deferred)? else {
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
        Some(index.total_lines()),
        lines,
        if exhausted && terminal {
            None
        } else {
            Some((line, offset))
        },
    ))
}

fn search(
    mut reader: BufReader<File>,
    index: &LineIndex,
    selection: &Selection,
    limit: usize,
    terminal: bool,
    cancellation: &super::super::CancellationToken,
) -> Result<OutputPreview, ToolError> {
    let matcher = selection.matcher.as_ref().expect("search matcher");
    // Validate independently of whether the starting line matches.
    index.seek_line(&mut reader, selection.start, cancellation)?;
    seek_offset(&mut reader, selection.offset, index.bytes, cancellation)?;
    let mut line = selection.start.saturating_sub(selection.context).max(1);
    index.seek_line(&mut reader, line, cancellation)?;
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
        if lines.len() >= limit {
            break;
        }
        while lookahead.len() <= selection.context {
            let raw = read_piece(&mut reader, index.bytes, 4 * 1024 * 1024 + 1)?;
            if raw.len() > 4 * 1024 * 1024 {
                return Err(ToolError::Failed(
                    "regex source line exceeds 4 MiB; read this field without a pattern".into(),
                ));
            }
            if raw.is_empty() || (!terminal && !raw.ends_with(b"\n")) {
                break;
            }
            let raw = String::from_utf8(raw)
                .map_err(|_| ToolError::Failed("saved output is not UTF-8".into()))?;
            let text = raw
                .strip_suffix('\n')
                .map(|s| s.strip_suffix('\r').unwrap_or(s))
                .unwrap_or(&raw)
                .to_owned();
            let matched = matcher
                .is_match(text.as_bytes())
                .map_err(|error| ToolError::Failed(error.to_string()))?;
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
            let Some((value, count, size)) = fit_line(&text[offset..], used, true)? else {
                break;
            };
            used += size;
            lines.push(value);
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
        Some(index.total_lines()),
        lines,
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
            field: "/result/stdout".into(),
            matcher: None,
            context: 0,
            start,
            offset,
        }
    }

    fn saved(text: &str) -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("field.txt");
        std::fs::write(&path, text).unwrap();
        (directory, path)
    }

    #[test]
    fn whole_lines_are_preferred_and_large_lines_are_fully_accessible() {
        let text = format!(
            "{}\n{}\n{}",
            "a".repeat(4000),
            "b".repeat(4000),
            "🦀\"\\".repeat(4000)
        );
        let (_directory, path) = saved(&text);
        let first = page(&path, &selection(1, 0), 100, true, &Default::default()).unwrap();
        assert_eq!(first.lines.len(), 1);
        assert_eq!(first.next.unwrap().start, 2);
        assert_eq!(first.next.unwrap().offset, 0);
        let mut query = selection(1, 0);
        let mut reconstructed = String::new();
        let mut previous = 1;
        for _ in 0..100 {
            let view = page(&path, &query, 100, true, &Default::default()).unwrap();
            assert!(serde_json::to_vec(&view).unwrap().len() <= PAGE_BYTES);
            assert_eq!(
                view,
                page(&path, &query, 100, true, &Default::default()).unwrap()
            );
            for (index, row) in view.lines.iter().enumerate() {
                let number = (query.start + index) as u64;
                if number != previous {
                    reconstructed.push('\n');
                }
                reconstructed.push_str(row.as_str());
                previous = number;
            }
            let Some(next) = view.next else {
                break;
            };
            query = selection(next.start, next.offset);
        }
        assert_eq!(reconstructed, text);
    }

    #[test]
    fn live_search_defers_unfinished_context_and_resumes_without_duplicates() {
        let (_directory, path) = saved("before\nERROR\npar");
        let mut query = selection(1, 0);
        query.matcher = Some(std::sync::Arc::new(
            crate::tool::builtins::search::output_matcher("ERROR").unwrap(),
        ));
        query.context = 1;
        let first = page(&path, &query, 100, false, &Default::default()).unwrap();
        assert_eq!(first.lines.len(), 1);
        assert_eq!(first.lines[0], "before");
        assert_eq!(first.next.unwrap().start, 2);
        let mut output = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(output, "tial\nlast\n").unwrap();
        query.start = 2;
        let rest = page(&path, &query, 100, true, &Default::default()).unwrap();
        assert_eq!(
            rest.lines
                .iter()
                .map(|row| row.as_str())
                .collect::<Vec<_>>(),
            vec!["ERROR", "partial"]
        );
        assert!(rest.next.is_none());
        query.start = 3;
        query.offset = 1;
        let rest = page(&path, &query, 100, true, &Default::default()).unwrap();
        assert_eq!(rest.lines[0], "artial");
    }
}
