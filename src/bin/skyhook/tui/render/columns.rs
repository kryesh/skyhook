//! Shared agent and request column measurement and formatting.

use super::*;

/// Numeric fields share their own right edge, not just the footer's right edge.
/// Measure all agents, including off-screen rows, to keep columns stable on scroll.
pub(super) struct AgentStatsColumns([usize; 3]);

pub(super) const AGENT_STATS_HEADERS: [&str; 3] = ["Output", "Input (uncached)", "Context"];

impl AgentStatsColumns {
    pub(super) fn menu<'a>(rows: impl IntoIterator<Item = &'a [String; 3]>, width: u16) -> Self {
        let mut columns = Self::new(rows);
        for (column, header) in columns.0.iter_mut().zip(AGENT_STATS_HEADERS) {
            *column = (*column).max(header.width());
        }
        // Keep all three columns visible on narrow terminals. Headers and values
        // wrap within the same columns instead of clipping away token fields.
        let available = usize::from(width.saturating_sub(6));
        while columns.0.iter().sum::<usize>() > available {
            let largest = (0..3).max_by_key(|&index| columns.0[index]).unwrap();
            columns.0[largest] = columns.0[largest].saturating_sub(1);
        }
        columns
    }

    pub(super) fn wrapped(value: &str, width: usize) -> Vec<String> {
        wrap_words(Line::from(value.to_owned()), width.max(1))
            .into_iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect()
    }

    pub(super) fn wrapped_height(&self, row: &[String; 3]) -> usize {
        row.iter()
            .zip(self.0)
            .map(|(value, width)| Self::wrapped(value, width).len())
            .max()
            .unwrap_or(1)
    }

    pub(super) fn draw(
        &self,
        frame: &mut Frame,
        rect: Rect,
        row: &[String; 3],
        fg: Color,
        bg: Color,
    ) {
        let mut x = rect.x;
        for (value, width) in row.iter().zip(self.0) {
            for (line, value) in Self::wrapped(value, width).into_iter().enumerate() {
                if line >= usize::from(rect.height) {
                    break;
                }
                text(
                    frame,
                    r(x, rect.y + line as u16, width as u16, 1),
                    format!("{}{value}", " ".repeat(width.saturating_sub(value.width()))),
                    fg,
                    bg,
                );
            }
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

/// Like agent statistics, request fields use widths measured across the complete
/// list, not the viewport. Metadata is left aligned and numeric fields are right aligned.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct RequestColumns {
    pub(super) metadata: [usize; 4],
    pub(super) statistics: [usize; 4],
}

impl RequestColumns {
    pub(super) fn new<'a>(rows: impl IntoIterator<Item = &'a model::RequestRow>) -> Self {
        let mut columns = Self::default();
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

    pub(super) fn line(&self, row: &model::RequestRow, width: u16, p: Palette) -> Line<'static> {
        // The animation overlay paints at column zero. Reserve its cell and a
        // separator on every row so running/completed requests stay aligned.
        let gutter = width.min(2);
        let width = width - gutter;
        let statistics = row
            .statistics()
            .iter()
            .zip(self.statistics)
            .map(|(value, width)| format!("{}{value}", " ".repeat(width - value.width())))
            .collect::<Vec<_>>()
            .join(" · ");
        let show_statistics = statistics.width() + 18 <= width as usize;
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
    fn agents_palette_stats_headers_align_and_wrap_with_their_values() {
        let headers = AGENT_STATS_HEADERS.map(String::from);
        let values = ["12345".into(), "56789(12345)".into(), "54321".into()];
        for width in [100, 44, 30, 20] {
            let columns = AgentStatsColumns::menu([&values], width);
            assert!(columns.width() <= width);
            let header_height = columns.wrapped_height(&headers) as u16;
            let value_height = columns.wrapped_height(&values) as u16;
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(
                width,
                header_height + value_height,
            ))
            .unwrap();
            let p = Palette::new(false);
            terminal
                .draw(|frame| {
                    columns.draw(
                        frame,
                        r(0, 0, width, header_height),
                        &headers,
                        p.muted,
                        p.input,
                    );
                    columns.draw(
                        frame,
                        r(0, header_height, width, value_height),
                        &values,
                        p.muted,
                        p.input,
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let mut x = 0;
            for (index, column_width) in columns.0.iter().enumerate() {
                let read_column = |y, height| {
                    (y..y + height)
                        .map(|y| {
                            (x..x + *column_width as u16)
                                .map(|x| buffer[(x, y)].symbol())
                                .collect::<String>()
                                .trim()
                                .to_owned()
                        })
                        .collect::<String>()
                };
                assert_eq!(
                    read_column(0, header_height).replace(' ', ""),
                    headers[index].replace(' ', "")
                );
                assert_eq!(read_column(header_height, value_height), values[index]);
                x += *column_width as u16 + 3;
            }
            if width >= 44 {
                assert_eq!(header_height, 1);
                assert_eq!(value_height, 1);
                assert!(
                    columns
                        .0
                        .iter()
                        .zip(&headers)
                        .all(|(width, header)| *width >= header.width())
                );
            }
        }
        assert_eq!(
            AgentStatsColumns::wrapped("Input (uncached)", 10),
            ["Input", "(uncached)"]
        );
    }

    #[test]
    fn request_rows_reserve_spinner_gutter_and_show_unlabelled_statistics() {
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
            assert!(text.ends_with("84 · 80 · 20 · 1.2s"));
            for label in ["Out ", "In ", "Cached ", "Time "] {
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
            if width == 99 {
                assert!(text.ends_with("— · — · — · —"));
            }
        }
    }
}
