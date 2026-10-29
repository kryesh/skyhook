//! How one string is shortened, with the record of what that leaves out.
use serde_json::Value;

use super::{HEAD_SHARE, TEXT_BYTES, TEXT_LINES, pool::Text};
use crate::job::output::{FieldPointer, OutputTruncation};

/// The prefix of `text` kept within `bytes` (and `lines`, preferring whole lines
/// there), and its record when it is shorter than the string.
pub(super) fn shorten(
    text: &Text,
    field: &FieldPointer,
    bytes: usize,
    lines: Option<usize>,
) -> (Value, Option<OutputTruncation>) {
    let prefix = &text.prefix;
    let line_end = lines.and_then(|lines| {
        prefix
            .iter()
            .enumerate()
            .filter(|(_, byte)| **byte == b'\n')
            .nth(lines - 1)
            .map(|(index, _)| index + 1)
    });
    let mut end = bytes.min(prefix.len()).min(line_end.unwrap_or(usize::MAX));
    end = match std::str::from_utf8(&prefix[..end]) {
        Ok(_) => end,
        Err(error) => error.valid_up_to(),
    };
    if lines.is_some()
        && end < text.bytes
        && let Some(last) = prefix[..end].iter().rposition(|&byte| byte == b'\n')
    {
        end = last + 1;
    }
    // A cut between CR and LF would split one line terminator across pages.
    if end > 0 && prefix.get(end) == Some(&b'\n') && prefix[end - 1] == b'\r' {
        end -= 1;
    }
    let kept = String::from_utf8_lossy(&prefix[..end]).into_owned();
    if end >= text.bytes {
        return (kept.into(), None);
    }
    let line = 1 + prefix[..end].iter().filter(|&&byte| byte == b'\n').count();
    let offset =
        (prefix[..end].iter().rposition(|&byte| byte == b'\n')).map_or(end, |last| end - last - 1);
    let record = OutputTruncation::Text {
        field: field.clone(),
        total_lines: total_lines(text),
        next_start: line,
        next_offset: (offset != 0).then_some(offset),
    };
    (kept.into(), Some(record))
}

/// `text` within the text-field limits, keeping its first and last whole lines
/// around a marker line when it is over them: the head its share of the limits,
/// the tail what that leaves. Text whose last line alone is over the tail's
/// bytes is cut at its end instead, which a position inside a line continues.
pub(super) fn ends(text: &Text, field: &FieldPointer) -> (Value, Option<OutputTruncation>) {
    let (head_only, record) = shorten(text, field, TEXT_BYTES, Some(TEXT_LINES));
    if record.is_none() {
        return (head_only, record);
    }
    let total_lines = total_lines(text);
    let share = |limit: usize| limit * HEAD_SHARE.0 / HEAD_SHARE.1;
    let (head, head_lines) = leading_lines(&text.prefix, share(TEXT_LINES), share(TEXT_BYTES));
    let (tail, tail_lines) = trailing_lines(
        &text.end,
        text.end.len() == text.bytes,
        (TEXT_LINES - share(TEXT_LINES)).min(total_lines - head_lines),
        (TEXT_BYTES - head.len()).min(text.bytes - head.len()),
        <[u8]>::len,
    );
    if tail_lines == 0 {
        return (head_only, record);
    }
    let (next_start, tail_start) = (head_lines + 1, total_lines - tail_lines + 1);
    let kept = format!(
        "{}… lines {next_start}–{} omitted …\n{}",
        String::from_utf8_lossy(head),
        tail_start - 1,
        String::from_utf8_lossy(tail),
    );
    let record = OutputTruncation::TextGap {
        field: field.clone(),
        total_lines,
        next_start,
        tail_start,
    };
    (kept.into(), Some(record))
}

fn total_lines(text: &Text) -> usize {
    text.newlines + usize::from(text.bytes > 0 && !text.ends_line)
}

/// The leading whole lines of `prefix` within `lines` and `bytes`, and their count.
fn leading_lines(prefix: &[u8], lines: usize, bytes: usize) -> (&[u8], usize) {
    let ends = prefix
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'\n')
        .map(|(index, _)| index + 1);
    let (count, end) = (ends.take(lines).take_while(|end| *end <= bytes).enumerate())
        .last()
        .map_or((0, 0), |(index, end)| (index + 1, end));
    (&prefix[..end], count)
}

/// The trailing whole lines of `end`, a string's last bytes, and their count:
/// at most `lines`, whose `cost`s, each line's with its terminator, are together
/// within `budget`. Its first bytes start a line only when it is the `whole`
/// string.
pub(in crate::job::output) fn trailing_lines(
    end: &[u8],
    whole: bool,
    lines: usize,
    budget: usize,
    cost: impl Fn(&[u8]) -> usize,
) -> (&[u8], usize) {
    let body = end.strip_suffix(b"\n").unwrap_or(end);
    let (mut start, mut count, mut used) = (end.len(), 0, 0);
    while count < lines && start > 0 {
        let line_end = if count == 0 { body.len() } else { start - 1 };
        let line_start = match body[..line_end].iter().rposition(|&byte| byte == b'\n') {
            Some(newline) => newline + 1,
            None if whole => 0,
            None => break,
        };
        used += cost(&end[line_start..start]);
        if used > budget {
            break;
        }
        (start, count) = (line_start, count + 1);
    }
    (&end[start..], count)
}
