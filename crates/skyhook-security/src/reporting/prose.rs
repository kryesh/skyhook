//! Authored text set into a generated page, read as CommonMark parses it so it never
//! reshapes the page around it: its headings sit below the page's, raw HTML shows as
//! written, a code block it leaves open is closed before the page goes on, text set
//! within a line pairs no delimiter with the line around it, and a link's text holds no
//! link of its own.

use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, LinkType, Options, Parser, Tag, TagEnd};

fn events(text: &str) -> Vec<(Event<'_>, Range<usize>)> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    Parser::new_ext(text, options).into_offset_iter().collect()
}

/// Whether the character at byte `at` is backslash-escaped.
fn escaped(text: &str, at: usize) -> bool {
    text[..at].bytes().rev().take_while(|b| *b == b'\\').count() % 2 == 1
}

/// Where authored text is set in a generated page.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Setting {
    /// As blocks of its own.
    Blocks,
    /// Within a line.
    Line,
    /// Within a line, as a link's text.
    Label,
}

fn autolink(event: &Event) -> bool {
    matches!(
        event,
        Event::Start(Tag::Link {
            link_type: LinkType::Autolink | LinkType::Email,
            ..
        })
    )
}

/// The bytes of `text` from `from` that read as markup the text around them can reshape:
/// every ASCII punctuation character of raw HTML, so it shows as written; within a line,
/// each backtick outside code and each emphasis delimiter forming no emphasis of its own, so
/// it pairs with nothing around it; and as a link's text, each bracket outside code and each
/// autolink's opening, so it holds no link of its own.
fn exposed(text: &str, from: usize, setting: Setting) -> Vec<usize> {
    let events = events(text);
    let bytes = text.as_bytes();
    let mut marks: Vec<usize> = (events.iter())
        .filter(|(event, _)| matches!(event, Event::Html(_) | Event::InlineHtml(_)))
        .flat_map(|(_, range)| range.clone())
        .filter(|at| bytes[*at].is_ascii_punctuation())
        .collect();
    if setting != Setting::Blocks {
        // Code spans and autolinks read their contents verbatim, escapes included. They never
        // nest or overlap, so in document order their ranges are ordered by start.
        let verbatim: Vec<&Range<usize>> = (events.iter())
            .filter(|(event, _)| matches!(event, Event::Code(_)) || autolink(event))
            .map(|(_, range)| range)
            .collect();
        let delimiters = (events.iter())
            .filter(|(event, _)| matches!(event, Event::Text(_)))
            .flat_map(|(_, range)| range.clone())
            .filter(|at| matches!(bytes[*at], b'*' | b'_' | b'~'));
        let shielded = |at: &usize| {
            let after = verbatim.partition_point(|range| range.start <= *at);
            after > 0 && verbatim[after - 1].contains(at)
        };
        let ticks = text.match_indices('`').map(|(at, _)| at);
        let brackets = (text.match_indices(['[', ']']).map(|(at, _)| at))
            .filter(|_| setting == Setting::Label);
        marks.extend(
            (ticks.chain(delimiters).chain(brackets))
                .filter(|at| !escaped(text, *at) && !shielded(at)),
        );
        if setting == Setting::Label {
            let opened = events.iter().filter(|(event, _)| autolink(event));
            marks.extend(opened.map(|(_, range)| range.start));
        }
    }
    marks.retain(|at| *at >= from);
    marks.sort_unstable();
    marks.dedup();
    marks
}

/// `text` with each of `edits`, ranges in order that do not overlap, replaced in one pass.
fn spliced<'a>(text: &str, edits: impl IntoIterator<Item = (Range<usize>, &'a str)>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for (range, replacement) in edits {
        out.push_str(&text[copied..range.start]);
        out.push_str(replacement);
        copied = range.end;
    }
    out.push_str(&text[copied..]);
    out
}

/// `text` with every byte [`exposed`] finds backslash-escaped, parsed again to confirm none
/// is left: an escape can leave other delimiters unpaired.
fn escape(mut text: String, from: usize, setting: Setting) -> String {
    loop {
        let marks = exposed(&text, from, setting);
        if marks.is_empty() {
            return text;
        }
        text = spliced(&text, marks.into_iter().map(|at| (at..at, "\\")));
    }
}

/// `text` with every heading rewritten as an ATX heading `level` deeper, its lines joined.
fn demote(text: &str, level: usize) -> String {
    let events = events(text);
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let mut index = 0;
    while index < events.len() {
        let (Event::Start(Tag::Heading { level: own, .. }), range) = &events[index] else {
            index += 1;
            continue;
        };
        let end = index
            + (events[index..].iter())
                .position(|(event, _)| matches!(event, Event::End(TagEnd::Heading(_))))
                .expect("a heading ends");
        let inner = &events[index + 1..end];
        let marker = "#".repeat((*own as usize + level).min(6));
        let close = range.start + text[range.clone()].trim_end().len();
        match (
            inner.first(),
            inner.iter().map(|(_, inner)| inner.end).max(),
        ) {
            (Some((_, first)), Some(last)) => {
                edits.push((range.start..first.start, format!("{marker} ")));
                for (at, (event, line)) in inner.iter().enumerate() {
                    if matches!(event, Event::SoftBreak | Event::HardBreak) {
                        let next = (inner[at + 1..].iter())
                            .map(|(_, next)| next.start)
                            .find(|start| *start >= line.end)
                            .unwrap_or(line.end);
                        edits.push((line.start..next, " ".to_owned()));
                    }
                }
                edits.push((last..close, String::new()));
            }
            _ => edits.push((range.start..close, marker)),
        }
        index = end + 1;
    }
    spliced(
        text,
        (edits.iter()).map(|(range, replacement)| (range.clone(), replacement.as_str())),
    )
}

/// `text` with a fenced code block it leaves open closed, so nothing after it is code.
fn close(mut text: String) -> String {
    let probe = format!("{text}\n\nx");
    let after = probe.len() - 1;
    let open = (events(&probe).into_iter()).find_map(|(event, range)| match event {
        Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(_))) if range.contains(&after) => {
            Some(range.start)
        }
        _ => None,
    });
    if let Some(start) = open {
        let fence = probe[start..].trim_start_matches(' ');
        let mark = fence.chars().next();
        text.push('\n');
        text.extend(fence.chars().take_while(|c| Some(*c) == mark));
    }
    text
}

/// Authored text as blocks below a heading of `level`, ending in one line break.
pub(super) fn block(text: &str, level: usize) -> String {
    let text = text.replace("\r\n", "\n");
    if text.trim_end().is_empty() {
        return String::new();
    }
    // Only line breaks go: the last line may be code, whose trailing whitespace is content.
    let text = text.trim_end_matches('\n');
    let mut text = close(demote(&escape(text.to_owned(), 0, Setting::Blocks), level));
    text.push('\n');
    text
}

/// Authored text set within a line of generated text: its line breaks become spaces, a
/// backtick or emphasis delimiter that pairs with nothing of its own is escaped so it pairs
/// with nothing else in the line either, and a trailing backslash is escaped so it escapes
/// nothing generated after it.
pub(super) fn line(text: &str) -> String {
    inline(text, Setting::Line)
}

/// Authored text set as a link's text within a line: as [`line`], with every bracket and
/// autolink escaped so it holds no link of its own and closes nothing early.
pub(super) fn label(text: &str) -> String {
    inline(text, Setting::Label)
}

fn inline(text: &str, setting: Setting) -> String {
    // A leading word keeps the text inline, as it is within the page's line.
    const LEAD: &str = "x ";
    let flat: Vec<&str> = (text.split(['\r', '\n']))
        .filter(|line| !line.is_empty())
        .collect();
    let mut text = escape(format!("{LEAD}{}", flat.join(" ")), LEAD.len(), setting);
    if escaped(&text, text.len()) {
        text.push('\\');
    }
    text.split_off(LEAD.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The headings, raw HTML and text of `markdown` as CommonMark parses it, each
    /// emphasis braced.
    fn parsed(markdown: &str) -> (Vec<(usize, String)>, usize, String) {
        let (mut headings, mut html, mut text) = (Vec::new(), 0, String::new());
        let mut heading: Option<(usize, String)> = None;
        for (event, _) in events(markdown) {
            let value = match event {
                Event::Start(Tag::Heading { level, .. }) => {
                    heading = Some((level as usize, String::new()));
                    continue;
                }
                Event::End(TagEnd::Heading(_)) => {
                    headings.extend(heading.take());
                    continue;
                }
                Event::Html(_) | Event::InlineHtml(_) => {
                    html += 1;
                    continue;
                }
                Event::Start(Tag::Emphasis | Tag::Strong | Tag::Strikethrough) => "{".into(),
                Event::End(TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough) => "}".into(),
                Event::Text(value) | Event::Code(value) => value,
                _ => continue,
            };
            match &mut heading {
                Some((_, title)) => title.push_str(&value),
                None => text.push_str(&value),
            }
        }
        (headings, html, text)
    }

    /// Authored blocks keep their meaning but not their power: headings move below the
    /// page's, raw HTML outside real code shows as written, code stays verbatim to its last
    /// line, and what the page sets after them keeps its own structure.
    #[test]
    fn authored_blocks_never_reshape_the_page() {
        for (authored, headings, text) in [
            (
                "# Cause\r\n<script>x</script> and `<b>`\r\n",
                vec![(3, "Cause".to_owned())],
                "<script>x</script> and `<b>`",
            ),
            (
                "Setext\nheading\n===",
                vec![(3, "Setext heading".into())],
                "",
            ),
            ("> # Quoted", vec![(3, "Quoted".into())], ""),
            // Indented code and fenced code are verbatim, an unfinished fence closed.
            ("text\n\n    # kept <i>  \n\n", vec![], "text# kept <i>  \n"),
            ("```\n# kept <i> \t\n", vec![], "# kept <i> \t\n"),
            ("~~~~\n~~~\n<i>", vec![], "~~~\n<i>\n"),
            // A backtick that forms no code span shields nothing.
            ("`open <b>", vec![], "`open <b>"),
            ("\\`<b>`", vec![], "`<b>`"),
            // Raw HTML shows exactly as written, a backtick it holds pairing with nothing.
            ("<b title='`'> `<i>`", vec![], "<b title='`'> <i>"),
        ] {
            let page = format!("## Section\n\n{}\n## After\n\nfact\n", block(authored, 2));
            let (found, html, rendered) = parsed(&page);
            let mut expected = vec![(2, "Section".to_owned())];
            expected.extend(headings);
            expected.push((2, "After".to_owned()));
            assert_eq!(found, expected, "{authored:?}: {page}");
            assert_eq!(html, 0, "{authored:?}: {page}");
            assert_eq!(rendered, format!("{text}fact"), "{authored:?}: {page}");
        }
    }

    /// Text within a line pairs no backtick or emphasis delimiter with the page's code or
    /// with other authored text in its paragraph, keeps the code and emphasis it closes
    /// itself, and its line breaks never start a block.
    #[test]
    fn authored_lines_stay_within_their_line() {
        for (authored, text) in [
            ("a `b` <c>", "a b <c>"),
            ("unpaired `tick", "unpaired `tick"),
            ("two\n```\nlines", "two ``` lines"),
            ("a*b and x_ _y, a~~b", "a*b and x_ _y, a~~b"),
            ("**own** _own_ ~~own~~ ***a*", "{own} {own} {own} **{a}"),
            ("<http://a_b*c>", "http://a_b*c"),
            ("<b title='`*'> `<i>`", "<b title='`*'> <i>"),
        ] {
            let page = format!(
                "Root flaw 1: {}. Candidate `2`: {} (x).\n",
                line(authored),
                line(authored)
            );
            let (headings, html, rendered) = parsed(&page);
            assert!(headings.is_empty(), "{page}");
            assert_eq!(
                (html, rendered),
                (0, format!("Root flaw 1: {text}. Candidate 2: {text} (x).")),
                "{page}"
            );
        }
    }

    /// A link's text holds no link of its own and closes nothing early: each link it
    /// sets stays one link around the authored text, read as written.
    #[test]
    fn authored_labels_hold_no_link() {
        for (authored, text) in [
            ("[x](y)", "[x](y)"),
            ("a ] b", "a ] b"),
            ("[a [b", "[a [b"),
            ("![i](j)", "![i](j)"),
            ("<http://a_b> and `]`", "<http://a_b> and ]"),
            ("a \\", "a \\"),
            ("`a \\", "`a \\"),
        ] {
            let page = format!("[{}](u) and [{}](v)\n", label(authored), label(authored));
            let (mut depth, mut links, mut rendered) = (0, Vec::new(), String::new());
            for (event, _) in events(&page) {
                match event {
                    Event::Start(Tag::Link { dest_url, .. }) => {
                        depth += 1;
                        links.push((depth, dest_url.to_string()));
                    }
                    Event::End(TagEnd::Link) => depth -= 1,
                    Event::Text(value) | Event::Code(value) => rendered.push_str(&value),
                    _ => {}
                }
            }
            let expected = [(1, "u".to_owned()), (1, "v".to_owned())];
            assert_eq!(links, expected, "{page}");
            assert_eq!(rendered, format!("{text} and {text}"), "{page}");
        }
    }
}
