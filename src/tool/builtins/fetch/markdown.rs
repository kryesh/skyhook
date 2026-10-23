//! HTML to markdown over a parsed document, walked without recursion and written
//! through one writer that charges the extraction budget before every write.
//!
//! dom_query's own `md()` does not fit: it recurses as deep as the document nests,
//! builds without a budget, leaves links relative to the page, and escapes too
//! little to keep page text from reading as markdown syntax.

mod writer;

use dom_query::NodeRef;
use html5ever::{LocalName, local_name};

use super::budget::{Budget, Spent};
use super::text::{Scope, Unreadable};
use writer::{Emphasis, HeadingLevel, Mark, Marker, Prefix, Writer};

/// Markdown escaped only where text would otherwise read as markdown syntax, with
/// every link resolved against `base` as a browser would, so the model can fetch it.
pub(super) fn markdown(
    root: NodeRef,
    scope: Scope,
    base: &reqwest::Url,
    budget: &Budget,
) -> Result<String, Unreadable> {
    let mut writer = Writer::new(budget, base);
    walk(root, scope, budget, &mut writer)?;
    let markdown = writer.finish()?;
    let markdown = markdown.trim_end();
    if markdown.is_empty() {
        return Err(Unreadable::NoText);
    }
    Ok(markdown.to_owned())
}

fn walk<'a>(
    root: NodeRef<'a>,
    scope: Scope,
    budget: &Budget,
    writer: &mut Writer<'a>,
) -> Result<(), Spent> {
    let mut lists = Vec::new();
    let mut steps = vec![Step::Enter(root)];
    while let Some(step) = steps.pop() {
        let node = match step {
            Step::Exit(close) => {
                close.apply(writer, &mut lists)?;
                continue;
            }
            Step::Row { cells, delimiter } => {
                writer.line()?;
                writer.syntax("|")?;
                steps.push(Step::Exit(Close::Row { delimiter }));
                steps.extend(cells.into_iter().rev().map(Step::Cell));
                continue;
            }
            Step::Cell(cell) => {
                if writer.cell_start(cell)? {
                    steps.push(Step::Exit(Close::Cell));
                    let content = cell.into_iter().flat_map(|cell| cell.children_it(true));
                    steps.extend(content.map(Step::Enter));
                }
                continue;
            }
            Step::Enter(node) => node,
        };
        if node.is_text() {
            writer.text(&node.text())?;
            continue;
        }
        let Some(tag) = name(&node) else {
            continue;
        };
        if scope.skips(&node, &tag) {
            continue;
        }
        let kind = Kind::of(&tag);
        // A table with a header row, outside code and other tables, is written as
        // GFM rows, then closes as a block; its captions come first.
        if matches!(kind, Kind::Table)
            && writer.code().is_none()
            && !writer.in_cell()
            && let Some(rows) = grid(node, scope, budget)?
        {
            writer.block()?;
            steps.push(Step::Exit(Close::Block));
            let width = rows.first().map_or(0, Vec::len);
            let rows = rows.into_iter().enumerate().map(|(at, cells)| Step::Row {
                cells,
                delimiter: (at == 0).then_some(width),
            });
            steps.extend(rows.rev());
            let captions = (node.children_it(true))
                .filter(|child| name(child) == Some(local_name!("caption")));
            steps.extend(captions.map(Step::Enter));
            continue;
        }
        let close = match writer.code() {
            // Code is literal: markup inside it adds only its text and line breaks.
            Some(code) => match kind {
                Kind::Break => {
                    code.push('\n');
                    None
                }
                kind if kind.breaks_code() => {
                    end_line(code);
                    Some(Close::CodeLine)
                }
                _ => None,
            },
            None => enter(node, kind, writer, &mut lists)?,
        };
        if let Some(close) = close {
            steps.push(Step::Exit(close));
        }
        steps.extend(node.children_it(true).map(Step::Enter));
    }
    Ok(())
}

/// Writes what an element opens with; what leaving it writes.
fn enter<'a>(
    node: NodeRef<'a>,
    kind: Kind,
    writer: &mut Writer<'a>,
    lists: &mut Vec<usize>,
) -> Result<Option<Close>, Spent> {
    let close = match kind {
        Kind::Break => {
            writer.hard_break()?;
            return Ok(None);
        }
        Kind::Rule => {
            writer.block()?;
            writer.syntax("* * *")?;
            writer.block()?;
            return Ok(None);
        }
        Kind::Image => {
            writer.image(node)?;
            return Ok(None);
        }
        Kind::Pre if !writer.in_cell() => {
            writer.begin_code();
            Close::CodeBlock {
                language: language(node),
            }
        }
        Kind::Pre | Kind::Code => {
            writer.begin_code();
            Close::CodeSpan
        }
        Kind::Heading(level) => {
            writer.block()?;
            writer.heading(level);
            Close::Heading
        }
        Kind::Emphasis(emphasis) => {
            writer.open(Mark::Emphasis(emphasis));
            Close::Span
        }
        Kind::Link => match node.attr("href") {
            Some(href) if !inline_data(&href) => {
                writer.open(Mark::Link(node));
                Close::Span
            }
            _ => return Ok(None),
        },
        Kind::Quote => {
            writer.block()?;
            writer.push_prefix(Prefix::Quote);
            Close::Quote
        }
        Kind::List => {
            let nested = parent_is(&node, local_name!("li"));
            if nested {
                writer.line()?;
            } else {
                writer.block()?;
            }
            let start = node
                .attr("start")
                .and_then(|start| start.trim().parse().ok());
            lists.push(start.unwrap_or(1));
            if nested {
                Close::NestedList
            } else {
                Close::List
            }
        }
        Kind::Item => {
            let marker = match lists
                .last_mut()
                .filter(|_| parent_is(&node, local_name!("ol")))
            {
                Some(number) => {
                    let marker = Marker::Number(*number);
                    *number = number.saturating_add(1);
                    marker
                }
                None => Marker::Bullet,
            };
            writer.line()?;
            // A cell keeps its content on one line, so the marker goes inline.
            if writer.in_cell() {
                writer.open(Mark::Item(marker));
                Close::Span
            } else {
                writer.push_prefix(Prefix::Item {
                    marker: Some(marker),
                    width: marker.width(),
                });
                Close::Item
            }
        }
        // A table read as its text.
        Kind::Table | Kind::Block => {
            writer.block()?;
            Close::Block
        }
        Kind::Inline => return Ok(None),
    };
    Ok(Some(close))
}

/// How the converter reads an element, by its tag.
#[derive(Clone, Copy)]
enum Kind {
    Break,
    Rule,
    Image,
    Pre,
    Code,
    Heading(HeadingLevel),
    Emphasis(Emphasis),
    Link,
    Quote,
    List,
    Item,
    Table,
    /// Starts and ends a paragraph of its own.
    Block,
    /// Adds only its content.
    Inline,
}

impl Kind {
    fn of(tag: &LocalName) -> Self {
        match *tag {
            local_name!("br") => Self::Break,
            local_name!("hr") => Self::Rule,
            local_name!("img") => Self::Image,
            local_name!("pre") => Self::Pre,
            local_name!("code") => Self::Code,
            local_name!("h1") => Self::Heading(HeadingLevel::H1),
            local_name!("h2") => Self::Heading(HeadingLevel::H2),
            local_name!("h3") => Self::Heading(HeadingLevel::H3),
            local_name!("h4") => Self::Heading(HeadingLevel::H4),
            local_name!("h5") => Self::Heading(HeadingLevel::H5),
            local_name!("h6") => Self::Heading(HeadingLevel::H6),
            local_name!("em") | local_name!("i") => Self::Emphasis(Emphasis::Em),
            local_name!("strong") | local_name!("b") => Self::Emphasis(Emphasis::Strong),
            local_name!("a") => Self::Link,
            local_name!("blockquote") => Self::Quote,
            local_name!("ul") | local_name!("ol") => Self::List,
            local_name!("li") => Self::Item,
            local_name!("table") => Self::Table,
            local_name!("address")
            | local_name!("article")
            | local_name!("aside")
            | local_name!("body")
            | local_name!("caption")
            | local_name!("center")
            | local_name!("dd")
            | local_name!("details")
            | local_name!("dialog")
            | local_name!("dir")
            | local_name!("div")
            | local_name!("dl")
            | local_name!("dt")
            | local_name!("fieldset")
            | local_name!("figcaption")
            | local_name!("figure")
            | local_name!("footer")
            | local_name!("form")
            | local_name!("header")
            | local_name!("legend")
            | local_name!("main")
            | local_name!("menu")
            | local_name!("nav")
            | local_name!("p")
            | local_name!("section")
            | local_name!("summary")
            | local_name!("thead")
            | local_name!("tbody")
            | local_name!("tfoot")
            | local_name!("tr")
            | local_name!("td")
            | local_name!("th") => Self::Block,
            _ => Self::Inline,
        }
    }

    /// Whether the element starts and ends a line of the code it is inside.
    fn breaks_code(self) -> bool {
        !matches!(
            self,
            Self::Image | Self::Code | Self::Emphasis(_) | Self::Link | Self::Inline
        )
    }
}

/// Ends the code's last line, unless it has none or it is ended.
fn end_line(code: &mut String) {
    if !code.is_empty() && !code.ends_with('\n') {
        code.push('\n');
    }
}

enum Step<'a> {
    Enter(NodeRef<'a>),
    Exit(Close),
    /// A row of a table written as GFM rows, the first followed by the
    /// delimiter row for its `delimiter` columns.
    Row {
        cells: Vec<Option<NodeRef<'a>>>,
        delimiter: Option<usize>,
    },
    /// A cell of such a row with the content of its element, if any.
    Cell(Option<NodeRef<'a>>),
}

/// What leaving an element writes.
enum Close {
    Block,
    /// Closes the innermost inline mark, unless no content followed its opening.
    Span,
    CodeBlock {
        language: String,
    },
    CodeSpan,
    /// A block inside code, which ends a line of it.
    CodeLine,
    Heading,
    Quote,
    List,
    /// A list inside a list item, which continues the item.
    NestedList,
    Item,
    Row {
        delimiter: Option<usize>,
    },
    Cell,
}

impl Close {
    fn apply(self, writer: &mut Writer, lists: &mut Vec<usize>) -> Result<(), Spent> {
        // A word ends with its element, before the state the element set is
        // undone; a span or code element ends it only where it writes anything.
        if !matches!(self, Self::Span | Self::CodeSpan) {
            writer.flush()?;
        }
        match self {
            Self::Block => writer.block()?,
            Self::Span => writer.end_span()?,
            Self::CodeBlock { language } => writer.end_code_block(&language)?,
            Self::CodeSpan => writer.end_code_span()?,
            Self::CodeLine => {
                if let Some(code) = writer.code() {
                    end_line(code);
                }
            }
            Self::Heading => writer.end_heading()?,
            Self::Quote => {
                writer.pop_prefix();
                writer.block()?;
            }
            Self::List => {
                lists.pop();
                writer.block()?;
            }
            Self::NestedList => {
                lists.pop();
                writer.line()?;
            }
            Self::Item => {
                writer.pop_prefix();
            }
            Self::Row { delimiter } => {
                if let Some(columns) = delimiter {
                    writer.line()?;
                    writer.syntax("|")?;
                    for _ in 0..columns {
                        writer.syntax(" --- |")?;
                    }
                }
            }
            Self::Cell => writer.cell_end()?,
        }
        Ok(())
    }
}

fn name(node: &NodeRef) -> Option<LocalName> {
    node.qual_name_ref().map(|name| name.local.clone())
}

/// Whether `href` is inline data: bulk, not a link to follow.
fn inline_data(href: &str) -> bool {
    reqwest::Url::parse(href).is_ok_and(|url| url.scheme() == "data")
}

fn parent_is(node: &NodeRef, tag: LocalName) -> bool {
    node.parent()
        .and_then(|parent| name(&parent))
        .is_some_and(|name| name == tag)
}

/// A table with a header row, its first, in a `thead` or holding a `th`, as rows
/// of cells all as wide as the widest. A cell is repeated in every column and row
/// it spans within its row group, so its value stays under each header it spans,
/// and a row is padded with empty cells. None for a table without a header row,
/// which reads as its text. The grid is charged to `budget` as it is laid out.
fn grid<'a>(
    table: NodeRef<'a>,
    scope: Scope,
    budget: &Budget,
) -> Result<Option<Vec<Vec<Option<NodeRef<'a>>>>>, Spent> {
    const SLOT: usize = size_of::<Option<NodeRef>>();
    let kept = |node: &NodeRef| name(node).is_some_and(|tag| !scope.skips(node, &tag));
    let named =
        |node: &NodeRef, tags: &[LocalName]| name(node).is_some_and(|tag| tags.contains(&tag));
    let (group, row, cell) = (
        [
            local_name!("thead"),
            local_name!("tbody"),
            local_name!("tfoot"),
        ],
        [local_name!("tr")],
        [local_name!("td"), local_name!("th")],
    );
    // The parser places every row of a table in a row group.
    let groups: Vec<Vec<_>> = (table.children_it(false))
        .filter(|child| kept(child) && named(child, &group))
        .map(|group| {
            (group.children_it(false))
                .filter(|child| kept(child) && named(child, &row))
                .collect()
        })
        .collect();
    let header = groups.iter().flatten().next().is_some_and(|first| {
        parent_is(first, local_name!("thead"))
            || first
                .children_it(false)
                .any(|child| name(&child) == Some(local_name!("th")))
    });
    if !header {
        return Ok(None);
    }
    let mut grid = Vec::new();
    for rows in groups {
        // Each column's cell spanning down, and how many rows of the group below
        // it still spans.
        let mut spans: Vec<(Option<NodeRef>, usize)> = Vec::new();
        for row in rows {
            let above: Vec<_> = (spans.iter_mut())
                .map(|(cell, rows)| {
                    let spanned = (*rows > 0).then_some(*cell).flatten();
                    *rows = rows.saturating_sub(1);
                    spanned
                })
                .collect();
            let mut cells = Vec::new();
            let mut place = |cell: Option<NodeRef<'a>>| {
                budget.charge(SLOT)?;
                cells.push(cell);
                Ok(())
            };
            let mut column = 0;
            for cell in row
                .children_it(false)
                .filter(|child| kept(child) && named(child, &cell))
            {
                while let Some(Some(spanned)) = above.get(column) {
                    place(Some(*spanned))?;
                    column += 1;
                }
                // As browsers read them: at most 1000 columns, and a row span of
                // zero reaches the end of the row group.
                let colspan = span(&cell, "colspan").unwrap_or(1).clamp(1, 1000);
                let below = match span(&cell, "rowspan").unwrap_or(1) {
                    0 => usize::MAX,
                    rows => rows.min(65_534) - 1,
                };
                for _ in 0..colspan {
                    place(Some(cell))?;
                    if spans.len() <= column {
                        spans.resize(column + 1, (None, 0));
                    }
                    spans[column] = (Some(cell), below);
                    column += 1;
                }
            }
            for spanned in above.into_iter().skip(column) {
                place(spanned)?;
            }
            grid.push(cells);
        }
    }
    let width = grid.iter().map(Vec::len).max().unwrap_or(0);
    for cells in &mut grid {
        budget.charge((width - cells.len()) * SLOT)?;
        cells.resize(width, None);
    }
    Ok(Some(grid))
}

/// The cell's `attribute` span as HTML reads a non-negative integer: the digits
/// after any whitespace and a plus sign, up to the first that is not one, and
/// saturating. None without a digit.
fn span(cell: &NodeRef, attribute: &str) -> Option<usize> {
    let value = cell.attr(attribute)?;
    let value = value.trim_ascii_start();
    let digits = (value.strip_prefix('+').unwrap_or(value).bytes()).take_while(u8::is_ascii_digit);
    digits.fold(None, |number, digit| {
        let number = number.unwrap_or(0usize).saturating_mul(10);
        Some(number.saturating_add(usize::from(digit - b'0')))
    })
}

/// The language a `language-` class names on the block or its code, limited to
/// what a fence's info string can hold.
fn language(pre: NodeRef) -> String {
    let code = pre
        .first_element_child()
        .filter(|child| name(child) == Some(local_name!("code")));
    [Some(pre), code]
        .into_iter()
        .flatten()
        .filter_map(|node| node.class())
        .find_map(|class| {
            class
                .split_ascii_whitespace()
                .find_map(|name| name.strip_prefix("language-"))
                .map(|language| {
                    let allowed = |ch: &char| ch.is_ascii_alphanumeric() || "_+.#-".contains(*ch);
                    language.chars().filter(allowed).collect()
                })
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::super::budget;
    use super::*;

    fn convert(html: &str, scope: Scope) -> Result<String, Unreadable> {
        let base = reqwest::Url::parse("https://example.com/a/").unwrap();
        let budget = Budget::default();
        let document = budget::parse(html, &budget).unwrap();
        markdown(document.body().unwrap(), scope, &base, &budget)
    }

    fn assert_converts(cases: &[(&str, &str)]) {
        for (html, expected) in cases {
            assert_eq!(
                convert(html, Scope::Page).as_deref(),
                Ok(*expected),
                "{html}"
            );
        }
    }

    #[test]
    fn writes_block_and_inline_structure() {
        let digits = "1".repeat(64 * 1024);
        let (ordinal, escaped) = (format!("<p>{digits}.</p>"), format!("{digits}\\."));
        assert_converts(&[
            (
                "<h2>Title <em>here</em></h2><p>One<br>two <b> bold </b>end.</p><hr><p>after</p>",
                "## Title *here*\n\nOne  \ntwo **bold** end.\n\n* * *\n\nafter",
            ),
            (
                "<ul><li>a<ul><li>b</li></ul></li><li><p>c</p><p>d</p></li></ul><ol start=9><li>x</li><li>y</li></ol>",
                "- a\n  - b\n\n- c\n\n  d\n\n9. x\n10. y",
            ),
            (
                "<blockquote><p>q1</p><p>q2 <a href='/x'>link</a></p><blockquote>deep</blockquote></blockquote>",
                "> q1\n>\n> q2 [link](https://example.com/x)\n> >\n> > deep",
            ),
            (
                "<pre class='language-rust'>let a = 1;\n```\n  b</pre><p>use <code>a`b</code> and <code><a href=t>t</a></code></p>",
                "````rust\nlet a = 1;\n```\n  b\n````\n\nuse ``a`b`` and `t`",
            ),
            // Code is literal, and fenced once around nested code.
            (
                "<p><code>foo<em>bar</em><code>`b`</code></code></p>",
                "`` foobar`b` ``",
            ),
            // Text that would read as syntax is escaped; inline image data is dropped.
            (
                "<p>1. not a list</p><p>- nor this</p><p># or this, a_b _c *c* [d]</p><p><img src='i.png' alt='[pic]'><img src=' DATA:x'></p>",
                "1\\. not a list\n\n\\- nor this\n\n\\# or this, a_b \\_c \\*c\\* \\[d\\]\n\n![\\[pic\\]](https://example.com/a/i.png)",
            ),
            (&ordinal, &escaped),
            // Attribute text is text: whitespace collapses and markup is escaped.
            (
                "<p><img src=i.png alt='first\n\n# Injected heading\n- item'></p><table><tr><th><img alt='two\nlines' src=j.png></th><th>v</th></tr></table>",
                "![first \\# Injected heading - item](https://example.com/a/i.png)\n\n| ![two lines](https://example.com/a/j.png) | v |\n| --- | --- |",
            ),
            (
                "<pre class='language-x`y'># h\n- i</pre><p>&amp;copy; ~~old~~ <a href='mailto:a\\'>mail</a></p><h2>Issue #</h2><ol start=18446744073709551615><li>x</li><li>y</li></ol>",
                "```xy\n# h\n- i\n```\n\n\\&copy; \\~\\~old\\~\\~ [mail](mailto:a%5C)\n\n## Issue \\#\n\n18446744073709551615. x\n18446744073709551615. y",
            ),
            // Text never becomes markup: an image, a table, a heading inside a
            // link, a fence inside emphasis, or emphasis around a line break.
            (
                "<p>Hello!<a href=u>x</a> <a href='/a)b?x=&amp;copy;'>y</a></p><p>| a |<br>| --- |</p>",
                "Hello\\![x](https://example.com/a/u) [y](https://example.com/a%29b?x=\\&copy;)\n\n\\| a \\|  \n\\| --- \\|",
            ),
            (
                "<a href=u><h2>T</h2></a><b><pre>x</pre></b><p><strong>Note:<br></strong>next</p>",
                "## [T](https://example.com/a/u)\n\n```\nx\n```\n\n**Note:**  \nnext",
            ),
            (
                "<table><tr><th>h</th></tr><tr><td><b><ul><li></li></ul></b>x</td></tr></table>",
                "| h |\n| --- |\n| x |",
            ),
            (
                "<table><tr><th>A|B</th><th>C</th></tr><tr><td><p>x</p><p>y</p></td></tr></table><table><tr><td>plain</td></tr></table>",
                "| A\\|B | C |\n| --- | --- |\n| x y |  |\n\nplain",
            ),
        ]);
    }

    /// A spanning cell repeats in each column and row it spans, and every row is
    /// as wide as the widest, so each value is read under its header.
    #[test]
    fn tables_repeat_spanning_cells_in_a_rectangular_grid() {
        assert_converts(&[
            (
                "<table><caption>Cap</caption><tr><th colspan=2>Name</th></tr><tr><td>A</td><td>B</td></tr></table>",
                "Cap\n\n| Name | Name |\n| --- | --- |\n| A | B |",
            ),
            (
                "<table><tr><th>K</th><th>V</th><th>W</th></tr><tr><td rowspan=2>k</td><td>1</td><td rowspan=0>w</td></tr><tr><td>2</td></tr><tr><td>x</td></tr></table>",
                "| K | V | W |\n| --- | --- | --- |\n| k | 1 | w |\n| k | 2 | w |\n| x |  | w |",
            ),
            (
                "<table><thead><tr><td>h</td></tr></thead><tr><td>a</td><td colspan=x>b</td></tr></table>",
                "| h |  |\n| --- | --- |\n| a | b |",
            ),
            // A row span ends with its row group.
            (
                "<table><thead><tr><th rowspan=0>A</th><th>B</th></tr></thead><tbody><tr><td rowspan=2>x</td><td>y</td></tr></tbody><tbody><tr><td>z</td><td>w</td></tr></tbody></table>",
                "| A | B |\n| --- | --- |\n| x | y |\n| z | w |",
            ),
            // Spans read as browsers read them: leading digits, saturating.
            (
                "<table><tr><th colspan=' +2x'>H</th></tr><tr><td rowspan=99999999999999999999999>k</td><td>1</td></tr><tr><td>2</td></tr></table>",
                "| H | H |\n| --- | --- |\n| k | 1 |\n| k | 2 |",
            ),
        ]);
        // A spanning cell is written once, however much it reads to write little.
        let spanning = format!(
            "<table><tr><th>h</th></tr><tr><td colspan=1000 rowspan=0>{}{}</td></tr>{}</table>",
            " ".repeat(1 << 20),
            "<i></i>".repeat(10_000),
            "<tr></tr>".repeat(500)
        );
        assert!(convert(&spanning, Scope::Page).is_ok());
    }

    #[test]
    fn inline_marks_close_at_block_and_cell_ends() {
        assert_converts(&[
            // A card link becomes one link per block it spans.
            (
                "<a href=u><div><h3>Title</h3><p>desc</p></div></a>",
                "### [Title](https://example.com/a/u)\n\n[desc](https://example.com/a/u)",
            ),
            (
                "<a href='/doc'><p>first</p><p>second</p></a>",
                "[first](https://example.com/doc)\n\n[second](https://example.com/doc)",
            ),
            ("<p><b>one<br>two</b></p>", "**one**  \n**two**"),
            // Inside a table, a mark closes in each cell it spans.
            (
                "<b><table><tr><th>x</th><th>y</th></tr></table></b>",
                "| **x** | **y** |\n| --- | --- |",
            ),
            (
                "<table><tr><th>h</th></tr><tr><td><a href=u>a<p>b</p></a></td></tr></table>",
                "| h |\n| --- |\n| [a b](https://example.com/a/u) |",
            ),
        ]);
    }

    #[test]
    fn code_keeps_its_line_breaks_and_drops_skipped_elements() {
        assert_converts(&[
            ("<pre>a<br>b</pre>", "```\na\nb\n```"),
            ("<pre><div>a</div><div>b</div>c</pre>", "```\na\nb\nc\n```"),
            ("<pre><code>a<br>b</code></pre>", "```\na\nb\n```"),
            (
                "<pre>let x;<button>Copy</button></pre><p><code>y<button>Copy</button></code></p>",
                "```\nlet x;\n```\n\n`y`",
            ),
            // A code span holds no line break, so one reads as the space it renders as.
            ("<p><code>a<br>b</code></p>", "`a b`"),
        ]);
    }

    #[test]
    fn escapes_words_split_across_text_nodes() {
        assert_converts(&[
            ("<p>&amp;<span>copy;</span></p>", "\\&copy;"),
            ("<p><span>1</span>. Not a list</p>", "1\\. Not a list"),
            ("<p>1<span>.</span> Intro</p>", "1\\. Intro"),
            // Elements that write nothing leave the word around them whole.
            ("<p>&amp;<b></b>copy;</p>", "\\&copy;"),
            ("<p>1<b></b>. Not a list</p>", "1\\. Not a list"),
            ("<p>&amp;<a href='/x'></a>copy;</p>", "\\&copy;"),
            ("<p>1<code></code>. Not a list</p>", "1\\. Not a list"),
            ("<p>1<b><i></i></b>. x</p>", "1\\. x"),
            ("<p>a<b>b</b>c</p>", "a**b**c"),
            ("<p>a<code>b</code>c</p>", "a`b`c"),
            (
                "<p>snake_case _lead trail_ a__b</p>",
                "snake_case \\_lead trail\\_ a\\_\\_b",
            ),
        ]);
    }

    #[test]
    fn page_scope_skips_hidden_elements() {
        let html = concat!(
            "<p>shown</p><p hidden>HIDDEN</p><div aria-hidden=' TRUE '>ARIA</div><p aria-hidden=false>kept</p>",
            "<p style='color: red; DISPLAY: None !important'>NONE</p><p style='visibility:hidden'>INVISIBLE</p>",
            "<p style='display:none; display: block'>styled</p>",
            "<p style='display:none !important; display:block'>IMPORTANT</p>",
            "<p style='visibility:visible ! IMPORTANT; visibility:hidden'>important</p>",
        );
        assert_eq!(
            convert(html, Scope::Page).as_deref(),
            Ok("shown\n\nkept\n\nstyled\n\nimportant")
        );
        let article = convert(html, Scope::Article).unwrap();
        for hidden in ["HIDDEN", "ARIA", "NONE", "INVISIBLE", "IMPORTANT"] {
            assert!(article.contains(hidden), "{article}");
        }
    }
}
