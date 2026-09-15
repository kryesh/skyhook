//! Entry-local rows with logarithmic row lookup and height updates.
use super::Row;
use std::{
    collections::{HashMap, HashSet},
    ops::Index,
};

#[derive(Default)]
struct Heights {
    values: Vec<usize>,
    tree: Vec<usize>,
    total: usize,
}
impl Heights {
    fn prefix(&self, mut end: usize) -> usize {
        let mut sum = 0;
        while end != 0 {
            sum += self.tree[end - 1];
            end &= end - 1;
        }
        sum
    }
    fn set(&mut self, index: usize, value: usize) {
        if index == self.values.len() {
            let end = index + 1;
            let begin = end & (end - 1);
            let sum = self.prefix(index) - self.prefix(begin);
            self.values.push(0);
            self.tree.push(sum);
        }
        let old = self.values[index];
        self.values[index] = value;
        self.total = self.total - old + value;
        let mut i = index + 1;
        while i <= self.tree.len() {
            self.tree[i - 1] = self.tree[i - 1] - old + value;
            i += i & i.wrapping_neg();
        }
    }
    fn locate(&self, row: usize) -> Option<(usize, usize)> {
        if row >= self.total {
            return None;
        }
        let mut entry = 0;
        let mut sum = 0;
        let mut bit = 1usize << self.tree.len().ilog2();
        while bit != 0 {
            let next = entry + bit;
            if next <= self.tree.len() && sum + self.tree[next - 1] <= row {
                entry = next;
                sum += self.tree[next - 1];
            }
            bit >>= 1;
        }
        Some((entry, row - sum))
    }
}

#[derive(Default)]
pub struct RowBlocks {
    blocks: Vec<Vec<Row>>,
    heights: Heights,
    sources: HashMap<u64, HashSet<usize>>,
    entry_sources: HashMap<usize, Vec<u64>>,
}
impl RowBlocks {
    pub fn entry_start(&self, entry: usize) -> Option<usize> {
        (entry < self.entry_count()).then(|| self.heights.prefix(entry))
    }
    pub fn entry_count(&self) -> usize {
        self.heights.values.len()
    }
    pub fn len(&self) -> usize {
        self.heights.total
    }
    pub fn get(&self, row: usize) -> Option<&Row> {
        let (entry, offset) = self.heights.locate(row)?;
        self.blocks.get(entry)?.get(offset)
    }
    pub fn iter(&self) -> Rows<'_> {
        Rows {
            rows: self,
            position: 0,
        }
    }
    pub fn clear(&mut self) {
        self.blocks.clear();
        self.sources.clear();
        self.entry_sources.clear();
        self.heights = Heights::default();
    }
    pub(super) fn truncate_entries(&mut self, len: usize) {
        while self.blocks.len() > len {
            let index = self.blocks.len() - 1;
            self.blocks[index].clear();
            self.sync_entry(index, Vec::new());
            let h = &mut self.heights;
            h.values.pop();
            h.tree.pop();
            self.blocks.pop();
        }
    }
    /// Update rows and their admitted highlight-source memberships as one unit.
    pub(super) fn update_entry(
        &mut self,
        index: usize,
        mut sources: Vec<u64>,
        update: impl FnOnce(&mut Vec<Row>),
    ) {
        sources.sort_unstable();
        sources.dedup();
        self.ensure_entry(index);
        update(&mut self.blocks[index]);
        self.sync_entry(index, sources);
    }
    // Fixture convenience; production mutations use update_entry as well,
    // retaining the existing row-vector allocation.
    #[cfg(test)]
    pub(super) fn replace_entry(&mut self, index: usize, rows: Vec<Row>, sources: Vec<u64>) {
        self.update_entry(index, sources, |block| *block = rows);
    }
    fn sync_entry(&mut self, index: usize, sources: Vec<u64>) {
        self.heights.set(index, self.blocks[index].len());
        if let Some(old) = self.entry_sources.remove(&index) {
            for source in old {
                if let Some(entries) = self.sources.get_mut(&source) {
                    entries.remove(&index);
                    if entries.is_empty() {
                        self.sources.remove(&source);
                    }
                }
            }
        }
        for &source in &sources {
            self.sources.entry(source).or_default().insert(index);
        }
        if !sources.is_empty() {
            self.entry_sources.insert(index, sources);
        }
    }
    pub(super) fn highlight_entries(&self, sources: &[u64], dirty: &mut Vec<usize>) {
        for source in sources {
            if let Some(entries) = self.sources.get(source) {
                dirty.extend(entries.iter().copied());
            }
        }
    }
    fn ensure_entry(&mut self, index: usize) {
        while self.blocks.len() <= index {
            self.heights.set(self.blocks.len(), 0);
            self.blocks.push(Vec::new());
        }
    }
}
impl Index<usize> for RowBlocks {
    type Output = Row;
    fn index(&self, index: usize) -> &Row {
        self.get(index).expect("row index out of bounds")
    }
}
pub struct Rows<'a> {
    rows: &'a RowBlocks,
    position: usize,
}
impl<'a> Iterator for Rows<'a> {
    type Item = &'a Row;
    fn next(&mut self) -> Option<Self::Item> {
        let row = self.rows.get(self.position)?;
        self.position += 1;
        Some(row)
    }
    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        self.position = self.position.saturating_add(n).min(self.rows.len());
        self.next()
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.rows.len() - self.position;
        (remaining, Some(remaining))
    }
}
impl ExactSizeIterator for Rows<'_> {}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(entry: usize, label: usize) -> Row {
        super::super::tests::fixture_row(
            ratatui::text::Line::from(format!("{entry}:{label}")),
            Default::default(),
            0,
            80,
            entry,
        )
    }

    #[derive(Default)]
    struct Naive {
        blocks: Vec<Vec<Row>>,
        sources: Vec<Vec<u64>>,
    }
    impl Naive {
        fn replace(&mut self, index: usize, rows: Vec<Row>, sources: Vec<u64>) {
            let len = self.blocks.len().max(index + 1);
            self.blocks.resize_with(len, Vec::new);
            self.sources.resize_with(len, Vec::new);
            self.blocks[index] = rows;
            self.sources[index] = sources;
        }
        fn check(&self, actual: &RowBlocks) {
            assert_eq!(actual.entry_count(), self.blocks.len());
            let flat: Vec<_> = self.blocks.iter().flatten().collect();
            assert_eq!(actual.len(), flat.len());
            assert_eq!(actual.iter().count(), flat.len());
            let mut prefix = 0;
            for (index, block) in self.blocks.iter().enumerate() {
                assert_eq!(actual.entry_start(index), Some(prefix));
                for offset in 0..block.len() {
                    assert_eq!(
                        actual.heights.locate(prefix + offset),
                        Some((index, offset))
                    );
                }
                prefix += block.len();
            }
            assert_eq!(actual.entry_start(self.blocks.len()), None);
            for (index, expected) in flat.iter().enumerate() {
                let found = actual.get(index).unwrap();
                assert_eq!(
                    (found.entry, found.text()),
                    (expected.entry, expected.text())
                );
            }
            assert!(actual.get(flat.len()).is_none());
            let mut reverse: HashMap<u64, HashSet<usize>> = HashMap::new();
            for (index, sources) in self.sources.iter().enumerate() {
                for source in sources {
                    reverse.entry(*source).or_default().insert(index);
                }
            }
            for source in reverse.keys().copied().chain([0, 7, u64::MAX]) {
                let mut dirty = Vec::new();
                actual.highlight_entries(&[source], &mut dirty);
                let unique: HashSet<_> = dirty.iter().copied().collect();
                assert_eq!(dirty.len(), unique.len(), "duplicate reverse membership");
                assert_eq!(unique, reverse.get(&source).cloned().unwrap_or_default());
            }
        }
    }

    #[test]
    fn entry_updates_grow_shrink_and_preserve_zero_height_prefixes() {
        let mut actual = RowBlocks::default();
        let mut naive = Naive::default();
        naive.check(&actual);
        for (index, height) in [(3, 4), (0, 2), (1, 0), (2, 3), (3, 1), (0, 0)] {
            let rows: Vec<_> = (0..height).map(|label| row(index, label)).collect();
            let sources = vec![index as u64, 7, 7];
            actual.replace_entry(index, rows.clone(), sources.clone());
            naive.replace(index, rows, sources);
            naive.check(&actual);
        }
        for step in 0..24 {
            let index = step * 17 % naive.blocks.len();
            let sources = if step % 3 == 0 { vec![] } else { vec![2, 2, 3] };
            actual.update_entry(index, sources.clone(), |rows| {
                if step % 2 == 0 {
                    rows.push(row(index, step));
                } else {
                    rows.truncate(step % 4);
                }
            });
            if step % 2 == 0 {
                naive.blocks[index].push(row(index, step));
            } else {
                naive.blocks[index].truncate(step % 4);
            }
            naive.sources[index] = sources;
            naive.check(&actual);
        }
        // Truncate, clear and reappend remove stale sources and heights.
        for len in [3, 1, 0] {
            actual.truncate_entries(len);
            naive.blocks.truncate(len);
            naive.sources.truncate(len);
            naive.check(&actual);
        }
        actual.replace_entry(2, vec![row(2, 1)], vec![2, 2]);
        naive.replace(2, vec![row(2, 1)], vec![2]);
        naive.check(&actual);
        actual.clear();
        naive = Naive::default();
        naive.check(&actual);
    }

    #[test]
    fn height_index_matches_flat_prefixes() {
        let mut h = Heights::default();
        let mut values = Vec::new();
        for index in 0..513 {
            let value = index % 7;
            values.push(value);
            h.set(index, value);
        }
        for step in 0..100 {
            let index = step * 73 % values.len();
            values[index] = step % 13;
            h.set(index, values[index]);
            let mut total = 0;
            for (i, &value) in values.iter().enumerate() {
                assert_eq!(h.prefix(i), total);
                for offset in 0..value {
                    assert_eq!(h.locate(total + offset), Some((i, offset)));
                }
                total += value;
            }
            assert_eq!(h.total, total);
            assert_eq!(h.locate(total), None);
        }
    }
}
