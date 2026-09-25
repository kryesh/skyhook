//! Lossless grapheme and word wrapping for terminal lines.

use super::*;

/// Printable ASCII: every byte is its own grapheme, one cell wide.
fn narrow_ascii(text: &str) -> bool {
    text.bytes().all(|byte| matches!(byte, b' '..=b'~'))
}

/// A string's graphemes as `(byte offset, grapheme, cell width)`. Printable ASCII
/// skips grapheme segmentation, which dominates layout of ordinary text.
pub(super) fn cells(text: &str) -> impl Iterator<Item = (usize, &str, usize)> {
    let ascii = narrow_ascii(text);
    let narrow = ascii.then(|| (0..text.len()).map(move |byte| (byte, &text[byte..=byte], 1)));
    let segmented = (!ascii).then(|| {
        text.grapheme_indices(true)
            .map(|(byte, grapheme)| (byte, grapheme, grapheme.width()))
    });
    narrow
        .into_iter()
        .flatten()
        .chain(segmented.into_iter().flatten())
}

/// The cells a string paints, grapheme by grapheme (which can differ from its
/// width measured as a whole).
pub(super) fn cells_width(text: &str) -> usize {
    if narrow_ascii(text) {
        text.len()
    } else {
        cells(text).map(|(_, _, width)| width).sum()
    }
}

pub fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    text.split('\n')
        .flat_map(|line| wrap_line(Line::from(line.to_owned()), width.max(1)))
        .map(|line| line.to_string())
        .collect()
}
pub(super) fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    wrap_line_widths(line, width, width)
}

pub(super) fn wrap_line_widths(
    line: Line<'static>,
    width: usize,
    continuation_width: usize,
) -> Vec<Line<'static>> {
    let mut width = width.max(1);
    let template = Line {
        style: line.style,
        alignment: line.alignment,
        ..Line::default()
    };
    let mut result = Vec::new();
    let mut spans = Vec::new();
    let mut used = 0;
    for span in line.spans {
        let mut start = 0;
        for (byte, _, size) in cells(&span.content) {
            if used + size > width && used > 0 {
                if byte > start {
                    let chunk = span.content[start..byte].to_owned();
                    spans.push(Span::styled(chunk, span.style));
                }
                result.push(Line {
                    spans: std::mem::take(&mut spans),
                    ..template.clone()
                });
                start = byte;
                width = continuation_width.max(1);
                used = 0;
            }
            used += size;
        }
        if start == 0 && !span.content.is_empty() {
            spans.push(span);
        } else if start < span.content.len() {
            spans.push(Span::styled(span.content[start..].to_owned(), span.style));
        }
    }
    result.push(Line { spans, ..template });
    result
}
/// Wrap prose at whitespace without losing source bytes or inline styles.
/// Only an unbroken token wider than the viewport may be split. Keeping the
/// separating whitespace also preserves copy/selection and streaming offsets.
pub(super) fn wrap_words(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    if line.width() <= width {
        return vec![line];
    }
    let template = Line {
        style: line.style,
        alignment: line.alignment,
        ..Line::default()
    };
    let mut tokens: Vec<(bool, Vec<Span<'static>>)> = Vec::new();
    for span in line.spans {
        let mut start = 0;
        let mut whitespace = None;
        for (offset, ch) in span.content.char_indices() {
            let next = ch.is_whitespace();
            if whitespace.is_some_and(|previous| previous != next) {
                let previous = whitespace.unwrap();
                if tokens.last().is_none_or(|token| token.0 != previous) {
                    tokens.push((previous, Vec::new()));
                }
                tokens.last_mut().unwrap().1.push(Span::styled(
                    span.content[start..offset].to_owned(),
                    span.style,
                ));
                start = offset;
            }
            whitespace = Some(next);
        }
        if let Some(whitespace) = whitespace {
            if tokens.last().is_none_or(|token| token.0 != whitespace) {
                tokens.push((whitespace, Vec::new()));
            }
            tokens
                .last_mut()
                .unwrap()
                .1
                .push(Span::styled(span.content[start..].to_owned(), span.style));
        }
    }
    let mut result = Vec::new();
    let mut row = template.clone();
    let mut used = 0;
    for (whitespace, spans) in tokens {
        let token_width = spans.iter().map(Span::width).sum::<usize>();
        if whitespace {
            // Keep separators on the preceding row, even when they lie beyond
            // its visible edge. They are clipped by the viewport, not discarded
            // from selection text or rendered as a blank-only continuation row.
            used += token_width;
            row.spans.extend(spans);
            continue;
        }
        if token_width > 0
            && used > 0
            && (used >= width || token_width <= width && used + token_width > width)
        {
            result.push(std::mem::replace(&mut row, template.clone()));
            used = 0;
        }
        if used + token_width <= width {
            used += token_width;
            row.spans.extend(spans);
        } else {
            // Whitespace and overlong tokens retain every grapheme; the latter
            // cannot fit on any row even if moved to its own line.
            row.spans.extend(spans);
            let mut wrapped = wrap_line(row, width);
            row = wrapped.pop().unwrap_or_default();
            used = row.width();
            result.extend(wrapped);
        }
    }
    result.push(row);
    result
}

/// Truncate metadata at grapheme boundaries.
pub(super) fn clipped_header(value: &str, width: u16) -> String {
    let budget = width as usize;
    if budget == 0 {
        return String::new();
    }
    if value.width() <= budget {
        return value.to_owned();
    }
    let mut used = 0;
    let end = cells(value)
        .find(|&(_, _, width)| {
            used += width;
            used > budget - 1
        })
        .map_or(value.len(), |(byte, _, _)| byte);
    format!("{}…", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_match_grapheme_segmentation_with_and_without_the_ascii_fast_path() {
        for text in [
            "",
            "plain ascii ~!@#$%^&*()_+{}|:<>?",
            "tab\tnewline\ncarriage\r\nnul\0",
            "pré e\u{301}lan",
            "👩‍💻 世界 ﻻ",
        ] {
            let expected: Vec<_> = text
                .grapheme_indices(true)
                .map(|(byte, grapheme)| (byte, grapheme, grapheme.width()))
                .collect();
            assert_eq!(cells(text).collect::<Vec<_>>(), expected, "{text:?}");
            let width: usize = expected.iter().map(|(_, _, width)| width).sum();
            assert_eq!(cells_width(text), width, "{text:?}");
        }
    }

    #[test]
    fn prose_word_wrap_preserves_graphemes_styles_and_cross_span_words() {
        let line = Line::from(vec![
            Span::styled("pré", Style::default().fg(Color::Red)),
            Span::styled(
                "fix e\u{301}lan 👩‍💻 世界  fin  ",
                Style::default().fg(Color::Blue),
            ),
        ]);
        let graphemes = |lines: &[Line<'_>]| {
            let spans = lines.iter().flat_map(|line| &line.spans);
            let graphemes = spans.flat_map(|span| {
                span.content
                    .graphemes(true)
                    .map(|g| (g.to_owned(), span.style))
                    .collect::<Vec<_>>()
            });
            graphemes.collect::<Vec<_>>()
        };
        for width in [0, 1, 2, 3, 5, 7, 12, 40] {
            let parts = wrap_words(line.clone(), width);
            assert_eq!(graphemes(&parts), graphemes(std::slice::from_ref(&line)));
        }
        let parts = wrap_words(line, 7);
        let parts = parts.iter().map(|part| {
            part.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        });
        let words: Vec<_> = parts
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        assert_eq!(words, ["préfix", "e\u{301}lan", "👩‍💻", "世界", "fin"]);
    }
}
