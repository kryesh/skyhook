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
            self.register_sources(index, Vec::new());
            let h = &mut self.heights;
            h.set(index, 0);
            h.values.pop();
            h.tree.pop();
            self.blocks.pop();
        }
    }
    pub(super) fn register_sources(&mut self, index: usize, sources: Vec<u64>) {
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
    pub(super) fn block_mut(&mut self, index: usize) -> &mut Vec<Row> {
        while self.blocks.len() <= index {
            self.heights.set(self.blocks.len(), 0);
            self.blocks.push(Vec::new());
        }
        &mut self.blocks[index]
    }
    pub(super) fn finish_update(&mut self, index: usize) {
        self.heights.set(index, self.blocks[index].len());
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
