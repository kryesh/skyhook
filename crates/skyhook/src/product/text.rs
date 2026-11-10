//! Tables of a product's records, which a front end lays out and a product writes into its
//! Markdown documents.
use std::fmt::Write as _;

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
}
