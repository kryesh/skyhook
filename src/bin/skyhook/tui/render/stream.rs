//! Append-only layout for live prose. Never owns a copy of the input or old rows.
//!
//! Plain prose keeps only source offsets and two unstable wrap rows. Rich Markdown
//! commits complete top-level blocks separated by blank lines, retaining the final
//! block for reparsing. Reference syntax, HTML, and terminal-control cleaning
//! conservatively disable block commits because they can change earlier output.
//! A single giant rich block or open fence remains an explicit full-suffix fallback.
//! Source checkpoints are width-independent; a resize currently reparses to rebuild
//! wrapped rows (resizes are not the routine streaming path).

use super::markdown::{self, LayoutLine};
use super::{Palette, wrap_words};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthStr;

pub(super) type Suffix = (usize, Vec<LayoutLine>);

#[derive(Default)]
pub(super) struct StreamLayout {
    initialized: bool,
    len: usize,
    width: usize,
    plain: Option<Plain>,
    prefix: String,
    stable_bytes: usize,
    stable_rows: usize,
    global: bool,
    // Source offset, first content row (after its separator), plain checkpoint.
    tail_plain: Option<(usize, usize, Plain)>,
}

/// Only byte offsets into the caller's text, plus a bounded wrapping checkpoint.
#[derive(Default)]
struct Plain {
    line_start: usize,
    visible_end: usize,
    tail_start: usize,
    tail_row: usize,
    continued: bool,
    rows: usize,
    // Collapse paragraph gaps, but do not render a trailing blank at EOF.
    pending_blank: bool,
}

impl StreamLayout {
    /// Replace rows starting at the returned index with the returned suffix.
    /// `None` means no change. `append_from` must be the previous byte length,
    /// with an unchanged prefix; use `None` for replacement, even at equal length.
    /// Width changes automatically invalidate the layout.
    #[cfg(test)]
    pub(super) fn update_prefixed(
        &mut self,
        text: &str,
        width: usize,
        p: Palette,
        append_from: Option<usize>,
        prefix: &str,
    ) -> Option<Suffix> {
        self.update_highlighted(text, width, p, append_from, prefix, None)
    }

    pub(super) fn update_highlighted(
        &mut self,
        text: &str,
        width: usize,
        p: Palette,
        append_from: Option<usize>,
        prefix: &str,
        highlights: Option<&super::super::tool_view::HighlightCache>,
    ) -> Option<Suffix> {
        let width = width.max(1);
        // Reserve the first-line prefix even in later fragments: otherwise a
        // committed table could change width compared with a full render.
        let markdown_width = width.saturating_sub(prefix.width());
        let append = self.initialized
            && append_from == Some(self.len)
            && self.len <= text.len()
            && text.is_char_boundary(self.len)
            && self.width == width
            && self.prefix == prefix;
        if append && self.len == text.len() {
            return None;
        }
        let from = if append { self.len } else { 0 };
        if !append {
            self.plain = Some(Plain::default());
            self.stable_bytes = 0;
            self.stable_rows = 0;
            self.global = false;
            self.tail_plain = None;
            self.prefix = prefix.to_owned();
        }
        self.initialized = true;
        self.len = text.len();
        self.width = width;
        if !prefix.is_empty() && width < prefix.len() {
            self.plain = None;
        }
        if let Some(plain) = &mut self.plain {
            if let Some(suffix) = plain.append(text, from, width, prefix, p) {
                return Some(suffix);
            }
            // An unsafe suffix can reinterpret any old prefix (setext headings,
            // references, tables, etc.). Never retain rows when downgrading.
            self.plain = None;
        }
        // Reference definitions and HTML/control cleaning can retroactively affect
        // earlier blocks. Never retain a prefix in those documents.
        self.global |= text[from..]
            .chars()
            .any(|c| matches!(c, '[' | ']' | '<' | '>') || (c.is_control() && c != '\n'));
        if self.global {
            self.stable_bytes = 0;
            self.stable_rows = 0;
        }
        let start = self.stable_bytes;
        let truncate = self.stable_rows;
        let source = &text[start..];
        if !self.global
            && let Some((offset, row, plain)) = &mut self.tail_plain
        {
            if *offset == start
                && let Some((truncate_tail, suffix)) = plain.append(
                    source,
                    from.saturating_sub(start),
                    width,
                    if truncate == 0 { prefix } else { "" },
                    p,
                )
            {
                return Some((*row + truncate_tail, suffix));
            }
            self.tail_plain = None;
        }
        let boundary = if self.global {
            0
        } else {
            stable_boundary(source)
        };
        let mut output = Vec::new();
        if boundary > 0 {
            let block = render(
                &source[..boundary],
                width,
                markdown_width,
                p,
                if truncate == 0 { prefix } else { "" },
                false,
                highlights,
            );
            append_block(&mut output, block, truncate > 0);
            self.stable_bytes += boundary;
            self.stable_rows += output.len();
        }
        let tail = &source[boundary..];
        if boundary > 0 && !self.global && !tail.is_empty() {
            let mut plain = Plain::default();
            if let Some((_, suffix)) = plain.append(
                tail,
                0,
                width,
                if self.stable_rows == 0 { prefix } else { "" },
                p,
            ) && plain.rows > 0
            {
                let row = self.stable_rows + usize::from(self.stable_rows > 0);
                append_block(&mut output, suffix, self.stable_rows > 0);
                self.tail_plain = Some((self.stable_bytes, row, plain));
                return Some((truncate, output));
            }
        }
        if !tail.is_empty() || output.is_empty() {
            let block = render(
                tail,
                width,
                markdown_width,
                p,
                if self.stable_rows == 0 { prefix } else { "" },
                self.stable_rows == 0,
                highlights,
            );
            append_block(&mut output, block, self.stable_rows > 0);
        }
        Some((truncate, output))
    }
}

// Fragments trim their edges. Restore the separator only when another visible
// block follows, never as a trailing blank in the committed prefix.
fn append_block(output: &mut Vec<LayoutLine>, block: Vec<LayoutLine>, separated: bool) {
    if !block.is_empty() {
        if separated {
            output.push((Line::default(), false).into());
        }
        output.extend(block);
    }
}

fn render(
    text: &str,
    width: usize,
    markdown_width: usize,
    p: Palette,
    prefix: &str,
    placeholder: bool,
    highlights: Option<&super::super::tool_view::HighlightCache>,
) -> Vec<LayoutLine> {
    let cleaned = super::model::clean(text);
    markdown::layout_highlighted(
        &cleaned,
        p,
        placeholder,
        width,
        markdown_width,
        prefix,
        highlights,
    )
}

// Look through containers and empty blocks without changing the plain-prose
// fast path. A list item already emits a marker before its table, whereas a
// block quote does not emit a row until its content arrives.
pub(super) fn starts_with_table(text: &str) -> bool {
    use pulldown_cmark::{Event, Parser, Tag};
    for event in Parser::new_ext(text, super::markdown::options()) {
        match event {
            Event::Start(Tag::Table(_)) => return true,
            Event::Start(Tag::BlockQuote(_) | Tag::Paragraph | Tag::Heading { .. })
            | Event::End(_) => {}
            _ => return false,
        }
    }
    false
}

// Only commit complete top-level blocks with an explicit blank-line separator.
// The last block is retained because closing syntax can still reinterpret it.
pub(super) fn stable_boundary(text: &str) -> usize {
    use pulldown_cmark::{Event, Parser};
    let mut depth = 0usize;
    let mut boundary = 0;
    let mut candidate = 0;
    for (event, range) in Parser::new_ext(text, super::markdown::options()).into_offset_iter() {
        match event {
            Event::Start(_) => {
                if depth == 0 && candidate > 0 {
                    let next = &text[candidate..];
                    // A partial list marker (e.g. `1` before `.` arrives) can
                    // still merge with the preceding list. Wait until its first
                    // line is complete, or starts with unambiguous plain prose.
                    if next.contains('\n')
                        || next.chars().next().is_some_and(|c| safe_char(c, true))
                    {
                        boundary = candidate;
                    }
                }
                depth += 1;
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    candidate = 0;
                }
                if depth == 0 && range.end < text.len() {
                    let end = range.end;
                    let gap = text[end..]
                        .bytes()
                        .take_while(|b| matches!(b, b'\n' | b'\r'))
                        .count();
                    if gap > 0 && (text[..end].ends_with('\n') || gap > 1) {
                        candidate = end + gap;
                    }
                }
            }
            _ => {}
        }
    }
    boundary
}

impl Plain {
    fn append(
        &mut self,
        text: &str,
        from: usize,
        width: usize,
        prefix: &str,
        p: Palette,
    ) -> Option<Suffix> {
        // The checkpoint subtracts the whole prefix from rendered bytes. It
        // cannot resume within a prefix that spans several wrapping rows.
        if !prefix.is_empty() && width < prefix.len() {
            return None;
        }
        let truncate = self.tail_row;
        let mut output = Vec::new();
        let mut cursor = from;
        // split_inclusive avoids rescanning any old logical line. Old trailing
        // spaces remain just an offset until they actually become visible.
        for piece in text[from..].split_inclusive('\n') {
            let newline = piece.ends_with('\n');
            let body = piece.strip_suffix('\n').unwrap_or(piece);
            for (offset, ch) in body.char_indices() {
                let at = cursor + offset;
                if !safe_char(ch, at == self.line_start) {
                    return None;
                }
                if ch != ' ' {
                    self.visible_end = at + ch.len_utf8();
                }
            }
            self.emit(text, width, &mut output, prefix, p)?;
            cursor += piece.len();
            if newline {
                if self.visible_end <= self.line_start && self.rows > 0 {
                    self.pending_blank = true;
                }
                self.line_start = cursor;
                self.visible_end = cursor;
                self.tail_start = cursor;
                self.tail_row = self.rows;
                self.continued = false;
            }
        }
        // Initial empty input, or input containing only empty lines, has the
        // same single placeholder row as markdown(). It is not a stable row.
        if self.rows == 0 {
            output.push((Line::from(prefix.to_owned()), false).into());
        }
        Some((truncate, output))
    }

    fn emit(
        &mut self,
        text: &str,
        width: usize,
        output: &mut Vec<LayoutLine>,
        prefix: &str,
        p: Palette,
    ) -> Option<()> {
        if self.visible_end <= self.tail_start {
            return Some(());
        }
        if self.pending_blank {
            output.push((Line::default(), false).into());
            self.rows += 1;
            self.tail_row += 1;
            self.pending_blank = false;
        }
        let start_row = self.tail_row;
        let mut byte = self.tail_start;
        let mut checkpoints = Vec::new();
        let prefix_len = if self.tail_row == 0 { prefix.len() } else { 0 };
        let mut spans = Vec::new();
        if self.tail_row == 0 && !prefix.is_empty() {
            spans.push(Span::raw(prefix.to_owned()));
        }
        spans.push(Span::styled(
            text[byte..self.visible_end].to_owned(),
            Style::default().fg(p.content.fg),
        ));
        let lines = wrap_words(Line::from(spans), width);
        let mut rendered_bytes = 0;
        for (i, line) in lines.into_iter().enumerate() {
            let continued = self.continued || i > 0;
            checkpoints.push((byte, start_row + i, continued));
            rendered_bytes += line
                .spans
                .iter()
                .map(|span| span.content.len())
                .sum::<usize>();
            byte = self.tail_start + rendered_bytes.saturating_sub(prefix_len);
            output.push((line, continued).into());
        }
        self.rows = start_row + checkpoints.len();
        // Retain TWO rows: appending VS16 / ZWJ / combining text can change the
        // last grapheme's width and move it back into the preceding wrap row.
        // A growing word can also exceed `width` and switch from moving intact
        // to the next row to filling the preceding row as a split long token.
        // Once a token spans more than two rows, the checkpoint may safely be
        // inside it: that suffix starts at column zero and wraps identically.
        // wrap_words keeps every source byte (including trailing separators),
        // so rendered byte counts remain exact source checkpoints.
        let &(start, row, continued) = &checkpoints[checkpoints.len().saturating_sub(2)];
        // Degenerate combining sequences/zero-width runs must not turn the
        // checkpoint into an unbounded old-prefix scan on subsequent appends.
        if self.visible_end - start > width.saturating_mul(32).saturating_add(256) {
            return None;
        }
        self.tail_start = start;
        self.tail_row = row;
        self.continued = continued;
        Some(())
    }
}

fn safe_char(ch: char, line_start: bool) -> bool {
    // Combining/format characters can change an earlier grapheme across token
    // or checkpoint boundaries. Reparse these uncommon sequences conservatively.
    if unicode_width::UnicodeWidthChar::width(ch) == Some(0) {
        return false;
    }
    if ch.is_control()
        || matches!(
            ch,
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '&' | '~' | '#' | '|'
        )
    {
        return false;
    }
    // Leading spaces can introduce indented code; digits can become ordered
    // lists when punctuation arrives in a later append. Reject early, rather
    // than needing unbounded lookbehind when that punctuation arrives.
    if line_start && (ch == ' ' || ch.is_ascii_digit() || matches!(ch, '-' | '+' | '=')) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    type CanonicalRow = (Vec<(String, Style)>, Style, bool, markdown::RowLayout);

    fn canonical(rows: &[LayoutLine]) -> Vec<CanonicalRow> {
        rows.iter()
            .map(|row| {
                let mut spans: Vec<(String, Style)> = Vec::new();
                for span in &row.line.spans {
                    if span.content.is_empty() {
                        continue;
                    }
                    if let Some((text, style)) = spans.last_mut()
                        && *style == span.style
                    {
                        text.push_str(&span.content);
                    } else {
                        spans.push((span.content.to_string(), span.style));
                    }
                }
                (spans, row.line.style, row.continued, row.layout.clone())
            })
            .collect()
    }

    fn reference(text: &str, width: usize, p: Palette) -> Vec<LayoutLine> {
        markdown::layout_highlighted(text, p, true, width.max(1), width.max(1), "", None)
    }

    #[test]
    fn markdown_code_geometry_streaming_and_resize_match_saved() {
        let input = "prose `inline`\n\n```rust\nlet answer = 42;  \n\n    \n// long comment with words for wrapping\n```\n\nafter";
        let mut layout = StreamLayout::default();
        let mut rows = Vec::new();
        let mut text = String::new();
        let p = Palette::new();
        for width in [7, 29, 3] {
            text.clear();
            rows.clear();
            for ch in input.chars() {
                let old = text.len();
                text.push(ch);
                if let Some((at, suffix)) =
                    layout.update_prefixed(&text, width, p, (old != 0).then_some(old), "")
                {
                    rows.truncate(at);
                    rows.extend(suffix);
                }
                assert_eq!(
                    canonical(&rows),
                    canonical(&reference(&text, width, p)),
                    "{text:?}, width {width}"
                );
            }
            assert_eq!(rows.iter().filter(|row| row.layout.decorative).count(), 2);
            let mut copied = String::new();
            let mut first = true;
            for row in rows.iter().filter(|row| !row.layout.decorative) {
                if !first && !row.continued {
                    copied.push('\n');
                }
                copied.push_str(&row.line.to_string());
                first = false;
            }
            assert_eq!(
                copied,
                "prose inline\n\nlet answer = 42;  \n\n    \n// long comment with words for wrapping\n\nafter"
            );
        }
        for width in [7, 31, 3] {
            let p = Palette::new();
            if let Some((at, suffix)) =
                layout.update_prefixed(&text, width, p, Some(text.len()), "")
            {
                rows.truncate(at);
                rows.extend(suffix);
            }
            assert_eq!(canonical(&rows), canonical(&reference(&text, width, p)));
        }
    }

    fn check_chunks(chunks: &[&str], width: usize) {
        let p = Palette::new();
        let mut cache = StreamLayout::default();
        let mut text = String::new();
        let mut rows = Vec::new();
        for chunk in chunks {
            let old = text.len();
            text.push_str(chunk);
            if let Some((at, suffix)) = cache.update_prefixed(&text, width, p, Some(old), "") {
                assert!(at <= rows.len());
                rows.truncate(at);
                rows.extend(suffix);
            }
            assert_eq!(
                canonical(&rows),
                canonical(&reference(&text, width, p)),
                "input {text:?}, width {width}"
            );
        }
    }

    #[test]
    fn common_markdown_every_character_and_split() {
        for source in [
            "\n\nfirst\nsoft break  \nhard break\n\n\nsecond\n\n",
            "# title\n\nplain\n\nnext\n\n## later\n\nlast",
            "- first\n- second\n\n- third\n\nend",
            "1. first\n2. second\n\n3. third\n\nend",
            "- [ ] pending\n- [x] done\n\nend",
            "- parent\n  - child\n  - sibling\n\n  continuation\n\n- next",
            "> first\n> next\n>\n> paragraph\n\nend",
            "~~deleted~~ then **bold** and *emphasis*\n\nplain",
            "| left | right |\n| :--- | ---: |\n| a | long value |\n| longer | b |\n\nend",
            "before\n\n```text\nline\n\nline\n```\n\nafter",
            "before\n\n---\n\nafter",
            "# title\n\nplain\n\n[link][id]\n\n[id]: https://example.com",
            "# title\n\nplain\n\n<div>\nhtml\n</div>\n\nend",
        ] {
            let chunks: Vec<_> = source
                .char_indices()
                .map(|(i, c)| &source[i..i + c.len_utf8()])
                .collect();
            for width in [1, 9, 80] {
                check_chunks(&chunks, width);
                for (at, _) in source.char_indices() {
                    check_chunks(&[&source[..at], &source[at..]], width);
                }
            }
        }
    }

    #[test]
    fn prefixed_fragment_boundaries_and_cleaning() {
        let p = Palette::new();
        for source in [
            "# title\n\nplain\n\nnext\n\n**rich**\n\nend",
            "first\n\nsecond\nsoft  \nhard\n\n",
            "# title\n\nplain\n\n\tindented\n\nend",
            "# title\n\nplain\n\n[link][id]\n\n[id]: https://example.com",
            "# title\n\nplain\r\n\r\nend\u{1b}",
            "```\n```\n\nhello",
            "```\n```\n\n# Heading",
        ] {
            for width in [1, 3, 9, 80] {
                let mut cache = StreamLayout::default();
                let mut text = String::new();
                let mut rows = Vec::new();
                for ch in source.chars() {
                    let old = text.len();
                    text.push(ch);
                    let (at, suffix) = cache
                        .update_prefixed(&text, width, p, Some(old), "↳ ")
                        .unwrap();
                    rows.truncate(at);
                    rows.extend(suffix);
                    assert_eq!(
                        canonical(&rows),
                        canonical(&render(
                            &text,
                            width,
                            width.saturating_sub("↳ ".width()),
                            p,
                            "↳ ",
                            true,
                            None
                        )),
                        "{text:?}, width {width}"
                    );
                }
            }
        }
    }

    fn check_prefixed_chunks(chunks: &[&str], width: usize, prefix: &str) {
        let p = Palette::new();
        let mut cache = StreamLayout::default();
        let mut text = String::new();
        let mut rows = Vec::new();
        for chunk in chunks {
            let old = text.len();
            text.push_str(chunk);
            if let Some((at, suffix)) = cache.update_prefixed(&text, width, p, Some(old), prefix) {
                assert!(at <= rows.len());
                rows.truncate(at);
                rows.extend(suffix);
            }
            let expected = render(
                &text,
                width,
                width.max(1).saturating_sub(prefix.width()),
                p,
                prefix,
                true,
                None,
            );
            assert_eq!(
                canonical(&rows),
                canonical(&expected),
                "input {text:?}, width {width}, prefix {prefix:?}"
            );
        }
    }

    const NARROW_TABLE: &str = "| Name | Description |\n| :--- | ---: |\n| 界 | **long words** and `code` |\n| e\u{301} | abcdefghijklmnopqrstuvwxyz |";

    #[test]
    fn narrow_tables_keep_borders_on_single_rows() {
        let p = Palette::new();
        let quoted = NARROW_TABLE
            .lines()
            .map(|line| format!("> > {line}\n"))
            .collect::<String>();
        for source in [
            NARROW_TABLE.to_owned(),
            quoted,
            format!("```\n```\n\n{NARROW_TABLE}"),
        ] {
            for prefix in ["", "  ", "↳ ", "界 "] {
                for width in [18, 23, 28] {
                    let mut cache = StreamLayout::default();
                    let (_, rows) = cache
                        .update_prefixed(&source, width, p, None, prefix)
                        .unwrap();
                    assert!(rows.len() > 4, "cells should wrap inside the table");
                    let table_start = rows
                        .iter()
                        .position(|row| row.line.to_string().contains('┌'))
                        .unwrap();
                    let table_rows = &rows[table_start..];
                    let separator = table_rows
                        .iter()
                        .find(|row| row.line.to_string().contains('├'))
                        .unwrap()
                        .line
                        .to_string();
                    // Measure actual screen columns, including quote containers;
                    // stripping the spinner would hide first-row misalignment.
                    let columns: Vec<_> = separator
                        .char_indices()
                        .filter(|(_, ch)| {
                            matches!(
                                ch,
                                '│' | '├' | '┼' | '┤' | '┌' | '┬' | '┐' | '└' | '┴' | '┘'
                            )
                        })
                        .map(|(at, _)| separator[..at].width())
                        .collect();
                    let mut separators = 0;
                    for row in table_rows {
                        let line = &row.line;
                        let continued = row.continued;
                        let text = line.to_string();
                        assert!(line.width() <= width, "{text:?}, width {width}");
                        assert!(!continued, "downstream wrapper split a table row: {text:?}");
                        let actual: Vec<_> = text
                            .char_indices()
                            .filter(|(_, ch)| {
                                matches!(
                                    ch,
                                    '│' | '├'
                                        | '┼'
                                        | '┤'
                                        | '┌'
                                        | '┬'
                                        | '┐'
                                        | '└'
                                        | '┴'
                                        | '┘'
                                )
                            })
                            .map(|(at, _)| text[..at].width())
                            .collect();
                        assert_eq!(actual, columns, "misaligned physical borders: {text:?}");
                        if text.contains('├') {
                            separators += 1;
                            assert!(text.ends_with('┤'));
                            assert_eq!(text.matches('┼').count(), 1);
                        } else if text.contains('┌') {
                            assert!(text.ends_with('┐'));
                            assert_eq!(text.matches('┬').count(), 1);
                        } else if text.contains('└') {
                            assert!(text.ends_with('┘'));
                            assert_eq!(text.matches('┴').count(), 1);
                        } else {
                            assert!(text.starts_with('│') && text.ends_with('│'), "{text:?}");
                        }
                    }
                    assert_eq!(separators, 2);
                }
            }
        }
    }

    #[test]
    fn table_resize_invalidates_committed_rows_and_continues_streaming() {
        let p = Palette::new();
        for prefix in ["", "  ", "界 "] {
            let mut cache = StreamLayout::default();
            let mut source = format!("{NARROW_TABLE}\n\nplain tail");
            let (_, mut rows) = cache.update_prefixed(&source, 60, p, None, prefix).unwrap();
            let wide_rows = rows.len();
            for width in [17, 8, 3, 35, 60] {
                // Claim an unchanged append so width, rather than replacement,
                // must invalidate both the table and its committed checkpoint.
                let (at, suffix) = cache
                    .update_prefixed(&source, width, p, Some(source.len()), prefix)
                    .unwrap();
                assert_eq!(at, 0);
                rows.truncate(at);
                rows.extend(suffix);
                assert_eq!(
                    canonical(&rows),
                    canonical(&render(
                        &source,
                        width,
                        width.saturating_sub(prefix.width()),
                        p,
                        prefix,
                        true,
                        None
                    ))
                );
                if width == 17 {
                    assert!(rows.len() > wide_rows);
                }
                assert!(
                    cache
                        .update_prefixed(&source, width, p, Some(source.len()), prefix)
                        .is_none()
                );
                let old = source.len();
                source.push_str(" more");
                let (at, suffix) = cache
                    .update_prefixed(&source, width, p, Some(old), prefix)
                    .unwrap();
                rows.truncate(at);
                rows.extend(suffix);
                assert_eq!(
                    canonical(&rows),
                    canonical(&render(
                        &source,
                        width,
                        width.saturating_sub(prefix.width()),
                        p,
                        prefix,
                        true,
                        None
                    ))
                );
            }
        }
    }

    #[test]
    fn tables_match_full_render_at_every_chunk_boundary() {
        let quoted = NARROW_TABLE
            .lines()
            .map(|line| format!("> {line}\n"))
            .collect::<String>();
        let listed = NARROW_TABLE
            .lines()
            .map(|line| format!("  {line}\n"))
            .collect::<String>();
        for source in [
            format!("{NARROW_TABLE}\n\nplain tail"),
            format!("```\n```\n\n{NARROW_TABLE}\n\nend"),
            format!("# Heading\n\nfirst paragraph\n\n{NARROW_TABLE}\n\n{NARROW_TABLE}\n\nend"),
            format!("{quoted}\nend"),
            format!("- parent\n\n{listed}\nend"),
        ] {
            let chunks: Vec<_> = source
                .char_indices()
                .map(|(i, c)| &source[i..i + c.len_utf8()])
                .collect();
            for width in [3, 9, 17, 32] {
                for prefix in ["", "  ", "界 "] {
                    check_prefixed_chunks(&chunks, width, prefix);
                    for (at, _) in source.char_indices() {
                        check_prefixed_chunks(&[&source[..at], &source[at..]], width, prefix);
                    }
                }
            }
        }
    }

    #[test]
    fn word_wrapped_prose_every_character() {
        for text in [
            "A quick brown fox jumps over the lazy dog, with   repeated spaces.",
            "Alpha **boldwords** then *emphasized words* and `inlinecode` finish.",
            "A prefix supercalifragilisticexpialidocious then ordinary words.",
            "Hello abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz end.",
            "Wide 中文文字没有空格 and e\u{301}e\u{301}e\u{301} with emoji 👩\u{200d}💻❤️ end.",
            "# Heading\n\nStyled **longboldtokenwithoutseparators** prose.\n\nA plain tail with lengthywords.",
            "Longtokenwith**boldmiddle**andplainending after short words.",
        ] {
            let chunks: Vec<_> = text
                .char_indices()
                .map(|(i, c)| &text[i..i + c.len_utf8()])
                .collect();
            for width in 1..=24 {
                check_chunks(&chunks, width);
            }
        }
    }

    #[test]
    fn reset_resize_and_unchanged() {
        let mut cache = StreamLayout::default();
        let p = Palette::new();
        let text = "ordinary prose that wraps";
        let (_, rows) = cache.update_prefixed(text, 8, p, None, "").unwrap();
        assert_eq!(canonical(&rows), canonical(&reference(text, 8, p)));
        assert!(
            cache
                .update_prefixed(text, 8, p, Some(text.len()), "")
                .is_none()
        );
        for (text, width) in [(text, 3), ("replacement", 3), ("`code`", 3)] {
            let (at, rows) = cache.update_prefixed(text, width, p, None, "").unwrap();
            assert_eq!(at, 0);
            assert_eq!(canonical(&rows), canonical(&reference(text, width, p)));
        }
    }

    #[test]
    fn mixed_markdown_chunk_boundaries_match_full_render() {
        let alphabet = [
            "a", " ", "\n", "\n\n", "-", "+", "1", ".", "*", "_", "~", "#", "|", "`", ":", "  ",
            "中",
        ];
        let mut seed = 71u64;
        for width in [1, 8, 80] {
            for _ in 0..200 {
                let mut chunks = Vec::new();
                for _ in 0..50 {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    chunks.push(alphabet[(seed >> 32) as usize % alphabet.len()]);
                }
                check_chunks(&chunks, width);
            }
        }
    }
}
