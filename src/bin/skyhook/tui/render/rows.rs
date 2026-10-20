//! Entry-local rows addressed through cumulative entry heights. A splice moves
//! the rows of the entries after it without laying them out again; heights are
//! summed once per update, from the first entry that moved or changed height.
use super::super::{model::Splice, tool_view::Document};
use super::{Row, selection::LineOrder};
use std::{ops::Index, sync::OnceLock};

/// One entry's layout: its rows and the highlight sources they show.
#[derive(Default)]
pub(super) struct Laid {
    pub(super) rows: Vec<Row>,
    /// Sorted and unique.
    pub(super) sources: Vec<u64>,
    /// Fenced code found while laying out Markdown, for highlight scheduling.
    pub(super) fences: Document,
}

#[derive(Default)]
struct Block {
    laid: Laid,
    /// The order of the rows' logical lines, once a selection needs it.
    order: OnceLock<LineOrder>,
}

#[derive(Default)]
pub struct RowBlocks {
    blocks: Vec<Block>,
    /// Rows through the end of each entry; between a splice and the next
    /// layout, only those before `moved`.
    ends: Vec<usize>,
    /// The first entry whose end a splice or a new height made stale.
    moved: Option<usize>,
}
impl RowBlocks {
    pub fn entry_start(&self, entry: usize) -> Option<usize> {
        (entry < self.entry_count()).then(|| entry.checked_sub(1).map_or(0, |i| self.ends[i]))
    }
    pub fn entry_count(&self) -> usize {
        self.blocks.len()
    }
    pub fn len(&self) -> usize {
        self.ends.last().copied().unwrap_or(0)
    }
    /// The entry showing `row`, and the row's offset within it.
    pub fn locate(&self, row: usize) -> Option<(usize, usize)> {
        // Zero-height entries end at or before `row`, so they are skipped.
        let entry = self.ends.partition_point(|&end| end <= row);
        Some((entry, row - self.entry_start(entry)?))
    }
    pub fn get(&self, row: usize) -> Option<&Row> {
        let (entry, offset) = self.locate(row)?;
        self.blocks[entry].laid.rows.get(offset)
    }
    /// Rows from `row` onward with their entries, without visiting earlier entries.
    pub fn iter_from(&self, row: usize) -> impl Iterator<Item = (usize, &Row)> {
        let (first, offset) = self.locate(row).unwrap_or((self.blocks.len(), 0));
        let blocks = (first..).zip(&self.blocks[first..]);
        blocks.flat_map(move |(entry, block)| {
            let skip = if entry == first { offset } else { 0 };
            block.laid.rows[skip..].iter().map(move |row| (entry, row))
        })
    }
    /// An entry's rows; none past the last.
    pub(super) fn entry_rows(&self, entry: usize) -> &[Row] {
        self.blocks.get(entry).map_or(&[], |block| &block.laid.rows)
    }
    pub(super) fn fences(&self, entry: usize) -> Option<&Document> {
        Some(&self.blocks.get(entry)?.laid.fences)
    }
    /// The order of an entry's logical lines in its current layout.
    pub(super) fn line_order(&self, entry: usize) -> Option<&LineOrder> {
        let block = self.blocks.get(entry)?;
        Some(block.order.get_or_init(|| LineOrder::of(&block.laid.rows)))
    }
    /// `len` entries, none laid out yet.
    pub(super) fn reset(&mut self, len: usize) {
        self.blocks = std::iter::repeat_with(Block::default).take(len).collect();
        self.ends = vec![0; len];
        self.moved = Some(0);
    }
    /// Replace entries with new ones, not laid out yet, moving those after them.
    pub(super) fn splice(&mut self, splice: Splice) {
        let Splice {
            at,
            removed,
            inserted,
        } = splice;
        let new = std::iter::repeat_with(Block::default).take(inserted);
        self.blocks.splice(at..at + removed, new);
        self.ends
            .splice(at..at + removed, std::iter::repeat_n(0, inserted));
        self.moved(at);
    }
    /// Lay the `dirty` entries out again, then sum heights from the first entry
    /// that moved.
    pub(super) fn layout(&mut self, dirty: &[usize], mut lay_out: impl FnMut(usize) -> Laid) {
        for &index in dirty {
            let block = &mut self.blocks[index];
            let height = block.laid.rows.len();
            block.order.take();
            block.laid = lay_out(index);
            let sources = &mut block.laid.sources;
            sources.extend(block.laid.fences.highlight_sources());
            sources.sort_unstable();
            sources.dedup();
            if block.laid.rows.len() != height {
                self.moved(index);
            }
        }
        if let Some(from) = self.moved.take() {
            let mut end = from.checked_sub(1).map_or(0, |i| self.ends[i]);
            for (block, slot) in self.blocks[from..].iter().zip(&mut self.ends[from..]) {
                end += block.laid.rows.len();
                *slot = end;
            }
        }
    }
    fn moved(&mut self, entry: usize) {
        self.moved = Some(self.moved.map_or(entry, |moved| moved.min(entry)));
    }
    /// Entries showing any of `sources`.
    pub(super) fn highlight_entries(&self, sources: &[u64], dirty: &mut Vec<usize>) {
        if sources.is_empty() {
            return;
        }
        let shows = |block: &Block| {
            (sources.iter()).any(|source| block.laid.sources.binary_search(source).is_ok())
        };
        let blocks = self.blocks.iter().enumerate();
        dirty.extend(blocks.filter_map(|(entry, block)| shows(block).then_some(entry)));
    }
}
impl Index<usize> for RowBlocks {
    type Output = Row;
    fn index(&self, index: usize) -> &Row {
        self.get(index).expect("row index out of bounds")
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    impl RowBlocks {
        /// One entry showing `rows`, for fixtures.
        pub(in crate::tui::render) fn entry(rows: Vec<Row>) -> Self {
            let mut blocks = Self::default();
            blocks.reset(1);
            let mut rows = Some(rows);
            blocks.layout(&[0], |_| Laid {
                rows: rows.take().expect("one entry"),
                sources: Vec::new(),
                fences: Document::default(),
            });
            blocks
        }

        pub(in crate::tui) fn iter(&self) -> impl Iterator<Item = &Row> {
            self.blocks.iter().flat_map(|block| &block.laid.rows)
        }
    }

    /// Each entry's row texts and highlight sources, as plain vectors.
    #[derive(Default)]
    struct Naive(Vec<(Vec<String>, Vec<u64>)>);
    impl Naive {
        fn check(&self, actual: &RowBlocks) {
            assert_eq!(actual.entry_count(), self.0.len());
            let flat: Vec<_> = (self.0.iter().enumerate())
                .flat_map(|(entry, (rows, _))| rows.iter().map(move |text| (entry, text.clone())))
                .collect();
            assert_eq!(
                (actual.len(), actual.iter().count()),
                (flat.len(), flat.len())
            );
            let mut prefix = 0;
            for (index, (rows, _)) in self.0.iter().enumerate() {
                assert_eq!(actual.entry_start(index), Some(prefix));
                for offset in 0..rows.len() {
                    assert_eq!(actual.locate(prefix + offset), Some((index, offset)));
                }
                prefix += rows.len();
            }
            assert_eq!(actual.entry_start(self.0.len()), None);
            assert!(actual.get(flat.len()).is_none());
            for start in 0..=flat.len() + 1 {
                let found = actual
                    .iter_from(start)
                    .map(|(entry, row)| (entry, row.text()));
                assert!(found.eq(flat.iter().skip(start).cloned()));
            }
            for source in [0, 1, 7, u64::MAX] {
                let mut dirty = Vec::new();
                actual.highlight_entries(&[source], &mut dirty);
                let shows = |(_, (_, sources)): &(usize, &(_, Vec<u64>))| sources.contains(&source);
                let expected = self.0.iter().enumerate().filter(shows).map(|(i, _)| i);
                assert!(dirty.into_iter().eq(expected), "source {source}");
            }
        }

        /// Lay `dirty` out again in both, with new heights (some zero) and sources.
        fn lay_out(&mut self, actual: &mut RowBlocks, dirty: &[usize], round: usize) {
            for &index in dirty {
                let rows = (0..(index + round) % 4).map(|row| format!("{round}:{index}:{row}"));
                self.0[index] = (rows.collect(), vec![index as u64 % 2, 7, 7]);
            }
            actual.layout(dirty, |index| {
                let line = |text: &String| ratatui::text::Line::from(text.clone());
                let row =
                    |text| super::super::tests::fixture_row(line(text), Default::default(), 0, 80);
                Laid {
                    rows: self.0[index].0.iter().map(row).collect(),
                    sources: self.0[index].1.clone(),
                    fences: Document::default(),
                }
            });
            self.check(actual);
        }
    }

    fn lines(actual: &RowBlocks, entry: usize) -> Vec<Arc<ratatui::text::Line<'static>>> {
        actual
            .entry_rows(entry)
            .iter()
            .map(|row| row.line.clone())
            .collect()
    }

    /// Splices move later entries' rows without laying them out again, and
    /// heights stay summed through growth, shrinking and zero-height entries.
    #[test]
    fn splices_move_rows_and_layouts_keep_heights_summed() {
        let (mut actual, mut naive) = (RowBlocks::default(), Naive::default());
        actual.reset(5);
        naive.0.resize_with(5, Default::default);
        naive.lay_out(&mut actual, &[0, 1, 2, 3, 4], 0);
        let splices = [
            (1, 0, 2),
            (0, 1, 0),
            (2, 2, 1),
            (5, 0, 3),
            (1, 3, 0),
            (4, 1, 1),
        ];
        for (round, (at, removed, inserted)) in (1..).zip(splices) {
            let moved: Vec<_> = (at + removed..actual.entry_count())
                .map(|entry| lines(&actual, entry))
                .collect();
            let splice = Splice {
                at,
                removed,
                inserted,
            };
            actual.splice(splice);
            let new = std::iter::repeat_with(Default::default).take(inserted);
            naive.0.splice(at..at + removed, new);
            naive.lay_out(&mut actual, &Vec::from_iter(at..at + inserted), round);
            for (offset, lines_before) in moved.iter().enumerate() {
                let after = lines(&actual, at + inserted + offset);
                let same = after
                    .iter()
                    .zip(lines_before)
                    .all(|(a, b)| Arc::ptr_eq(a, b));
                assert!(same && after.len() == lines_before.len(), "round {round}");
            }
        }
        naive.lay_out(&mut actual, &[0, 2], 9);
        actual.reset(0);
        Naive::default().check(&actual);
    }
}
