//! An extension's page laid out as rows: wrapped facts and stages, panels side by side,
//! and sections whose headers, records and "more" rows toggle what the viewer has open.
use super::*;
use crate::page::{
    Block, Cell, Detail, Disclosed, Fact, Page, Panel, Section, Stage, StageState, Toggle, Tone,
};

/// One laid-out row, what clicking it opens, and which of its spans are spinners: the
/// row is laid out once and only those spans change as the spinner turns.
pub(in crate::tui) struct PageLine {
    pub line: Line<'static>,
    pub toggle: Option<Toggle>,
    pub spinners: Vec<usize>,
}

const GAP: usize = 1;
const SEPARATOR: &str = " · ";
const STAGE_SEPARATOR: &str = " ── ";
/// The narrowest bar a panel row keeps beside its label and values.
const METER_MIN: usize = 8;
/// Indents of records under their section and of detail under its record.
const RECORD_INDENT: usize = 2;
const DETAIL_INDENT: usize = 6;
/// The narrowest values keep beside their labels.
const DETAIL_VALUE_MIN: usize = 16;

fn style(tone: Tone) -> Style {
    let ui = theme().ui;
    Style::default().fg(match tone {
        Tone::Normal => ui.fg,
        Tone::Muted => ui.muted,
        Tone::Accent => ui.accent,
        Tone::Success => ui.success,
        Tone::Warning => ui.warning,
        Tone::Error => ui.error,
    })
}

fn span(cell: &Cell) -> Span<'static> {
    Span::styled(model::clean(&cell.text), style(cell.tone))
}

fn muted(text: impl Into<String>) -> Span<'static> {
    Span::styled(text.into(), style(Tone::Muted))
}

fn plain(line: Line<'static>) -> PageLine {
    toggled(line, None)
}

fn toggled(line: Line<'static>, toggle: Option<Toggle>) -> PageLine {
    PageLine {
        line,
        toggle,
        spinners: Vec::new(),
    }
}

impl PageLine {
    /// A message shown in place of a page.
    pub(in crate::tui) fn text(text: String) -> Self {
        plain(Line::from(model::clean(&text)))
    }
}

/// The page as rows `width` cells wide.
pub(in crate::tui) fn lay_out(page: &Page, open: &Disclosed, width: usize) -> Vec<PageLine> {
    let mut lines = Vec::new();
    let mut previous: Option<&Block> = None;
    for block in &page.blocks {
        // Blocks of one kind run on; a different kind starts after a blank row.
        let kind = std::mem::discriminant(block);
        if previous.is_some_and(|previous| std::mem::discriminant(previous) != kind) {
            lines.push(plain(Line::default()));
        }
        match block {
            Block::Facts(facts) => lines.extend(flow(
                facts.iter().map(|f| (fact(f), None)),
                SEPARATOR,
                width,
            )),
            Block::Pipeline(stages) => {
                lines.extend(flow(stages.iter().map(stage), STAGE_SEPARATOR, width))
            }
            Block::Panels(panels) => lines.extend(panel_rows(panels, width)),
            Block::Section(section) => lines.extend(section_rows(section, open, width)),
        }
        previous = Some(block);
    }
    lines
}

fn fact(fact: &Fact) -> Vec<Span<'static>> {
    vec![muted(format!("{} ", fact.label)), span(&fact.value)]
}

/// A stage's spans, and which of them is a running spinner.
fn stage(stage: &Stage) -> (Vec<Span<'static>>, Option<usize>) {
    let (glyph, tone, label) = match stage.state {
        StageState::Done => ("✓", Tone::Success, Tone::Normal),
        StageState::Active => (spinner(0), Tone::Accent, Tone::Normal),
        StageState::Pending => ("·", Tone::Muted, Tone::Muted),
        StageState::Blocked => ("!", Tone::Warning, Tone::Warning),
    };
    let mut spans = vec![
        Span::styled(format!("{} {}", stage.label, stage.value), style(label)),
        Span::raw(" "),
    ];
    spans.push(Span::styled(glyph, style(tone)));
    let spinner = (stage.state == StageState::Active).then(|| spans.len() - 1);
    (spans, spinner)
}

/// Groups of spans joined by `separator`, a group never split across rows, each with
/// the span of it that is a spinner.
fn flow(
    groups: impl Iterator<Item = (Vec<Span<'static>>, Option<usize>)>,
    separator: &str,
    width: usize,
) -> Vec<PageLine> {
    let mut lines = Vec::new();
    let mut line = plain(Line::default());
    for (group, spinner) in groups {
        let group = Line::from(group);
        let joined = line.line.width() + separator.width() + group.width();
        if !line.line.spans.is_empty() && joined > width {
            lines.push(std::mem::replace(&mut line, plain(Line::default())));
        }
        if !line.line.spans.is_empty() {
            line.line.spans.push(muted(separator));
        }
        if let Some(index) = spinner {
            line.spinners.push(line.line.spans.len() + index);
        }
        line.line.spans.extend(group.spans);
    }
    if !line.line.spans.is_empty() {
        lines.push(line);
    }
    for row in &mut lines {
        row.line = clipped(std::mem::take(&mut row.line), width);
    }
    lines
}

/// Each value column's width over a panel's header and rows.
fn value_widths(panel: &Panel) -> Vec<usize> {
    let columns = (panel.rows.iter().map(|row| row.values.len()))
        .chain([panel.columns.len()])
        .max()
        .unwrap_or_default();
    (0..columns)
        .map(|column| {
            let header = panel.columns.get(column).map_or(0, |title| title.width());
            (panel.rows.iter())
                .filter_map(|row| row.values.get(column))
                .map(|cell| cell.text.width())
                .chain([header])
                .max()
                .unwrap_or_default()
        })
        .collect()
}

/// Border, padding and content: the narrowest box that shows a panel whole.
fn panel_width(panel: &Panel) -> usize {
    let values = joined_width(&value_widths(panel), "  ");
    let rows = panel.rows.iter().map(|row| {
        let meter = if row.meter.is_some() {
            METER_MIN + 1
        } else {
            0
        };
        row.label.width() + 2 + meter + values
    });
    let title = panel.title.width() + 4;
    rows.chain([title, values]).max().unwrap_or_default() + 4
}

fn panel_rows(panels: &[Panel], width: usize) -> Vec<PageLine> {
    let widest = panels.iter().map(panel_width).max().unwrap_or_default();
    let across = (1..=panels.len().max(1))
        .rev()
        .find(|&count| count * widest + (count - 1) * GAP <= width)
        .unwrap_or(1);
    let box_width = ((width + GAP) / across).saturating_sub(GAP).max(4);
    let mut lines = Vec::new();
    for (index, row) in panels.chunks(across).enumerate() {
        if index > 0 {
            lines.push(plain(Line::default()));
        }
        // Boxes in a row share their height: the shorter are padded before they close.
        let boxes: Vec<_> = row
            .iter()
            .map(|panel| panel_box(panel, box_width))
            .collect();
        let height = boxes.iter().map(Vec::len).max().unwrap_or_default();
        let empty = boxed(Line::default(), box_width);
        let bottom = Line::from(muted(format!("╰{}╯", "─".repeat(box_width - 2))));
        for offset in 0..=height {
            let mut line = Line::default();
            for (column, lines) in boxes.iter().enumerate() {
                if column > 0 {
                    line.spans.push(Span::raw(" ".repeat(GAP)));
                }
                let part = match lines.get(offset) {
                    Some(part) => part,
                    None if offset == height => &bottom,
                    None => &empty,
                };
                line.spans.extend(part.spans.iter().cloned());
            }
            lines.push(plain(line));
        }
    }
    lines
}

/// `content` between a box's side borders, `width` cells in all.
fn boxed(content: Line<'static>, width: usize) -> Line<'static> {
    let inner = width.saturating_sub(4);
    let content = clipped(content, inner);
    let pad = " ".repeat(inner - content.width());
    let mut spans = vec![muted("│ ")];
    spans.extend(content.spans);
    spans.push(Span::raw(pad));
    spans.push(muted(" │"));
    Line::from(spans)
}

/// A panel's top border and rows, `width` cells wide; its row closes the box.
fn panel_box(panel: &Panel, width: usize) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(4);
    let widths = value_widths(panel);
    let values_width = joined_width(&widths, "  ");
    let title = clipped(Line::from(panel.title.clone()), width.saturating_sub(5));
    let top = vec![
        muted("╭ "),
        Span::styled(title.to_string(), Style::default().fg(theme().ui.strong)),
        muted(format!(
            " {}╮",
            "─".repeat(width.saturating_sub(4 + title.width()))
        )),
    ];
    let mut lines = vec![Line::from(top)];
    if !panel.columns.is_empty() {
        let headers: Vec<String> = (widths.iter().enumerate())
            .map(|(column, width)| {
                right_aligned(panel.columns.get(column).map_or("", String::as_str), *width)
            })
            .collect();
        let header = format!(
            "{}{}",
            " ".repeat(inner.saturating_sub(values_width)),
            headers.join("  ")
        );
        lines.push(boxed(Line::from(muted(header)), width));
    }
    // Labels share a column; bars end beside the values.
    let labels = (panel.rows.iter().map(|row| row.label.width()))
        .max()
        .unwrap_or_default();
    for row in &panel.rows {
        let label = format!("{}{}", row.label, " ".repeat(labels - row.label.width()));
        let mut spans = vec![Span::raw(label)];
        // The label column, a space, the bar or blank room, a space, then the values.
        let room = inner.saturating_sub(labels + values_width + 1);
        match row.meter {
            Some(meter) if room >= 2 => {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    bar(meter.value, meter.total, room - 1),
                    style(Tone::Accent),
                ));
                spans.push(Span::raw(" "));
            }
            _ => spans.push(Span::raw(" ".repeat(room + 1))),
        }
        for (column, width) in widths.iter().enumerate() {
            if column > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(row.values.get(column).map_or_else(
                || Span::raw(" ".repeat(*width)),
                |cell| Span::styled(right_aligned(&cell.text, *width), style(cell.tone)),
            ));
        }
        lines.push(boxed(Line::from(spans), width));
    }
    lines
}

/// `value` of `total` as a bar of `width` cells ending at the right edge, beside the value.
/// The leading partial cell has only right-hand eighth and half blocks, and an eighth is too
/// thin to see, so any non-zero value fills at least half a cell.
fn bar(value: u64, total: u64, width: usize) -> String {
    let eighths = if total == 0 {
        0
    } else {
        (value.min(total) as u128 * width as u128 * 8).div_ceil(total as u128) as usize
    };
    let eighths = if value > 0 { eighths.max(4) } else { eighths };
    let (full, part) = (eighths / 8, eighths % 8);
    let lead = match part {
        0 => "",
        1..=5 => "▐",
        _ => "█",
    };
    let used = full + usize::from(part > 0);
    " ".repeat(width.saturating_sub(used)) + lead + &"█".repeat(full)
}

fn section_rows(section: &Section, open: &Disclosed, width: usize) -> Vec<PageLine> {
    let opened = open.limit(section.key).is_some();
    let mut header = vec![
        muted(model::Disclosure::new(opened).glyph()),
        Span::styled(
            model::clean(&section.title),
            Style::default().fg(theme().ui.strong),
        ),
    ];
    let mut counts = section.total.to_string();
    if let Some(summary) = &section.summary {
        counts.push_str(SEPARATOR);
        counts.push_str(summary);
    }
    header.push(muted(format!(" ({counts})")));
    // An open section's own rows, like an open record's detail, close it when clicked.
    let own = || Some(Toggle::Section(section.key));
    let mut lines = vec![toggled(clipped(Line::from(header), width), own())];
    if !opened {
        return lines;
    }
    if section.total == 0 {
        let none = muted(format!("{}none", " ".repeat(RECORD_INDENT + 2)));
        lines.push(toggled(Line::from(none), own()));
        return lines;
    }
    let indent = RECORD_INDENT + 2;
    let widths = record_widths(section, width.saturating_sub(indent));
    let row = |cells: Vec<Span<'static>>| {
        let mut spans = Vec::new();
        for (index, (cell, width)) in cells.into_iter().zip(&widths).enumerate() {
            let Some(width) = width else { continue };
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            let cell = clipped(Line::from(cell), *width);
            let pad = (index + 1 < widths.len()).then(|| " ".repeat(width - cell.width()));
            spans.extend(cell.spans);
            spans.extend(pad.map(Span::raw));
        }
        spans
    };
    let titles = section
        .columns
        .iter()
        .map(|column| muted(column.title.clone()));
    let mut heading = vec![Span::raw(" ".repeat(indent))];
    heading.extend(row(titles.collect()));
    lines.push(toggled(Line::from(heading), own()));
    for record in &section.records {
        let opened = open.detail(section.key, &record.key).is_some();
        let glyph = model::Disclosure::new(opened).glyph();
        let mut spans = vec![Span::raw(" ".repeat(RECORD_INDENT)), muted(glyph)];
        spans.extend(row(record.cells.iter().map(span).collect()));
        let toggle = Toggle::Record(section.key, record.key.clone());
        lines.push(toggled(
            clipped(Line::from(spans), width),
            Some(toggle.clone()),
        ));
        if let Some(detail) = &record.detail {
            lines.extend(detail_rows(&detail.lines, width, &toggle));
            if detail.more {
                let more = muted(format!("{}… more", " ".repeat(DETAIL_INDENT)));
                let toggle = Toggle::MoreDetail(section.key, record.key.clone());
                lines.push(toggled(Line::from(more), Some(toggle)));
            }
        }
    }
    let shown = section.records.len() as u64;
    if shown < section.total {
        let more = muted(format!(
            "{}… {shown} of {} · more",
            " ".repeat(indent),
            section.total
        ));
        lines.push(toggled(Line::from(more), Some(Toggle::More(section.key))));
    }
    lines
}

/// Each column's width, `None` for one dropped; the last takes what remains.
fn record_widths(section: &Section, room: usize) -> Vec<Option<usize>> {
    let columns = &section.columns;
    let mut widths: Vec<Option<usize>> = (columns.iter().enumerate())
        .map(|(index, column)| {
            let cells = (section.records.iter())
                .filter_map(|record| record.cells.get(index))
                .map(|cell| cell.text.width());
            Some(
                cells
                    .chain([column.title.width()])
                    .max()
                    .unwrap_or_default(),
            )
        })
        .collect();
    let Some(last) = columns.len().checked_sub(1) else {
        return widths;
    };
    let fixed = |widths: &[Option<usize>]| {
        let kept: Vec<usize> = widths[..last].iter().flatten().copied().collect();
        joined_width(&kept, "  ") + 2 * usize::from(!kept.is_empty())
    };
    // Keep at least a few cells of the last column before dropping others.
    while fixed(&widths) + 8 > room {
        let droppable = (0..last)
            .filter(|&index| widths[index].is_some() && columns[index].priority > 0)
            .max_by_key(|&index| (columns[index].priority, index));
        let Some(index) = droppable else { break };
        widths[index] = None;
    }
    let remaining = room.saturating_sub(fixed(&widths));
    widths[last] = widths[last].map(|width| width.min(remaining));
    widths
}

/// A record's detail rows, each closing `record` when clicked. Values sit beside their
/// labels, or below them on a page too narrow to leave values `DETAIL_VALUE_MIN` cells.
fn detail_rows(detail: &[Detail], width: usize, record: &Toggle) -> Vec<PageLine> {
    let label_width = (detail.iter())
        .filter_map(|line| line.label.as_ref())
        .map(|label| label.width())
        .max()
        .unwrap_or_default();
    let beside = DETAIL_INDENT + label_width + 2;
    let stacked = width < beside + DETAIL_VALUE_MIN;
    let indent = if stacked { DETAIL_INDENT + 2 } else { beside };
    let row = |line: Line<'static>| toggled(clipped(line, width), Some(record.clone()));
    let mut lines = Vec::new();
    for line in detail {
        let label = line.label.as_deref().unwrap_or_default();
        let mut lead = format!("{}{label}", " ".repeat(DETAIL_INDENT));
        if !stacked {
            lead.push_str(&" ".repeat(indent - lead.width()));
        } else if !label.is_empty() {
            lines.push(row(Line::from(muted(lead))));
            lead = " ".repeat(indent);
        }
        let wrapped = (hard_rows(&line.cells).into_iter())
            .flat_map(|hard| wrap_words(hard, width.saturating_sub(indent).max(1)));
        for (index, mut part) in wrapped.enumerate() {
            // A wrapped row keeps the space it broke at, which would overflow the width.
            if let Some(last) = part.spans.last_mut() {
                last.content = last.content.trim_end().to_owned().into();
            }
            let lead = match index {
                0 => std::mem::take(&mut lead),
                _ => " ".repeat(indent),
            };
            part.spans.insert(0, muted(lead));
            lines.push(row(part));
        }
    }
    lines
}

/// A detail's cells, a space apart, as the rows their hard line breaks start.
fn hard_rows(cells: &[Cell]) -> Vec<Line<'static>> {
    let mut rows = vec![Line::default()];
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            rows.last_mut().expect("a row").spans.push(Span::raw(" "));
        }
        for (part, text) in cell.text.split('\n').enumerate() {
            if part > 0 {
                rows.push(Line::default());
            }
            let text = Span::styled(model::clean(text), style(cell.tone));
            rows.last_mut().expect("a row").spans.push(text);
        }
    }
    rows
}

/// How the page is drawn: what is chosen and whether to scroll to it, the pointer to
/// highlight under, and where the draw reports hits and running spinners.
pub(in crate::tui) struct PageDraw<'a> {
    pub selected: Option<&'a Toggle>,
    pub reveal: bool,
    pub hover: Option<(u16, u16)>,
    pub hits: &'a mut Vec<(Rect, Hit)>,
    pub animating: &'a mut bool,
    /// The spinners' frame.
    pub tick: usize,
}

/// Draw `lines` in `area`, scrolled by `view`, adding a hit target for each toggle drawn.
pub(in crate::tui) fn draw_page(
    frame: &mut Frame,
    area: Rect,
    lines: &[PageLine],
    view: &mut super::super::viewport::Viewport,
    draw: PageDraw<'_>,
) {
    let PageDraw {
        selected,
        reveal,
        hover,
        hits,
        animating,
        tick,
    } = draw;
    let page = page_area(area);
    // A revealed record shows whole when it fits, else from its header.
    let rows = page.height.into();
    let span = selected.filter(|_| reveal).and_then(|toggle| {
        let on = |line: &PageLine| line.toggle.as_ref() == Some(toggle);
        let first = lines.iter().position(on)?;
        Some((
            first,
            first + lines[first..].iter().take_while(|line| on(line)).count() - 1,
        ))
    });
    if let Some((_, last)) = span {
        view.fit(rows, lines.len(), Some(last));
    }
    let top = view.fit(rows, lines.len(), span.map(|(first, _)| first));
    let base = Style::default().fg(theme().ui.fg).bg(theme().ui.base);
    for (offset, line) in lines.iter().skip(top).take(page.height.into()).enumerate() {
        let row = r(page.x, page.y + offset as u16, page.width, 1);
        // Like the conversation's entries: the chosen toggle's rows, and the row under
        // the pointer when it toggles something.
        let chosen = line.toggle.is_some()
            && (line.toggle.as_ref() == selected
                || hover.is_some_and(|point| row.contains(point.into())));
        *animating |= !line.spinners.is_empty();
        let style = if chosen {
            base.bg(theme().ui.selected)
        } else {
            base
        };
        if chosen {
            fill(frame, row, theme().ui.selected);
        }
        if line.spinners.is_empty() {
            render_line(&line.line, row, frame.buffer_mut(), style);
        } else {
            let mut turned = line.line.clone();
            for index in &line.spinners {
                if let Some(span) = turned.spans.get_mut(*index) {
                    span.content = spinner(tick).into();
                }
            }
            render_line(&turned, row, frame.buffer_mut(), style);
        }
        if let Some(toggle) = &line.toggle {
            hits.push((row, Hit::Page(toggle.clone())));
        }
    }
    let track = r(
        area.x + area.width.saturating_sub(1),
        page.y,
        1,
        page.height,
    );
    let shown = top as f64..(top + usize::from(page.height)) as f64;
    scroll_thumb(frame, track, shown, lines.len() as f64, theme().ui.base);
}

/// The page keeps the conversation's inset from the pane's edges; the scrollbar runs
/// along the right edge outside it.
pub(in crate::tui) fn page_area(area: Rect) -> Rect {
    area.inner(ratatui::layout::Margin::new(2, 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::{Column, Meter, PanelRow, Record, RecordKey, SectionKey};

    fn panel(title: &str) -> Panel {
        Panel {
            title: title.to_owned(),
            columns: vec!["pass".to_owned(), "run".to_owned()],
            rows: vec![PanelRow {
                label: "owed".to_owned(),
                values: vec!["12".into(), "12".into()],
                meter: Some(Meter { value: 3, total: 4 }),
            }],
        }
    }

    /// Bars grow leftwards from the value; a sliver of a non-zero share still shows.
    #[test]
    fn bars_end_beside_their_value() {
        assert_eq!(bar(1, 4, 8), "      ██");
        assert_eq!(bar(1, 100, 4), "   ▐");
        assert_eq!(bar(3, 3, 4), "████");
        assert_eq!(bar(0, 4, 4), "    ");
    }

    fn text(lines: &[PageLine]) -> Vec<String> {
        lines.iter().map(|line| line.line.to_string()).collect()
    }

    /// Panels sit three across while they fit, then two and one below, then stacked; a
    /// section's columns go highest priority first, the last column kept.
    #[test]
    fn panels_reflow_with_width_and_section_columns_drop_by_priority() {
        let page = Page {
            blocks: vec![Block::Panels(vec![panel("A"), panel("B"), panel("C")])],
        };
        let tops = |width| {
            let lines = text(&lay_out(&page, &Disclosed::default(), width));
            (lines.iter())
                .map(|line| line.matches('╭').count())
                .filter(|count| *count > 0)
                .collect::<Vec<_>>()
        };
        assert_eq!(tops(110), [3]);
        assert_eq!(tops(70), [2, 1]);
        assert_eq!(tops(30), [1, 1, 1]);

        const KEY: SectionKey = SectionKey("s");
        let column = |title: &str, priority| Column {
            title: title.to_owned(),
            priority,
        };
        let section = Page {
            blocks: vec![Block::Section(Section {
                key: KEY,
                title: "S".to_owned(),
                summary: None,
                total: 1,
                columns: vec![
                    column("id", 0),
                    column("status", 2),
                    column("severity", 1),
                    column("title", 0),
                ],
                records: vec![Record {
                    key: RecordKey("1".to_owned()),
                    cells: vec![
                        "#1".into(),
                        "shortlisted".into(),
                        "high".into(),
                        "a long enough title".into(),
                    ],
                    detail: None,
                }],
            })],
        };
        let mut open = Disclosed::default();
        open.apply(&Toggle::Section(KEY));
        let header = |width| text(&lay_out(&section, &open, width))[1].clone();
        assert!(header(80).contains("status") && header(80).contains("severity"));
        let narrow = header(36);
        assert!(
            !narrow.contains("status") && narrow.contains("severity"),
            "{narrow}"
        );
        let narrower = header(22);
        assert!(
            !narrower.contains("severity") && narrower.contains("title"),
            "{narrower}"
        );
    }

    /// A detail's hard line breaks start rows of their own; on a page too narrow to keep
    /// values beside their labels, each label sits above its values.
    #[test]
    fn details_break_at_newlines_and_stack_when_narrow() {
        let detail = [
            Detail {
                label: Some("evidence".to_owned()),
                cells: vec![Cell::new("alpha\nbeta", Tone::Normal)],
            },
            Detail {
                label: Some("id".to_owned()),
                cells: vec![Cell::new("seven", Tone::Normal)],
            },
        ];
        let record = Toggle::Record(SectionKey("s"), RecordKey("1".to_owned()));
        let rows = |width| text(&detail_rows(&detail, width, &record));
        assert_eq!(
            rows(40),
            [
                "      evidence  alpha",
                "                beta",
                "      id        seven",
            ]
        );
        assert_eq!(
            rows(22),
            [
                "      evidence",
                "        alpha",
                "        beta",
                "      id",
                "        seven",
            ]
        );
    }

    /// A stage in progress shows the agents view's spinner at the drawn frame, and
    /// drawing it asks for the next frame; the layout itself does not change.
    #[test]
    fn active_stages_spin() {
        let stage = |label: &str, state| Stage {
            label: label.to_owned(),
            value: "1".to_owned(),
            state,
        };
        let page = Page {
            blocks: vec![Block::Pipeline(vec![
                stage("sources", StageState::Done),
                stage("review", StageState::Active),
            ])],
        };
        let lines = lay_out(&page, &Disclosed::default(), 80);
        let backend = ratatui::backend::TestBackend::new(80, 4);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let (mut hits, mut animating) = (Vec::new(), false);
        terminal
            .draw(|frame| {
                let draw = PageDraw {
                    selected: None,
                    reveal: false,
                    hover: None,
                    hits: &mut hits,
                    animating: &mut animating,
                    tick: 3,
                };
                let area = frame.area();
                draw_page(frame, area, &lines, &mut Default::default(), draw);
            })
            .unwrap();
        let row: String = (0..80)
            .map(|x| terminal.backend().buffer()[(x, 1)].symbol().to_owned())
            .collect();
        assert!(
            row.contains(&format!("review 1 {}", spinner(3))) && !row.contains('◐'),
            "{row}"
        );
        assert!(animating);
    }
}
