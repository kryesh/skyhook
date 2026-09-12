//! Shared agent and request column measurement and formatting.

use super::*;

/// Numeric fields share their own right edge, not just the footer's right edge.
/// Measure all agents, including off-screen rows, to keep columns stable on scroll.
pub(super) struct AgentStatsColumns([usize; 3]);

pub(super) const AGENT_STATS_HEADERS: [&str; 3] = ["Output", "Input (uncached)", "Context"];

/// Shared visibility policy: reserve identity space, then a fixed-width status
/// column, then statistics as a group. Hidden columns never consume another row.
pub(super) struct AgentColumnsLayout {
    pub(super) identity_width: u16,
    pub(super) status_width: u16,
    pub(super) stats_width: u16,
}

impl AgentColumnsLayout {
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
                r(x, rect.y, width as u16, 1),
                format!("{}{value}", " ".repeat(width.saturating_sub(value.width()))),
                fg,
                bg,
            );
            x += width as u16 + 3;
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
        (self.0.iter().sum::<usize>() + 6) as u16
    }

    pub(super) fn format(&self, row: &[String; 3]) -> String {
        row.iter()
            .zip(self.0)
            .map(|(value, width)| format!("{}{value}", " ".repeat(width - value.width())))
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

pub(super) const REQUEST_STATS_HEADERS: [&str; 4] = [
    AGENT_STATS_HEADERS[0],
    AGENT_STATS_HEADERS[1],
    "Cached",
    "Time",
];

/// Like agent statistics, request fields use widths measured across the complete
/// list, not the viewport. Metadata is left aligned and numeric fields are right aligned.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct RequestColumns {
    pub(super) metadata: [usize; 4],
    pub(super) statistics: [usize; 4],
}

impl RequestColumns {
    pub(super) fn new<'a>(rows: impl IntoIterator<Item = &'a model::RequestRow>) -> Self {
        let mut columns = Self {
            statistics: REQUEST_STATS_HEADERS.map(UnicodeWidthStr::width),
            ..Self::default()
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
        let columns = Self::new(entries.iter().filter_map(|entry| entry.request.as_ref()));
        if *self != columns {
            *self = columns;
            // A wider token count or elapsed time changes even unchanged rows.
            dirty.extend(
                entries
                    .iter()
                    .enumerate()
                    .filter_map(|(index, entry)| entry.request.as_ref().map(|_| index)),
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
        values
            .iter()
            .zip(self.statistics)
            .map(|(value, width)| format!("{}{value}", " ".repeat(width - value.width())))
            .collect::<Vec<_>>()
            .join(" · ")
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
                let color = match row.status {
                    "Completed" => p.content.success,
                    "Failed" => p.content.error,
                    "Interrupted" => p.content.warning,
                    "Running" => p.content.info,
                    _ => p.content.muted,
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

    #[test]
    fn agents_palette_stats_align_without_delimiters() {
        let headers = AGENT_STATS_HEADERS.map(String::from);
        let values = ["123456789".into(), "56789(12345)".into(), "54321".into()];
        let columns = AgentStatsColumns::menu([&values]);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(columns.width(), 2)).unwrap();
        let p = Palette::new(false);
        terminal
            .draw(|frame| {
                for (y, row) in [&headers, &values].iter().enumerate() {
                    columns.draw(
                        frame,
                        r(0, y as u16, columns.width(), 1),
                        row,
                        p.muted,
                        p.input,
                    );
                }
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        for (y, row) in [&headers, &values].iter().enumerate() {
            let mut x = 0;
            for (index, width) in columns.0.iter().enumerate() {
                let end = x + *width as u16;
                let actual = (x..end)
                    .map(|x| buffer[(x, y as u16)].symbol())
                    .collect::<String>();
                assert_eq!(actual.trim_start(), row[index]);
                if index < 2 {
                    assert!((end..end + 3).all(|x| buffer[(x, y as u16)].symbol() == " "));
                }
                x = end + 3;
            }
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
            assert_eq!(columns.identity_width, identity);
            assert_eq!(columns.stats_width, stats);
            assert_eq!(columns.status_width, status);
        }
    }

    #[test]
    fn request_rows_reserve_spinner_gutter_and_align_with_statistics_headers() {
        let running = model::RequestRow {
            sequence: 7,
            purpose: skyhook::session::ModelPurpose::Agent,
            model: "qwen".into(),
            status: "Running",
            usage: Some(skyhook::provider::protocol::Usage {
                input_tokens: 80,
                cached_input_tokens: 20,
                output_tokens: 84,
            }),
            elapsed_tenths: Some(12),
        };
        let mut completed = running.clone();
        completed.sequence = 123;
        completed.status = "Completed";
        let columns = RequestColumns::new([&running, &completed]);
        for row in [&running, &completed] {
            let line = columns.line(row, 100, Palette::new(false));
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            assert!(text.starts_with(&format!("  Request #{}", row.sequence)));
            assert!(text.ends_with(&columns.format_statistics(&row.statistics())));
            let header = columns
                .header(100, Palette::new(false))
                .unwrap()
                .to_string();
            assert_eq!(header.width(), text.width());
            for (label, value) in REQUEST_STATS_HEADERS.iter().zip(row.statistics()) {
                let header_end = header.find(label).unwrap() + label.len();
                let value_end = text.rfind(&value).unwrap() + value.len();
                assert_eq!(
                    header[..header_end].width(),
                    text[..value_end].width(),
                    "{label} is not aligned"
                );
            }
            for label in REQUEST_STATS_HEADERS {
                assert!(!text.contains(label));
            }
            let area = Rect::new(0, 0, 100, 1);
            let mut buffer = Buffer::empty(area);
            Paragraph::new(line).render(area, &mut buffer);
            // This is the same overlay column used by the animated requests view.
            buffer[(0, 0)].set_symbol(spinner(0));
            assert_eq!(buffer[(0, 0)].symbol(), spinner(0));
            assert_eq!(buffer[(1, 0)].symbol(), " ");
            assert_eq!(buffer[(2, 0)].symbol(), "R");
            assert_eq!(buffer[(3, 0)].symbol(), "e");
        }
    }

    #[test]
    fn request_rows_keep_the_gutter_within_narrow_viewports() {
        let row = model::RequestRow {
            sequence: 12345,
            purpose: skyhook::session::ModelPurpose::Compaction,
            model: "long model 界界 😀".into(),
            status: "Running",
            usage: None,
            elapsed_tenths: None,
        };
        let columns = RequestColumns::new([&row]);
        for width in 0..100 {
            let line = columns.line(&row, width, Palette::new(false));
            assert!(line.width() <= width as usize, "overflow at width {width}");
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            assert!(text.starts_with(&" ".repeat(width.min(2) as usize)));
            let header = columns.header(width, Palette::new(false));
            assert_eq!(header.is_some(), text.contains('—'));
            if let Some(header) = header {
                assert_eq!(header.width(), usize::from(width));
                assert!(header.to_string().contains("Input (uncached)"));
                assert!(text.ends_with(&columns.format_statistics(&row.statistics())));
            }
        }
    }
}
