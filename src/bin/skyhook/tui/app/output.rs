//! One owner for a job's cached presentation, selection and asynchronous refresh.
use super::*;
use crate::tui::tool_view::OutputView;

#[derive(Default)]
pub struct OutputStore {
    entries: HashMap<JobId, OutputEntry>,
}
#[derive(Default)]
struct OutputEntry {
    cached: Option<OutputView>,
    query: Option<JobOutputQuery>,
    pending: Option<OutputAttempt>,
    final_output: bool,
}
/// A refresh is tied to its job and exact attempt; a selection change clears
/// the pending attempt and a session change clears the store, so a stale
/// completion can never match again.
#[derive(Clone)]
pub struct OutputAttempt {
    job: JobId,
    identity: Token,
}
impl OutputAttempt {
    pub fn job(&self) -> JobId {
        self.job
    }
}
impl OutputStore {
    pub fn get(&self, job: &JobId) -> Option<&OutputView> {
        self.entries.get(job)?.cached.as_ref()
    }
    pub fn query(&self, job: JobId) -> Option<&JobOutputQuery> {
        self.entries.get(&job)?.query.as_ref()
    }
    pub fn is_final(&self, job: JobId) -> bool {
        self.entries
            .get(&job)
            .is_some_and(|entry| entry.final_output)
    }
    pub fn set_query(&mut self, query: JobOutputQuery) {
        let entry = self.entries.entry(query.job).or_default();
        entry.query = Some(query);
        Self::invalidate_selection(entry);
    }
    pub fn clear_query(&mut self, job: JobId) {
        let entry = self.entries.entry(job).or_default();
        entry.query = None;
        Self::invalidate_selection(entry);
    }
    fn invalidate_selection(entry: &mut OutputEntry) {
        entry.pending = None;
        entry.final_output = false;
    }
    pub fn begin(&mut self, job: JobId) -> Option<(OutputAttempt, JobOutputQuery)> {
        let entry = self.entries.entry(job).or_default();
        if entry.pending.is_some() {
            return None;
        }
        let attempt = OutputAttempt {
            job,
            identity: Token::default(),
        };
        entry.pending = Some(attempt.clone());
        let query = entry
            .query
            .clone()
            .unwrap_or_else(|| JobOutputQuery::new(job));
        Some((attempt, query))
    }
    /// Stale completion cannot clear a newer refresh, finalize a newer query,
    /// repaint it, or install output into another session.
    pub fn complete(
        &mut self,
        attempt: OutputAttempt,
        finished: bool,
        result: Result<OutputView, String>,
    ) -> bool {
        let Some(entry) = self.entries.get_mut(&attempt.job) else {
            return false;
        };
        if !entry
            .pending
            .as_ref()
            .is_some_and(|pending| pending.identity.matches(&attempt.identity))
        {
            return false;
        }
        entry.pending = None;
        entry.final_output = finished;
        let value = result.unwrap_or_else(OutputView::error);
        let changed = entry
            .cached
            .as_ref()
            .is_none_or(|cached| cached.value() != value.value());
        // Keep the canonical product even when its visible JSON is unchanged.
        entry.cached = Some(value);
        changed
    }
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    #[cfg(test)]
    pub fn insert_product(&mut self, job: JobId, value: OutputView) {
        self.entries.entry(job).or_default().cached = Some(value);
    }
    #[cfg(test)]
    pub fn clear_pending(&mut self) {
        for entry in self.entries.values_mut() {
            entry.pending = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn job() -> JobId {
        JobId::new(1).unwrap()
    }
    fn value(s: &str) -> Result<OutputView, String> {
        Ok(OutputView::historical(json!({"result":s})))
    }
    #[test]
    fn query_change_rejects_old_completion_before_clearing_current_attempt() {
        let mut store = OutputStore::default();
        let (old, _) = store.begin(job()).unwrap();
        let mut query = JobOutputQuery::new(job());
        query.field = Some("/result/stdout".into());
        store.set_query(query);
        let (current, query) = store.begin(job()).unwrap();
        assert_eq!(query.field.as_deref(), Some("/result/stdout"));
        assert!(!store.complete(old, true, value("old")));
        assert!(store.begin(job()).is_none());
        assert!(!store.is_final(job()));
        assert!(store.complete(current, true, value("new")));
        assert!(store.is_final(job()));
        // A duplicate of a retired completion cannot retire a newer attempt of
        // the same query either.
        let (old, _) = store.begin(job()).unwrap();
        assert!(store.complete(old.clone(), false, value("first")));
        let (new, _) = store.begin(job()).unwrap();
        assert!(!store.complete(old, true, value("duplicate")));
        assert!(store.begin(job()).is_none());
        assert!(!store.is_final(job()));
        assert!(store.complete(new, false, value("refresh")));
    }
    #[test]
    fn cache_survives_refresh_equal_results_do_not_repaint_and_errors_finalize() {
        let mut store = OutputStore::default();
        let (first, _) = store.begin(job()).unwrap();
        assert!(store.complete(first, false, value("cached")));
        let (refresh, _) = store.begin(job()).unwrap();
        assert_eq!(store.get(&job()).unwrap().value()["result"], "cached");
        assert!(!store.complete(refresh, true, value("cached")));
        assert!(store.is_final(job()));
        store.clear_query(job());
        assert!(!store.is_final(job()));
        let (failed, _) = store.begin(job()).unwrap();
        assert!(store.complete(failed, true, Err("unavailable".into())));
        assert!(store.is_final(job()));
    }
    #[test]
    fn reset_rejects_outstanding_completion() {
        let mut store = OutputStore::default();
        let (old, _) = store.begin(job()).unwrap();
        store.clear();
        let (new, _) = store.begin(job()).unwrap();
        assert!(!store.complete(old, true, value("old")));
        assert!(store.begin(job()).is_none());
        assert!(store.complete(new, false, value("new")));
    }
}
