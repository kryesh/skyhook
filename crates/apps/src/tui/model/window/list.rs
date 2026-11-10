//! A run of consecutive keys read a page at a time.
use super::super::Gap;
use skyhook::session::Page;
use std::ops::{Range, RangeInclusive};

/// Consecutive keys read a page at a time, those kept, and whether more lie past
/// either end.
pub struct List<K> {
    run: Option<Run<K>>,
    pub earlier: bool,
    pub later: bool,
    /// The newest key the updates made, loaded or not.
    newest: Option<K>,
}

/// The keys read, from `first` to `last`, kept or passed, and those kept.
struct Run<K> {
    first: K,
    last: K,
    kept: Vec<K>,
}

impl<K> Default for List<K> {
    /// Nothing read yet: whatever exists lies past the end.
    fn default() -> Self {
        Self {
            run: None,
            earlier: false,
            later: true,
            newest: None,
        }
    }
}

impl<K: Ord + Copy> List<K> {
    /// None to read: every key will come as it is made.
    pub(in crate::tui::model) fn complete() -> Self {
        Self {
            later: false,
            ..Self::default()
        }
    }

    /// Add what `page` read in order, each key kept or passed, `full` when it read as
    /// many as it asked for, unless it was read for an end that has moved since; a page
    /// from an end reads the list afresh.
    pub(in crate::tui::model) fn insert(
        &mut self,
        page: Page<K>,
        read: impl IntoIterator<Item = (K, bool)>,
        full: bool,
    ) -> bool {
        let ends = self.run.as_ref().map(|run| (run.first, run.last));
        let empty = ends.is_none();
        match page {
            Page::First => (self.run, self.earlier, self.later) = (None, false, full),
            Page::Last => (self.run, self.earlier, self.later) = (None, full, false),
            Page::Before(key) if ends.is_none_or(|(first, _)| first == key) => {
                (self.earlier, self.later) = (full, self.later || empty);
            }
            Page::After(key) if ends.is_none_or(|(_, last)| last == key) => {
                (self.earlier, self.later) = (self.earlier || empty, full);
            }
            _ => return false,
        }
        let read: Vec<_> = read.into_iter().collect();
        if let (Some(&(start, _)), Some(&(end, _))) = (read.first(), read.last()) {
            let kept = read.iter().filter_map(|&(key, kept)| kept.then_some(key));
            match &mut self.run {
                None => {
                    self.run = Some(Run {
                        first: start,
                        last: end,
                        kept: kept.collect(),
                    });
                }
                Some(run) => {
                    (run.first, run.last) = (run.first.min(start), run.last.max(end));
                    match page {
                        Page::Before(_) => drop(run.kept.splice(0..0, kept)),
                        _ => run.kept.extend(kept),
                    }
                }
            }
        }
        // A key made after the read lies past what it read, however little it read.
        self.later |= self.newest > self.run.as_ref().map(|run| run.last);
        true
    }

    /// Add a key the updates made, at the end, while the end is loaded: kept, or passed.
    /// Returns whether it was kept.
    pub(in crate::tui::model) fn push(&mut self, key: K, kept: bool) -> bool {
        self.newest = self.newest.max(Some(key));
        let added = !self.later && self.run.as_ref().is_none_or(|run| run.last < key);
        if added {
            let run = self.run.get_or_insert_with(|| Run {
                first: key,
                last: key,
                kept: Vec::new(),
            });
            run.last = key;
            if kept {
                run.kept.push(key);
            }
        }
        added && kept
    }

    /// The keys kept, in order.
    pub fn kept(&self) -> &[K] {
        self.run.as_ref().map_or(&[], |run| &run.kept)
    }

    /// The first through the last key read, kept or passed.
    pub fn extent(&self) -> Option<RangeInclusive<K>> {
        self.run.as_ref().map(|run| run.first..=run.last)
    }

    /// Whether `key` lies among the keys read, kept or passed.
    pub(in crate::tui::model) fn holds(&self, key: K) -> bool {
        self.extent().is_some_and(|extent| extent.contains(&key))
    }

    /// Where `key` falls among the keys loaded.
    pub(in crate::tui::model) fn at(&self, key: K) -> usize {
        self.kept().partition_point(|k| *k < key)
    }

    /// Whether `key` lies within what is read, or past an end nothing lies beyond.
    pub(in crate::tui::model) fn reaches(&self, key: K) -> bool {
        let Some(run) = &self.run else {
            return !self.earlier && !self.later;
        };
        (!self.earlier || run.first <= key) && (!self.later || key <= run.last)
    }

    /// Let go of the keys beyond the `kept` around the view's `shown` ones.
    pub(in crate::tui::model) fn trim(&mut self, shown: Range<usize>, kept: usize) -> bool {
        let Some(run) = &mut self.run else {
            return false;
        };
        let before = shown.start.saturating_sub(kept / 2);
        let after = (run.kept.len() - shown.end).saturating_sub(kept / 2);
        if before + after == 0 {
            return false;
        }
        run.kept.drain(..before);
        run.kept.truncate(run.kept.len() - after);
        // What lies past the keys kept is read again.
        if let (Some(&start), Some(&end)) = (run.kept.first(), run.kept.last()) {
            if before > 0 {
                run.first = start;
            }
            if after > 0 {
                run.last = end;
            }
        }
        self.earlier |= before > 0;
        self.later |= after > 0;
        true
    }

    /// The page of up to `page` keys beyond the end `gap` stands for, while more lie
    /// there and trimming to the `kept` around the view's `shown` ones would keep what
    /// it reads.
    pub(in crate::tui::model) fn want(
        &self,
        gap: Gap,
        shown: Range<usize>,
        page: usize,
        kept: usize,
    ) -> Option<Page<K>> {
        let kept = |beyond: usize| beyond + page <= kept / 2;
        let run = self.run.as_ref()?;
        match gap {
            Gap::Earlier if self.earlier && kept(shown.start) => Some(Page::Before(run.first)),
            Gap::Later if self.later && kept(run.kept.len() - shown.end) => {
                Some(Page::After(run.last))
            }
            _ => None,
        }
    }
}
