//! One owner for a job's cached presentation, selection and asynchronous refresh.
use super::*;
use skyhook::job::JobView;

/// A job's presented output, or why it could not be loaded.
pub type LoadedOutput = Result<JobView, String>;

#[derive(Default)]
pub struct OutputStore {
    entries: HashMap<JobId, OutputEntry>,
}
#[derive(Default)]
struct OutputEntry {
    cached: Option<LoadedOutput>,
    query: Option<JobOutputQuery>,
    pending: Option<OutputAttempt>,
    final_output: bool,
}
/// A refresh is tied to its job and exact attempt; a selection change clears
/// the pending attempt, so a stale completion can never match again.
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
    pub fn get(&self, job: &JobId) -> Option<&LoadedOutput> {
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
        result: LoadedOutput,
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
        let changed = entry.cached.as_ref() != Some(&result);
        entry.cached = Some(result);
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::tool_view::tests::view;
    use serde_json::json;
    fn job() -> JobId {
        JobId::new(1).unwrap()
    }
    fn value(s: &str) -> LoadedOutput {
        Ok(view(json!({"result": s})))
    }
    #[test]
    fn query_change_rejects_old_completion_before_clearing_current_attempt() {
        let mut store = OutputStore::default();
        let (old, _) = store.begin(job()).unwrap();
        let mut query = JobOutputQuery::new(job());
        query.field = Some("/result/stdout".parse().unwrap());
        store.set_query(query);
        let (current, query) = store.begin(job()).unwrap();
        assert_eq!(query.field, Some("/result/stdout".parse().unwrap()));
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
        let cached = store.get(&job()).unwrap().as_ref().unwrap();
        assert_eq!(cached.result().unwrap(), "cached");
        assert!(!store.complete(refresh, true, value("cached")));
        assert!(store.is_final(job()));
        store.clear_query(job());
        assert!(!store.is_final(job()));
        let (failed, _) = store.begin(job()).unwrap();
        assert!(store.complete(failed, true, Err("unavailable".into())));
        assert!(store.is_final(job()));
    }
}
