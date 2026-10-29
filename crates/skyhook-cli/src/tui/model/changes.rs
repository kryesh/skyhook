//! How entries moved and changed since their rows were last laid out.

use super::Entry;

/// Entries `at..at + removed` replaced by `inserted` new ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Splice {
    pub at: usize,
    pub removed: usize,
    pub inserted: usize,
}

impl Splice {
    /// Where entry `index` is once this splice applies; none if it removed it.
    fn follow(self, index: usize) -> Option<usize> {
        match index.checked_sub(self.at) {
            None => Some(index),
            Some(into) if into < self.removed => None,
            Some(_) => Some(index - self.removed + self.inserted),
        }
    }
}

/// Splices in the order they apply, each indexed as the entries stand after
/// the ones before it, and the entries to lay out again, indexed as they stand
/// after all of them. Inserted entries are among those to lay out; entries a
/// splice only moved are not.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub splices: Vec<Splice>,
    pub dirty: Vec<usize>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.splices.is_empty() && self.dirty.is_empty()
    }

    pub(super) fn changed(&mut self, index: usize) {
        self.dirty.push(index);
    }

    pub(super) fn splice(&mut self, at: usize, removed: usize, inserted: usize) {
        if removed == 0 && inserted == 0 {
            return;
        }
        let splice = Splice {
            at,
            removed,
            inserted,
        };
        self.dirty.retain_mut(|index| match splice.follow(*index) {
            Some(moved) => {
                *index = moved;
                true
            }
            None => false,
        });
        self.dirty.extend(at..at + inserted);
        // A splice starting where the last one's insertion ends composes with
        // it into one, so a run of neighbouring splices moves the rows after
        // them once.
        match self.splices.last_mut() {
            Some(last) if last.at + last.inserted == at => {
                last.removed += removed;
                last.inserted += inserted;
            }
            _ => self.splices.push(splice),
        }
    }

    /// `new` took the place of `old` at `offset`: entries that differ at the
    /// same position, then the difference in length at the end.
    pub(super) fn replace(&mut self, old: &[Entry], new: &[Entry], offset: usize) {
        let common = old.len().min(new.len());
        let differ = (0..common).filter(|&index| old[index] != new[index]);
        self.dirty.extend(differ.map(|index| offset + index));
        self.splice(offset + common, old.len() - common, new.len() - common);
    }
}
