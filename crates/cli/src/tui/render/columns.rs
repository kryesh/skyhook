//! Shared agent and request column measurement and formatting.

use super::*;
use skyhook::session::RequestPhase;

/// Numeric fields share their own right edge, not just the footer's right edge.
/// Measure all agents, including off-screen rows, to keep columns stable on scroll.
pub(super) struct AgentStatsColumns {
    widths: [usize; 4],
    /// The first field shown; a narrow column gives up fields leftmost first.
    first: usize,
    /// Between fields; headed columns need no delimiter.
    separator: &'static str,
}

pub(super) const AGENT_STATS_HEADERS: [&str; 4] = ["Output", "Input (uncached)", "Context", "Time"];
/// The time column fits a counter through its first hour, so ticking never
/// shifts the columns beside it.
const AGENT_TIME_WIDTH: usize = "59m 59s".len();

const SEP: &str = " · ";

pub(super) fn joined_width(widths: &[usize], separator: &str) -> usize {
    widths.iter().sum::<usize>() + separator.width() * widths.len().saturating_sub(1)
}

pub(super) fn right_aligned(value: &str, width: usize) -> String {
    format!("{}{value}", " ".repeat(width.saturating_sub(value.width())))
}

fn join_right_aligned(values: &[String], widths: &[usize], separator: &str) -> String {
    let values = values.iter().zip(widths);
    let values = values.map(|(value, width)| right_aligned(value, *width));
    values.collect::<Vec<_>>().join(separator)
}

/// Menu item columns share their right edges, measured over every item so they
/// stay put while filtering. When the widest label would get less than
/// `MENU_LABEL_ROOM`, whole columns go, leftmost first.
pub(super) struct MenuColumns {
    widths: Vec<usize>,
    /// The first column shown.
    first: usize,
}

/// Between menu columns, and between them and the label.
pub(super) const MENU_GAP: &str = "  ";
/// Cells a menu label keeps before columns give way to it.
pub(super) const MENU_LABEL_ROOM: usize = 30;

impl MenuColumns {
    pub(super) fn new(items: &[ItemRef<'_>], width: u16) -> Self {
        let mut widths = Vec::new();
        for item in items {
            crate::text::widen(&mut widths, item.columns.iter().map(String::as_str));
        }
        // Labels are measured only against columns, never for a long list without any.
        let label = || {
            items
                .iter()
                .map(|item| item.label.width().min(MENU_LABEL_ROOM))
                .max()
        };
        let mut columns = Self { widths, first: 0 };
        while columns.first < columns.widths.len()
            && (usize::from(width).checked_sub(columns.width()))
                .is_none_or(|room| Some(room) < label())
        {
            columns.first += 1;
        }
        columns
    }

    /// Cells the shown columns take from the row, with their gap from the label.
    pub(super) fn width(&self) -> usize {
        match joined_width(&self.widths[self.first..], MENU_GAP) {
            0 => 0,
            width => width + MENU_GAP.width(),
        }
    }

    pub(super) fn format(&self, values: &[String]) -> String {
        let values: Vec<_> = (self.first..self.widths.len())
            .map(|index| values.get(index).cloned().unwrap_or_default())
            .collect();
        join_right_aligned(&values, &self.widths[self.first..], MENU_GAP)
    }
}

/// Where an agent list is drawn: across the frame, or in a narrow column beside a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TreePlane {
    Band,
    Column,
}

impl TreePlane {
    /// Cells an agent row is indented per tree level, up to a third of the row.
    pub(super) fn indent(self, agent: &model::AgentInfo, width: u16) -> u16 {
        let step = match self {
            Self::Band => 4,
            Self::Column => 2,
        };
        (agent.id.depth() as u16 * step).min(width / 3)
    }
}

/// Columns shared by every row of an agent list, measured over all its agents.
pub(super) struct AgentColumns {
    pub(super) plane: TreePlane,
    pub(super) identity_width: u16,
    pub(super) status_width: u16,
    pub(super) stats_width: u16,
    pub(super) stats: AgentStatsColumns,
}

impl AgentColumns {
    pub(super) fn new<'a>(
        agents: impl IntoIterator<Item = &'a model::AgentInfo>,
        width: u16,
        stats: AgentStatsColumns,
        plane: TreePlane,
    ) -> Self {
        // Identity space that keeps the deepest agent's name and target legible.
        let minimum = agents.into_iter().map(|agent| {
            plane.indent(agent, width)
                + 16.max(model::target_suffix(&agent.target).width() as u16 + 8)
        });
        let minimum = minimum.max().unwrap_or(16);
        match plane {
            TreePlane::Band => Self::fit(width, minimum, stats),
            TreePlane::Column => Self::narrow(width, minimum, stats),
        }
    }

    /// A column has no status label, its glyph's colour carrying the state: statistics
    /// give way leftmost first, so the time stays longest.
    fn narrow(width: u16, minimum_identity_width: u16, mut stats: AgentStatsColumns) -> Self {
        while stats.first < stats.widths.len()
            && usize::from(width)
                < usize::from(stats.width()) + usize::from(minimum_identity_width) + 2
        {
            stats.first += 1;
        }
        let stats_width = stats.width();
        let stats_reserved = if stats_width > 0 { stats_width + 2 } else { 0 };
        Self {
            plane: TreePlane::Column,
            identity_width: width.saturating_sub(stats_reserved),
            status_width: 0,
            stats_width,
            stats,
        }
    }

    /// Reserve identity space, then a fixed-width status column, then
    /// statistics as a group. Hidden columns never consume another row.
    fn fit(width: u16, minimum_identity_width: u16, stats: AgentStatsColumns) -> Self {
        let status_width = if usize::from(width) >= usize::from(minimum_identity_width) + 30 {
            28
        } else {
            0
        };
        let status_reserved = if status_width > 0 {
            status_width + 2
        } else {
            0
        };
        let remaining = width.saturating_sub(status_reserved);
        // Statistics must not reappear after status is hidden, even when their
        // measured width is smaller than the fixed-width status column.
        let stats_width = if status_width > 0
            && usize::from(remaining)
                >= usize::from(stats.width()) + usize::from(minimum_identity_width) + 2
        {
            stats.width()
        } else {
            0
        };
        let stats_reserved = if stats_width > 0 { stats_width + 2 } else { 0 };
        Self {
            plane: TreePlane::Band,
            identity_width: remaining.saturating_sub(stats_reserved),
            status_width,
            stats_width,
            stats,
        }
    }
}

impl AgentStatsColumns {
    /// Columns measured wide enough for their headers.
    pub(super) fn menu<'a>(rows: impl IntoIterator<Item = &'a [String; 4]>) -> Self {
        let mut columns = Self::new(rows);
        for (column, header) in columns.widths.iter_mut().zip(AGENT_STATS_HEADERS) {
            *column = (*column).max(header.width());
        }
        columns.separator = "   ";
        columns
    }

    pub(super) fn new<'a>(rows: impl IntoIterator<Item = &'a [String; 4]>) -> Self {
        let mut widths = [0, 0, 0, AGENT_TIME_WIDTH];
        for row in rows {
            for (width, value) in widths.iter_mut().zip(row) {
                *width = (*width).max(value.width());
            }
        }
        Self {
            widths,
            first: 0,
            separator: SEP,
        }
    }

    pub(super) fn width(&self) -> u16 {
        joined_width(&self.widths[self.first..], self.separator).min(u16::MAX as usize) as u16
    }

    /// A row outside the measured set is never truncated: it pads to the
    /// shared width when it fits and otherwise prints at its own width.
    pub(super) fn format(&self, row: &[String; 4]) -> String {
        let shown = self.first..;
        join_right_aligned(&row[shown.clone()], &self.widths[shown], self.separator)
    }
}

pub(super) const REQUEST_STATS_HEADERS: [&str; 4] =
    ["Output", "Input (uncached)", "Cached", "Time"];

// Request metadata that gives way to statistics: the model clips, then the sent
// time goes. The request number, purpose and status always show.
const SENT: usize = 1;
const MODEL: usize = 3;

/// Metadata fields joined, leaving out the ones given no room.
fn metadata_width(widths: &[usize; 5]) -> usize {
    let shown = widths.iter().filter(|width| **width > 0);
    shown.clone().sum::<usize>() + SEP.width() * shown.count().saturating_sub(1)
}

/// Like agent statistics, request fields use widths measured across the complete
/// list, not the viewport. Metadata is left aligned and numeric fields are right aligned.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct RequestColumns {
    metadata: [usize; 5],
    statistics: [usize; 4],
}

impl Default for RequestColumns {
    fn default() -> Self {
        Self::new(std::iter::empty())
    }
}

impl RequestColumns {
    pub(super) fn new<'a>(rows: impl IntoIterator<Item = &'a model::RequestRow>) -> Self {
        let mut columns = Self {
            statistics: REQUEST_STATS_HEADERS.map(UnicodeWidthStr::width),
            metadata: [0; 5],
        };
        for row in rows {
            for (width, value) in columns.metadata.iter_mut().zip(row.metadata()) {
                *width = (*width).max(value.width());
            }
            for (width, value) in columns.statistics.iter_mut().zip(row.statistics()) {
                *width = (*width).max(value.width());
            }
            // A live row's counter is painted into its empty Time column.
            if row.timing.live() {
                columns.statistics[3] = columns.statistics[3].max(COUNTER_WIDTH.into());
            }
        }
        columns
    }

    pub(super) fn update(&mut self, entries: &[model::Entry], dirty: &mut Vec<usize>) {
        let columns = Self::new(entries.iter().filter_map(|entry| entry.request()));
        if *self != columns {
            *self = columns;
            // A wider token count or elapsed time changes even unchanged rows.
            dirty.extend(
                entries
                    .iter()
                    .enumerate()
                    .filter_map(|(index, entry)| entry.request().map(|_| index)),
            );
        }
    }

    fn statistics_width(&self) -> usize {
        joined_width(&self.statistics, SEP)
    }

    /// Statistics show only beside the metadata that always shows.
    fn show_statistics(&self, width: u16) -> bool {
        let mut essential = self.metadata;
        essential[SENT] = 0;
        essential[MODEL] = 0;
        metadata_width(&essential) + self.statistics_width() + 2
            <= usize::from(width.saturating_sub(2))
    }

    fn metadata_widths(&self, room: usize) -> [usize; 5] {
        let mut widths = self.metadata;
        let over = |widths: &[usize; 5]| metadata_width(widths).saturating_sub(room);
        let mut without_model = widths;
        without_model[MODEL] = 0;
        if over(&without_model) > 0 {
            widths[SENT] = 0;
        }
        widths[MODEL] = widths[MODEL].saturating_sub(over(&widths));
        widths
    }

    fn format_statistics(&self, values: &[String; 4]) -> String {
        join_right_aligned(values, &self.statistics, SEP)
    }

    pub(super) fn header(&self, width: u16) -> Option<Line<'static>> {
        if !self.show_statistics(width) {
            return None;
        }
        Some(Line::from(vec![
            Span::raw(" ".repeat(usize::from(width) - self.statistics_width())),
            Span::styled(
                self.format_statistics(&REQUEST_STATS_HEADERS.map(String::from)),
                Style::default().fg(theme().ui.muted),
            ),
        ]))
    }

    /// Metadata without statistics fits `header_width`, which leaves room for a
    /// live row's painted counter.
    pub(super) fn line(
        &self,
        row: &model::RequestRow,
        width: u16,
        header_width: u16,
    ) -> Line<'static> {
        let show_statistics = self.show_statistics(width);
        // The animation overlay paints at column zero. Reserve its cell and a
        // separator on every row so running/completed requests stay aligned.
        let gutter = width.min(2);
        let width = width - gutter;
        let statistics = self.format_statistics(&row.statistics());
        let left_width = if show_statistics {
            width - statistics.width() as u16 - 2
        } else {
            header_width.saturating_sub(gutter)
        };
        let widths = self.metadata_widths(left_width.into());
        let status = match row.phase {
            RequestPhase::Completed { .. } => theme().ui.success,
            RequestPhase::Failed { .. } => theme().ui.error,
            RequestPhase::Interrupted { .. } | RequestPhase::Retrying { .. } => theme().ui.warning,
            RequestPhase::Requested | RequestPhase::Open { .. } => theme().ui.emphasis,
        };
        let styles = [
            Some(theme().ui.emphasis),
            Some(theme().ui.muted),
            None,
            None,
            Some(status),
        ];
        let mut fields = Vec::new();
        let metadata = row
            .metadata()
            .into_iter()
            .zip(widths.into_iter().zip(styles));
        for (value, (width, fg)) in metadata.filter(|(_, (width, _))| *width > 0) {
            if !fields.is_empty() {
                fields.push(Span::raw(SEP));
            }
            let style = Style {
                fg,
                ..Style::default()
            };
            let value = clipped(Line::from(Span::styled(value, style)), width);
            let padding = " ".repeat(width.saturating_sub(value.width()));
            fields.extend(value.spans);
            fields.push(Span::styled(padding, style));
        }
        let metadata = clipped(Line::from(fields), left_width as usize);
        let gap = width as usize - metadata.width();
        let mut spans = vec![Span::raw(" ".repeat(gutter as usize))];
        spans.extend(metadata.spans);
        if show_statistics {
            spans.push(Span::raw(" ".repeat(gap - statistics.width())));
            spans.push(Span::styled(
                statistics,
                Style::default().fg(theme().ui.muted),
            ));
        }
        Line::from(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::widgets::{Paragraph, Widget};
    use skyhook::session::{RequestNumber, RequestSeq};

    /// Every row formats to the columns' width, right-aligned, however many of its
    /// values are empty; short of room, every row drops the same leftmost columns
    /// and the label keeps its room. A long label does not push columns out while
    /// it keeps that room.
    #[test]
    fn menu_columns_align_rows_and_drop_the_same_leftmost_columns() {
        let owned = |values: &[&str]| -> Vec<String> {
            values.iter().map(|value| value.to_string()).collect()
        };
        let rows = [
            owned(&["0123456789abcdef", "12 events", "plan", "14:02"]),
            owned(&["fedcba", "3 events", "", "Sep 28 09:15"]),
            owned(&["", "", "", "draft"]),
        ];
        let label = "A session title long enough to fill most of a narrow palette";
        let items: Vec<_> = (rows.iter())
            .map(|columns| ItemRef {
                index: 0,
                label,
                detail: "",
                columns,
                emphasis: Emphasis::Normal,
                search: "",
            })
            .collect();
        let mut dropped = Vec::new();
        for width in [100, 60, 40, 20] {
            let layout = MenuColumns::new(&items, width);
            let width = usize::from(width);
            assert!(layout.width() <= width - label.width().min(MENU_LABEL_ROOM).min(width));
            dropped.push(layout.first);
            for row in &rows {
                let text = layout.format(row);
                if layout.width() > 0 {
                    assert_eq!(text.width() + MENU_GAP.width(), layout.width(), "{text:?}");
                    assert!(text.ends_with(row.last().unwrap().as_str()), "{text:?}");
                }
                let (gone, kept) = row.split_at(layout.first);
                assert!(kept.iter().all(|value| text.contains(value.as_str())));
                let gone = gone.iter().filter(|value| !value.is_empty());
                assert!(gone.clone().all(|value| !text.contains(value.as_str())));
            }
        }
        assert!(dropped.is_sorted() && dropped[0] == 0 && dropped[3] == rows[0].len());
    }

    const COMPLETED: RequestPhase = RequestPhase::Completed {
        attempt: 1,
        at: 1_200,
        usage: None,
    };

    fn request(phase: RequestPhase, output: u64) -> model::RequestRow {
        let timing = match phase.settled_at() {
            None => model::Timing::Since(0),
            Some(until) => model::Timing::Took { since: 0, until },
        };
        model::RequestRow {
            sequence: RequestSeq::default(),
            number: RequestNumber::FIRST,
            purpose: skyhook::session::ModelPurpose::Agent,
            model: "model".into(),
            phase,
            usage: skyhook::provider::protocol::Usage::prompted(80, 20, 0, output),
            sent: "14:02:31".into(),
            timing,
        }
    }

    #[test]
    fn agents_palette_stats_align_without_delimiters() {
        let headers = AGENT_STATS_HEADERS.map(String::from);
        let values = [
            "123456789".into(),
            "56789(12345)".into(),
            "54321".into(),
            "1h 02m 03s".into(),
        ];
        let columns = AgentStatsColumns::menu([&values]);
        for row in [&headers, &values] {
            let line = columns.format(row);
            assert_eq!(line.width(), usize::from(columns.width()));
            let mut rest = line.as_str();
            for (value, width) in row.iter().zip(columns.widths) {
                let (field, after) = rest.split_at(width);
                assert_eq!(field.trim_start(), value);
                let gap = after.len().min(columns.separator.len());
                assert!(after[..gap].trim().is_empty());
                rest = &after[gap..];
            }
        }
    }

    /// At every width the columns fill it exactly, keep the identity's minimum beside
    /// anything else, and only grow with it: a band gives statistics up before the
    /// status, and a column, which has no status label, gives statistics up leftmost
    /// first, so the time stays longest.
    #[test]
    fn agent_columns_hide_optional_fields_at_width_boundaries() {
        // The time column keeps room for an hour's counter.
        let values = ["x".repeat(10), "x".repeat(10), "x".repeat(8), "4.2s".into()];
        let stats = || AgentStatsColumns::new([&values]);
        let reserved = |width: u16| if width > 0 { width + 2 } else { 0 };
        let full = stats().width();
        for plane in [TreePlane::Band, TreePlane::Column] {
            let mut last = (0, 0);
            for width in 0..=120 {
                let columns = match plane {
                    TreePlane::Band => AgentColumns::fit(width, 16, stats()),
                    TreePlane::Column => AgentColumns::narrow(width, 16, stats()),
                };
                let (identity, stats, status) = (
                    columns.identity_width,
                    columns.stats_width,
                    columns.status_width,
                );
                assert_eq!(identity + reserved(stats) + reserved(status), width);
                if stats > 0 || status > 0 {
                    assert!(identity >= 16, "width {width}");
                }
                match plane {
                    TreePlane::Band => assert!(stats == 0 || status > 0, "width {width}"),
                    TreePlane::Column => assert_eq!(status, 0),
                }
                assert!(stats >= last.0 && status >= last.1, "width {width}");
                last = (stats, status);
            }
            assert_eq!(last.0, full, "the widest shows every statistic");
        }
        let narrow = AgentColumns::narrow(25, 16, stats());
        assert!(narrow.stats_width > 0 && narrow.stats_width < full);
        assert_eq!(narrow.stats.first, 3, "the time stays longest");
    }

    #[test]
    fn request_rows_reserve_spinner_gutter_and_align_with_statistics_headers() {
        let running = request(RequestPhase::Requested, 84);
        let completed = request(COMPLETED, 84);
        let columns = RequestColumns::new([&running, &completed]);
        // The running row's counter is painted into its Time column.
        assert_eq!(columns.statistics[3], usize::from(COUNTER_WIDTH));
        let header = columns.header(100).unwrap().to_string();
        for row in [&running, &completed] {
            let line = columns.line(row, 100, 100);
            let text = line.to_string();
            let sent = format!("  {}{SEP}{}", row.number, row.sent);
            assert!(text.starts_with(&sent), "{text}");
            assert!(text.ends_with(&columns.format_statistics(&row.statistics())));
            assert_eq!(header.width(), text.width());
            for (label, value) in REQUEST_STATS_HEADERS.iter().zip(row.statistics()) {
                let header_end = header.find(label).unwrap() + label.len();
                let value_end = text.rfind(&value).unwrap() + value.len();
                let aligned = header[..header_end].width() == text[..value_end].width();
                assert!(aligned, "{label} is not aligned");
                assert!(!text.contains(label));
            }
            let area = Rect::new(0, 0, 100, 1);
            let mut buffer = Buffer::empty(area);
            Paragraph::new(line).render(area, &mut buffer);
            // This is the same overlay column used by the animated requests view.
            buffer[(0, 0)].set_symbol(spinner(0));
            let gutter: Vec<_> = (0..4).map(|x| buffer[(x, 0)].symbol()).collect();
            assert_eq!(gutter, [spinner(0), " ", "1", " "]);
        }
        // On a 70-column terminal a live counter leaves statistics no room beside
        // the purpose and status, so they give way instead.
        let entry = model::Entry::request_entry(running.clone());
        let width = EntryGeometry::new(&entry, 70).body_width;
        assert!(columns.header(width).is_none());
        let header_width = width - COUNTER_WIDTH - 1;
        for (row, status) in [(&running, "Running"), (&completed, "Completed")] {
            let text = columns.line(row, width, header_width).to_string();
            assert!(text.contains("Agent") && text.contains(status), "{text}");
        }

        let row = model::RequestRow {
            sequence: RequestSeq::default(),
            number: RequestNumber::FIRST,
            purpose: skyhook::session::ModelPurpose::Compaction,
            model: "long model 界界 😀".into(),
            phase: RequestPhase::Requested,
            usage: Default::default(),
            sent: "Sep 28 23:59:10".into(),
            timing: model::Timing::Since(0),
        };
        let columns = RequestColumns::new([&row]);
        let mut shown = Vec::new();
        for width in [0u16, 1, 2, 3, 8, 20, 45, 46, 60, 81, 82, 86, 87, 99, 120] {
            let header_width = width.saturating_sub(COUNTER_WIDTH + 1);
            let line = columns.line(&row, width, header_width);
            assert!(line.width() <= width as usize, "overflow at width {width}");
            let text = line.to_string();
            assert!(text.starts_with(&" ".repeat(width.min(2) as usize)));
            let header = columns.header(width);
            assert_eq!(header.is_some(), text.contains('—'));
            if header.is_none() {
                assert!(text.trim_end().width() <= usize::from(header_width));
            }
            if let Some(header) = header {
                assert_eq!(header.width(), usize::from(width));
                assert!(header.to_string().contains("Input (uncached)"));
                assert!(text.ends_with(&columns.format_statistics(&row.statistics())));
                // The sent time gives way before the purpose and status do.
                assert!(text.contains("Compaction · ") && text.contains(" · Running"));
                shown.push((width, text.contains(&row.sent)));
            }
        }
        // A header fits on a wide enough row, and the sent time, giving way first, only
        // on a wider one.
        let sent = shown.iter().position(|(_, sent)| *sent).unwrap();
        assert!(sent > 0 && shown[sent..].iter().all(|(_, sent)| *sent));
    }

    #[test]
    fn request_width_growth_shrink_and_reordering_fan_out_only_when_needed() {
        let entries = |rows: &[&model::RequestRow]| {
            let rows = rows.iter();
            rows.map(|row| model::Entry::request_entry((*row).clone()))
                .collect::<Vec<_>>()
        };
        let short = request(COMPLETED, 1);
        let mut long = request(COMPLETED, u64::MAX);
        long.model = "a very long model name".into();
        let small = entries(&[&short, &short]);
        let large = entries(&[&short, &long]);
        let reordered = entries(&[&long, &short]);
        let mut columns = RequestColumns::new(small.iter().filter_map(|entry| entry.request()));
        let mut dirty = Vec::new();
        for (entries, expected) in [(&large, &[0, 1][..]), (&reordered, &[]), (&small, &[0, 1])] {
            columns.update(entries, &mut dirty);
            assert_eq!(dirty, expected);
            dirty.clear();
        }
    }
}
