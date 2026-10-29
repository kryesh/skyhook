//! Text shaping shared by every host.
use std::fmt::Write as _;
use unicode_width::UnicodeWidthStr;

use crate::page::{Block, Page, StageState};

/// Rows under a header; columns from `numeric_from` on align right.
pub struct Table<'a> {
    pub header: &'a [&'a str],
    pub numeric_from: usize,
    pub rows: Vec<Vec<String>>,
}

impl Table<'_> {
    pub fn markdown(&self, output: &mut String) {
        let _ = writeln!(output, "| {} |", self.header.join(" | "));
        let rule: Vec<_> = (0..self.header.len())
            .map(|column| {
                if column < self.numeric_from {
                    "---"
                } else {
                    "---:"
                }
            })
            .collect();
        let _ = writeln!(output, "| {} |", rule.join(" | "));
        for row in &self.rows {
            let cells: Vec<_> = row.iter().map(|cell| cell.replace('|', "\\|")).collect();
            let _ = writeln!(output, "| {} |", cells.join(" | "));
        }
    }

    /// Aligned plain text with a rule under the header, for a terminal: control
    /// characters in cells are escaped.
    pub fn render(&self, output: &mut String) {
        let rows: Vec<Vec<String>> = (self.rows.iter())
            .map(|row| {
                row.iter()
                    .map(skyhook::tool::diagnostic::escape_controls)
                    .collect()
            })
            .collect();
        let mut widths = Vec::new();
        widen(&mut widths, self.header.iter().copied());
        for row in &rows {
            widen(&mut widths, row.iter().map(String::as_str));
        }
        let mut line = |cells: &[&str]| {
            let mut line = String::new();
            for (column, (cell, width)) in cells.iter().zip(&widths).enumerate() {
                let pad = " ".repeat(width - cell.width());
                let (before, after) = if column < self.numeric_from {
                    ("", pad.as_str())
                } else {
                    (pad.as_str(), "")
                };
                let gap = if column > 0 { "  " } else { "" };
                let _ = write!(line, "{gap}{before}{cell}{after}");
            }
            let _ = writeln!(output, "{}", line.trim_end());
        };
        line(self.header);
        let rule: Vec<_> = widths.iter().map(|width| "─".repeat(*width)).collect();
        line(&rule.iter().map(String::as_str).collect::<Vec<_>>());
        for row in &rows {
            line(&row.iter().map(String::as_str).collect::<Vec<_>>());
        }
    }
}

impl Page {
    /// Plain text for a terminal: facts and stages on a line each, every panel a [`Table`]
    /// headed by its title and every section one under its title. A section lists the records it supplies, without
    /// their detail.
    pub fn render(&self, output: &mut String) {
        let line = |output: &mut String, line: String| {
            let _ = writeln!(
                output,
                "{}",
                skyhook::tool::diagnostic::escape_controls(line)
            );
        };
        for block in &self.blocks {
            match block {
                Block::Facts(facts) => {
                    let facts: Vec<_> = (facts.iter())
                        .map(|fact| format!("{} {}", fact.label, fact.value.text))
                        .collect();
                    line(output, facts.join(" · "));
                }
                Block::Pipeline(stages) => {
                    let stages: Vec<_> = (stages.iter())
                        .map(|stage| {
                            let state = match stage.state {
                                StageState::Done => "done",
                                StageState::Active => "active",
                                StageState::Pending => "pending",
                                StageState::Blocked => "blocked",
                            };
                            format!("{} {} ({state})", stage.label, stage.value)
                        })
                        .collect();
                    line(output, stages.join(" ── "));
                }
                Block::Panels(panels) => {
                    for panel in panels {
                        output.push('\n');
                        let header: Vec<_> = (std::iter::once(panel.title.as_str()))
                            .chain(panel.columns.iter().map(String::as_str))
                            .collect();
                        let rows = (panel.rows.iter())
                            .map(|row| {
                                let values = row.values.iter().map(|value| value.text.clone());
                                std::iter::once(row.label.clone()).chain(values).collect()
                            })
                            .collect();
                        Table {
                            header: &header,
                            numeric_from: 1,
                            rows,
                        }
                        .render(output);
                    }
                }
                Block::Section(section) => {
                    let summary = (section.summary.as_ref())
                        .map_or_else(String::new, |summary| format!(" · {summary}"));
                    output.push('\n');
                    line(
                        output,
                        format!("{} {}{summary}", section.title, section.total),
                    );
                    if section.records.is_empty() {
                        continue;
                    }
                    let header: Vec<_> = section.columns.iter().map(|c| c.title.as_str()).collect();
                    let rows = (section.records.iter())
                        .map(|record| record.cells.iter().map(|cell| cell.text.clone()).collect())
                        .collect();
                    Table {
                        header: &header,
                        numeric_from: header.len(),
                        rows,
                    }
                    .render(output);
                    let shown = section.records.len() as u64;
                    if shown < section.total {
                        line(output, format!("… {shown} of {}", section.total));
                    }
                }
            }
        }
    }
}

/// Grow `widths` to fit one more row's cells, adding columns the row introduces.
pub fn widen<'a>(widths: &mut Vec<usize>, row: impl IntoIterator<Item = &'a str>) {
    for (column, cell) in row.into_iter().enumerate() {
        if column == widths.len() {
            widths.push(0);
        }
        widths[column] = widths[column].max(cell.width());
    }
}

/// Normalize only as much text as the preview needs, without allocating a full
/// normalized copy. Whitespace-only tails still have to be inspected to preserve
/// the distinction between an exact fit and a truncated preview.
pub fn brief(value: &str, limit: usize) -> String {
    let mut text = String::with_capacity(value.len().min(limit.saturating_add(3)));
    let mut remaining = limit;
    let mut whitespace = false;
    let mut started = false;
    for ch in value.chars() {
        if ch.is_whitespace() {
            whitespace |= started;
            continue;
        }
        if whitespace {
            if remaining == 0 {
                text.push('…');
                return text;
            }
            text.push(' ');
            remaining -= 1;
            whitespace = false;
        }
        if remaining == 0 {
            text.push('…');
            return text;
        }
        text.push(ch);
        remaining -= 1;
        started = true;
    }
    text
}

/// `n` in full, its thousands separated by commas.
pub fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// A span for people: tenths of a second under a minute, whole seconds above.
/// A negative span, from clocks disagreeing, reads as zero.
pub fn duration(span: chrono::TimeDelta) -> String {
    let tenths = span.num_milliseconds().max(0) / 100;
    let seconds = tenths / 10;
    let (hours, minutes, seconds) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}.{}s", tenths % 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    #[test]
    fn numbers_group_their_thousands() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(10_816_684), "10,816,684");
    }

    #[test]
    fn durations_format_for_people() {
        for (millis, expected) in [
            (-5, "0.0s"),
            (99, "0.0s"),
            (5_260, "5.2s"),
            (65_900, "1m 05s"),
            (3_725_000, "1h 02m 05s"),
        ] {
            assert_eq!(duration(TimeDelta::milliseconds(millis)), expected);
        }
    }
}
