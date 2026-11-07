//! Script calls between their issue and their job's publication, counted so a
//! `wait` beside them knows they are coming.

use super::{JobId, JobManager, Jobs, SessionStore};

/// A call from its issue until its job is published or refused.
pub(crate) struct Issuing(SessionStore, JobId);

impl Issuing {
    /// Count a call `parent` issued, synchronously, until its job is published:
    /// the count is queued ahead of every command its caller issues after it.
    pub(crate) fn new(jobs: &JobManager, parent: JobId) -> Self {
        let store = jobs.store().clone();
        store.enqueue(move |state| state.jobs.issue(parent));
        Self(store, parent)
    }
}

impl Drop for Issuing {
    fn drop(&mut self) {
        let parent = self.1;
        self.0.enqueue(move |state| state.jobs.issued(parent));
    }
}

impl Jobs {
    fn issue(&mut self, parent: JobId) {
        *self.calls.issuing.entry(parent).or_default() += 1;
    }

    fn issued(&mut self, parent: JobId) {
        if let Some(count) = self.calls.issuing.get_mut(&parent) {
            *count -= 1;
            if *count == 0 {
                self.calls.issuing.remove(&parent);
                self.dirty.job(parent);
            }
        }
    }
}
