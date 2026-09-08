//! Append-only layout for live prose. Never owns a copy of the input or old rows.
//!
//! Plain prose keeps only source offsets and two unstable wrap rows. Rich Markdown
//! commits complete top-level blocks separated by blank lines, retaining the final
//! block for reparsing. Reference syntax, HTML, and terminal-control cleaning
//! conservatively disable block commits because they can change earlier output.
//! A single giant rich block or open fence remains an explicit full-suffix fallback.
//! Source checkpoints are width-independent; a resize currently reparses to rebuild
//! wrapped rows (resizes are not the routine streaming path).

#[cfg(test)]
use super::markdown;
use super::{Palette, wrap_words};
use ratatui::{style::Color, text::Line};

pub(super) type Suffix = (usize, Vec<(Line<'static>, bool)>);

#[derive(Default)]
pub(super) struct StreamLayout {
    initialized: bool,
    len: usize,
    width: usize,
    accent: Option<Color>,
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
    /// Width and Markdown palette changes automatically invalidate the layout.
    pub(super) fn update_prefixed(
        &mut self,
        text: &str,
        width: usize,
        p: Palette,
        append_from: Option<usize>,
        prefix: &str,
    ) -> Option<Suffix> {
        let width = width.max(1);
        let append = self.initialized
            && append_from == Some(self.len)
            && self.len <= text.len()
            && text.is_char_boundary(self.len)
            && self.width == width
            && self.accent == Some(p.accent)
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
        self.accent = Some(p.accent);
        if !prefix.is_empty() && width < prefix.len() {
            self.plain = None;
        }
        if let Some(plain) = &mut self.plain {
            if let Some(suffix) = plain.append(text, from, width, prefix) {
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
                p,
                if truncate == 0 { prefix } else { "" },
                false,
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
                p,
                if self.stable_rows == 0 { prefix } else { "" },
                self.stable_rows == 0,
            );
            append_block(&mut output, block, self.stable_rows > 0);
        }
        Some((truncate, output))
    }
}

// Fragments trim their edges. Restore the separator only when another visible
// block follows, never as a trailing blank in the committed prefix.
fn append_block(
    output: &mut Vec<(Line<'static>, bool)>,
    block: Vec<(Line<'static>, bool)>,
    separated: bool,
) {
    if !block.is_empty() {
        if separated {
            output.push((Line::default(), false));
        }
        output.extend(block);
    }
}

fn render(
    text: &str,
    width: usize,
    p: Palette,
    prefix: &str,
    placeholder: bool,
) -> Vec<(Line<'static>, bool)> {
    let cleaned = super::model::clean(text);
    let mut lines = super::markdown::render(&cleaned, p, placeholder);
    if let Some(first) = lines.first_mut()
        && !prefix.is_empty()
    {
        first
            .spans
            .insert(0, ratatui::text::Span::raw(prefix.to_owned()));
    }
    lines
        .into_iter()
        .flat_map(|line| {
            wrap_words(line, width)
                .into_iter()
                .enumerate()
                .map(|(i, line)| (line, i > 0))
        })
        .collect()
}

// Only commit complete top-level blocks with an explicit blank-line separator.
// The last block is retained because closing syntax can still reinterpret it.
fn stable_boundary(text: &str) -> usize {
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
    fn append(&mut self, text: &str, from: usize, width: usize, prefix: &str) -> Option<Suffix> {
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
            self.emit(text, width, &mut output, prefix)?;
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
            output.push((Line::from(prefix.to_owned()), false));
        }
        Some((truncate, output))
    }

    fn emit(
        &mut self,
        text: &str,
        width: usize,
        output: &mut Vec<(Line<'static>, bool)>,
        prefix: &str,
    ) -> Option<()> {
        if self.visible_end <= self.tail_start {
            return Some(());
        }
        if self.pending_blank {
            output.push((Line::default(), false));
            self.rows += 1;
            self.tail_row += 1;
            self.pending_blank = false;
        }
        let start_row = self.tail_row;
        let mut byte = self.tail_start;
        let mut checkpoints = Vec::new();
        let prefix_len = if self.tail_row == 0 { prefix.len() } else { 0 };
        let mut source = String::new();
        if self.tail_row == 0 {
            source.push_str(prefix);
        }
        source.push_str(&text[byte..self.visible_end]);
        let lines = wrap_words(Line::from(source), width);
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
            output.push((line, continued));
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

    fn canonical(rows: &[(Line<'static>, bool)]) -> Vec<(Vec<(String, Style)>, bool)> {
        rows.iter()
            .map(|(line, continued)| {
                let mut spans: Vec<(String, Style)> = Vec::new();
                for span in &line.spans {
                    if span.content.is_empty() {
                        continue;
                    }
                    if let Some((text, style)) = spans.last_mut()
                        && *style == span.style
                    {
                        text.push_str(&span.content);
                        continue;
                    }
                    spans.push((span.content.to_string(), span.style));
                }
                (spans, *continued)
            })
            .collect()
    }

    fn reference(text: &str, width: usize, p: Palette) -> Vec<(Line<'static>, bool)> {
        markdown(text, p)
            .into_iter()
            .flat_map(|line| {
                wrap_words(line, width.max(1))
                    .into_iter()
                    .enumerate()
                    .map(|(i, line)| (line, i > 0))
            })
            .collect()
    }

    fn check_chunks(chunks: &[&str], width: usize) {
        let p = Palette::new(false);
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
        let p = Palette::new(false);
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
                        canonical(&render(&text, width, p, "↳ ", true)),
                        "{text:?}, width {width}"
                    );
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
    fn reset_resize_palette_and_unchanged() {
        let mut cache = StreamLayout::default();
        let p = Palette::new(false);
        let text = "ordinary prose that wraps";
        let (_, rows) = cache.update_prefixed(text, 8, p, None, "").unwrap();
        assert_eq!(canonical(&rows), canonical(&reference(text, 8, p)));
        assert!(
            cache
                .update_prefixed(text, 8, p, Some(text.len()), "")
                .is_none()
        );
        for (text, width, p) in [
            (text, 3, p),
            ("replacement", 3, p),
            ("`code`", 3, Palette::new(true)),
        ] {
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
