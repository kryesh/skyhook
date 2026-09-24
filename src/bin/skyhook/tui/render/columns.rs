//! Shared agent and request column measurement and formatting.

use super::*;

/// Numeric fields share their own right edge, not just the footer's right edge.
/// Measure all agents, including off-screen rows, to keep columns stable on scroll.
#[derive(Clone, Copy)]
pub(super) struct AgentStatsColumns([usize; 3]);

pub(super) const AGENT_STATS_HEADERS: [&str; 3] = ["Output", "Input (uncached)", "Context"];

/// Shared visibility policy: reserve identity space, then a fixed-width status
/// column, then statistics as a group. Hidden columns never consume another row.
pub(super) struct AgentColumnsLayout {
    pub(super) identity_width: u16,
    pub(super) status_width: u16,
    pub(super) stats_width: u16,
}

fn right_aligned(value: &str, width: usize) -> String {
    format!("{}{value}", " ".repeat(width.saturating_sub(value.width())))
}

fn join_right_aligned(values: &[String], widths: &[usize]) -> String {
    let values = values.iter().zip(widths);
    let values = values.map(|(value, width)| right_aligned(value, *width));
    values.collect::<Vec<_>>().join(" · ")
}

impl AgentColumnsLayout {
    /// Identity space that keeps the deepest agent's name and target legible.
    pub(super) fn minimum_identity_width(agents: &[model::AgentInfo], width: u16) -> u16 {
        let widths = agents.iter().map(|agent| {
            let indent = (agent.id.depth() as u16 * 4).min(width / 3);
            indent + 16.max(model::target_suffix(&agent.target).width() as u16 + 8)
        });
        widths.max().unwrap_or(16)
    }

    pub(super) fn new(width: u16, minimum_identity_width: u16, stats_width: u16) -> Self {
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
                >= usize::from(stats_width) + usize::from(minimum_identity_width) + 2
        {
            stats_width
        } else {
            0
        };
        let stats_reserved = if stats_width > 0 { stats_width + 2 } else { 0 };
        Self {
            identity_width: remaining.saturating_sub(stats_reserved),
            status_width,
            stats_width,
        }
    }
}

impl AgentStatsColumns {
    pub(super) fn menu<'a>(rows: impl IntoIterator<Item = &'a [String; 3]>) -> Self {
        let mut columns = Self::new(rows);
        for (column, header) in columns.0.iter_mut().zip(AGENT_STATS_HEADERS) {
            *column = (*column).max(header.width());
        }
        columns
    }

    pub(super) fn draw(
        &self,
        frame: &mut Frame,
        rect: Rect,
        row: &[String; 3],
        fg: Color,
        bg: Color,
    ) {
        if rect.height == 0 {
            return;
        }
        let mut x = rect.x;
        for (value, width) in row.iter().zip(self.0) {
            text(
                frame,
                r(
                    x,
                    rect.y,
                    width.min(usize::from(rect.right().saturating_sub(x))) as u16,
                    1,
                ),
                right_aligned(value, width),
                fg,
                bg,
            );
            x = x
                .saturating_add(width.min(u16::MAX as usize) as u16)
                .saturating_add(3);
        }
    }

    pub(super) fn new<'a>(rows: impl IntoIterator<Item = &'a [String; 3]>) -> Self {
        let mut widths = [0; 3];
        for row in rows {
            for (width, value) in widths.iter_mut().zip(row) {
                *width = (*width).max(value.width());
            }
        }
        Self(widths)
    }

    pub(super) fn width(&self) -> u16 {
        (self.0.iter().sum::<usize>() + 6).min(u16::MAX as usize) as u16
    }

    /// A row outside the measured set is never truncated: it pads to the
    /// shared width when it fits and otherwise prints at its own width.
    pub(super) fn format(&self, row: &[String; 3]) -> String {
        join_right_aligned(row, &self.0)
    }
}

pub(super) const REQUEST_STATS_HEADERS: [&str; 4] =
    ["Output", "Input (uncached)", "Cached", "Time"];

/// Like agent statistics, request fields use widths measured across the complete
/// list, not the viewport. Metadata is left aligned and numeric fields are right aligned.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct RequestColumns {
    metadata: [usize; 4],
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
            metadata: [0; 4],
        };
        for row in rows {
            for (width, value) in columns.metadata.iter_mut().zip(row.metadata()) {
                *width = (*width).max(value.width());
            }
            for (width, value) in columns.statistics.iter_mut().zip(row.statistics()) {
                *width = (*width).max(value.width());
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
        self.statistics.iter().sum::<usize>() + 9
    }

    fn show_statistics(&self, width: u16) -> bool {
        self.statistics_width() + 18 <= usize::from(width.saturating_sub(2))
    }

    fn format_statistics(&self, values: &[String; 4]) -> String {
        join_right_aligned(values, &self.statistics)
    }

    pub(super) fn header(&self, width: u16, p: Palette) -> Option<Line<'static>> {
        if !self.show_statistics(width) {
            return None;
        }
        Some(Line::from(vec![
            Span::raw(" ".repeat(usize::from(width) - self.statistics_width())),
            Span::styled(
                self.format_statistics(&REQUEST_STATS_HEADERS.map(String::from)),
                Style::default().fg(p.content.muted),
            ),
        ]))
    }

    pub(super) fn line(&self, row: &model::RequestRow, width: u16, p: Palette) -> Line<'static> {
        let show_statistics = self.show_statistics(width);
        // The animation overlay paints at column zero. Reserve its cell and a
        // separator on every row so running/completed requests stay aligned.
        let gutter = width.min(2);
        let width = width - gutter;
        let statistics = self.format_statistics(&row.statistics());
        let left_width = if show_statistics {
            width - statistics.width() as u16 - 2
        } else {
            width
        };
        let mut widths = self.metadata;
        // Clip the model column first so IDs, purpose and state remain visible.
        let excess = (widths.iter().sum::<usize>() + 9).saturating_sub(left_width as usize);
        widths[2] = widths[2].saturating_sub(excess);
        let metadata = row
            .metadata()
            .iter()
            .zip(widths)
            .map(|(value, width)| {
                let value = clipped_header(value, width.min(u16::MAX as usize) as u16);
                format!("{value}{}", " ".repeat(width - value.width()))
            })
            .collect::<Vec<_>>();
        let status_start = metadata[..3].iter().map(String::len).sum::<usize>() + " · ".len() * 3;
        let title_end = metadata[0].len();
        let metadata = clipped_header(&metadata.join(" · "), left_width);
        let gap = width as usize - metadata.width();
        let mut spans = vec![Span::raw(" ".repeat(gutter as usize))];
        if metadata.is_char_boundary(title_end) && title_end <= metadata.len() {
            spans.push(Span::styled(
                metadata[..title_end].to_owned(),
                Style::default().fg(p.content.primary),
            ));
            if metadata.is_char_boundary(status_start) && status_start <= metadata.len() {
                spans.push(Span::raw(metadata[title_end..status_start].to_owned()));
                let color: Color = match row.status {
                    model::RequestStatus::Completed => p.content.success,
                    model::RequestStatus::Failed => p.content.error,
                    model::RequestStatus::Interrupted | model::RequestStatus::Retrying => {
                        p.content.warning
                    }
                    model::RequestStatus::Running => p.content.info,
                };
                spans.push(Span::styled(
                    metadata[status_start..].to_owned(),
                    Style::default().fg(color),
                ));
            } else {
                spans.push(Span::raw(metadata[title_end..].to_owned()));
            }
        } else {
            spans.push(Span::styled(
                metadata,
                Style::default().fg(p.content.primary),
            ));
        }
        if show_statistics {
            spans.push(Span::raw(" ".repeat(gap - statistics.width())));
            spans.push(Span::styled(
                statistics,
                Style::default().fg(p.content.muted),
            ));
        }
        Line::from(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::widgets::Widget;
    use skyhook::session::RequestSeq;

    fn request(status: model::RequestStatus, output: u64) -> model::RequestRow {
        model::RequestRow {
            sequence: RequestSeq::default(),
            purpose: skyhook::session::ModelPurpose::Agent,
            model: "model".into(),
            status,
            usage: Some(skyhook::provider::protocol::Usage {
                input_tokens: 80,
                cached_input_tokens: 20,
                output_tokens: output,
            }),
            elapsed_tenths: Some(12),
        }
    }

    #[test]
    fn agents_palette_stats_align_without_delimiters_and_narrow_viewports_are_safe() {
        let headers = AGENT_STATS_HEADERS.map(String::from);
        let values = ["123456789".into(), "56789(12345)".into(), "54321".into()];
        let columns = AgentStatsColumns::menu([&values]);
        let backend = ratatui::backend::TestBackend::new(columns.width(), 2);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let p = Palette::new();
        let rows = [&headers, &values];
        terminal
            .draw(|frame| {
                for (y, row) in rows.iter().enumerate() {
                    let area = r(0, y as u16, columns.width(), 1);
                    columns.draw(frame, area, row, p.muted, p.input);
                }
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        for (y, row) in rows.iter().enumerate() {
            let symbol = |x| buffer[(x, y as u16)].symbol();
            let mut x = 0;
            for (index, width) in columns.0.iter().enumerate() {
                let end = x + *width as u16;
                assert_eq!(
                    (x..end).map(symbol).collect::<String>().trim_start(),
                    row[index]
                );
                if index < 2 {
                    assert!((end..end + 3).all(|x| symbol(x) == " "));
                }
                x = end + 3;
            }
        }
        let small = ["1".into(), "2".into(), "3".into()];
        let large = ["123456789".into(), "界界界".into(), "1000% (10k/1k)".into()];
        for width in [0, 1, 2, 20, 60] {
            let columns = AgentStatsColumns::new([&small]);
            let backend = ratatui::backend::TestBackend::new(width.max(1), 1);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            let area = r(0, 0, width, 1);
            terminal
                .draw(|frame| columns.draw(frame, area, &large, p.fg, p.base))
                .unwrap();
        }
    }

    #[test]
    fn agent_columns_hide_optional_fields_at_width_boundaries() {
        for (width, identity, stats, status) in [
            (0, 0, 0, 0),
            (45, 45, 0, 0),
            (46, 16, 0, 28),
            (51, 21, 0, 28),
            (52, 22, 0, 28),
            (81, 51, 0, 28),
            (82, 16, 34, 28),
        ] {
            let columns = AgentColumnsLayout::new(width, 16, 34);
            let actual = (
                columns.identity_width,
                columns.stats_width,
                columns.status_width,
            );
            assert_eq!(actual, (identity, stats, status), "width {width}");
        }
    }

    #[test]
    fn request_rows_reserve_spinner_gutter_and_align_with_statistics_headers() {
        let running = request(model::RequestStatus::Running, 84);
        let completed = request(model::RequestStatus::Completed, 84);
        let columns = RequestColumns::new([&running, &completed]);
        let header = columns.header(100, Palette::new()).unwrap().to_string();
        for row in [&running, &completed] {
            let line = columns.line(row, 100, Palette::new());
            let text = line.to_string();
            assert!(text.starts_with(&format!("  Request #{}", row.sequence)));
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
            assert_eq!(gutter, [spinner(0), " ", "R", "e"]);
        }

        let row = model::RequestRow {
            sequence: RequestSeq::default(),
            purpose: skyhook::session::ModelPurpose::Compaction,
            model: "long model 界界 😀".into(),
            status: model::RequestStatus::Running,
            usage: None,
            elapsed_tenths: None,
        };
        let columns = RequestColumns::new([&row]);
        for width in [0, 1, 2, 3, 8, 20, 45, 46, 60, 81, 82, 99] {
            let line = columns.line(&row, width, Palette::new());
            assert!(line.width() <= width as usize, "overflow at width {width}");
            let text = line.to_string();
            assert!(text.starts_with(&" ".repeat(width.min(2) as usize)));
            let header = columns.header(width, Palette::new());
            assert_eq!(header.is_some(), text.contains('—'));
            if let Some(header) = header {
                assert_eq!(header.width(), usize::from(width));
                assert!(header.to_string().contains("Input (uncached)"));
                assert!(text.ends_with(&columns.format_statistics(&row.statistics())));
            }
        }
    }

    #[test]
    fn request_width_growth_shrink_and_reordering_fan_out_only_when_needed() {
        let entries = |rows: &[&model::RequestRow]| {
            let rows = rows.iter();
            rows.map(|row| model::Entry::request_entry((*row).clone()))
                .collect::<Vec<_>>()
        };
        let short = request(model::RequestStatus::Completed, 1);
        let mut long = request(model::RequestStatus::Completed, u64::MAX);
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
