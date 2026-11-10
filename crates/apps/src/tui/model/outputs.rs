//! One owner for a job's cached presentation and asynchronous refresh.
use skyhook::job::{FieldFile, JobView};
use skyhook::tool::ToolError;
use skyhook_worker::job::JobId;
use std::collections::HashMap;
use std::sync::Arc;

/// A job's presented output with the files holding its fields, or why it could not
/// be loaded.
pub type LoadedOutput = Result<(JobView, Vec<FieldFile>), OutputFailure>;

/// Why a job's output could not be loaded, as it is shown: the same failure loaded again
/// is unchanged.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputFailure(Arc<str>);

impl From<ToolError> for OutputFailure {
    fn from(error: ToolError) -> Self {
        Self(error.to_string().into())
    }
}

impl std::fmt::Display for OutputFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Loads in flight at once; the cards shown first take the slots.
pub const MAX_LOADS: usize = 6;

/// The outputs of the cards the window shows; the window bounds which those are.
#[derive(Default)]
pub struct OutputStore {
    entries: HashMap<JobId, OutputEntry>,
    /// The jobs whose output is to be read again, in the order it changed: a card
    /// that keeps changing waits behind the others.
    wanted: Vec<JobId>,
    /// Held by each load in flight, until its completion is handled.
    loads: Arc<()>,
}
#[derive(Default)]
struct OutputEntry {
    cached: Option<LoadedOutput>,
    /// Whether a load of it is in flight.
    loading: bool,
}
/// A job's load in flight, which holds its slot until its completion is handled.
pub struct OutputAttempt {
    job: JobId,
    _load: Arc<()>,
}
impl OutputStore {
    pub fn get(&self, job: &JobId) -> Option<&LoadedOutput> {
        self.entries.get(job)?.cached.as_ref()
    }
    /// Each job's output as loaded.
    pub fn loaded(&self) -> HashMap<JobId, LoadedOutput> {
        let loaded = self.entries.iter();
        loaded
            .filter_map(|(job, entry)| Some((*job, entry.cached.clone()?)))
            .collect()
    }
    fn want(&mut self, job: JobId) {
        if !self.wanted.contains(&job) {
            self.wanted.push(job);
        }
    }
    /// `job`'s output may have changed since it was loaded. Returns whether
    /// any of it was.
    pub fn changed(&mut self, job: JobId) -> bool {
        let known = self.entries.contains_key(&job);
        known.then(|| self.want(job)).is_some()
    }
    /// Every loaded output may have changed, unnoticed.
    pub fn changed_all(&mut self) {
        let jobs: Vec<_> = self.entries.keys().copied().collect();
        jobs.into_iter().for_each(|job| self.want(job));
    }
    /// The jobs among `shown` whose output is to be read, in the order they are
    /// wanted; one never read is wanted from now.
    pub fn wanted(&mut self, shown: &[JobId]) -> Vec<JobId> {
        for &job in shown {
            let entry = self.entries.entry(job).or_default();
            if entry.cached.is_none() && !entry.loading {
                self.want(job);
            }
        }
        let wanted = self.wanted.iter().filter(|job| shown.contains(job));
        wanted.copied().collect()
    }
    /// Let go of the outputs of jobs no longer `shown`.
    pub fn retain(&mut self, shown: impl Fn(&JobId) -> bool) {
        self.entries.retain(|job, entry| {
            let shown = shown(job);
            if !shown {
                entry.cached = None;
            }
            shown || entry.loading
        });
        let entries = &self.entries;
        self.wanted.retain(|job| entries.contains_key(job));
    }
    pub fn begin(&mut self, job: JobId) -> Option<OutputAttempt> {
        if Arc::strong_count(&self.loads) > MAX_LOADS {
            return None;
        }
        let entry = self.entries.entry(job).or_default();
        if std::mem::replace(&mut entry.loading, true) {
            return None;
        }
        self.wanted.retain(|wanted| *wanted != job);
        let _load = self.loads.clone();
        Some(OutputAttempt { job, _load })
    }
    /// Install a load's result. Returns the job when its shown output changed.
    pub fn complete(&mut self, attempt: OutputAttempt, output: LoadedOutput) -> Option<JobId> {
        let entry = self.entries.entry(attempt.job).or_default();
        entry.loading = false;
        let changed = entry.cached.as_ref() != Some(&output);
        entry.cached = Some(output);
        changed.then_some(attempt.job)
    }
}

/// A store holding exactly these outputs, as if each had loaded.
impl FromIterator<(JobId, LoadedOutput)> for OutputStore {
    fn from_iter<I: IntoIterator<Item = (JobId, LoadedOutput)>>(outputs: I) -> Self {
        let entries = outputs.into_iter().map(|(job, output)| {
            let entry = OutputEntry {
                cached: Some(output),
                loading: false,
            };
            (job, entry)
        });
        Self {
            entries: entries.collect(),
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::tool_view::tests::view;
    use serde_json::json;

    impl OutputAttempt {
        pub(crate) fn job(&self) -> JobId {
            self.job
        }
    }

    impl OutputStore {
        /// How many loads are in flight.
        pub(crate) fn loading(&self) -> usize {
            Arc::strong_count(&self.loads) - 1
        }
    }

    fn job() -> JobId {
        JobId::new(1).unwrap()
    }
    fn value(s: &str) -> LoadedOutput {
        Ok((view(json!({"result": s})), Vec::new()))
    }
    #[test]
    fn cache_survives_refresh_equal_results_do_not_repaint_and_errors_are_shown() {
        let mut store = OutputStore::default();
        let first = store.begin(job()).unwrap();
        assert!(store.begin(job()).is_none() && store.get(&job()).is_none());
        assert_eq!(store.complete(first, value("cached")), Some(job()));
        let refresh = store.begin(job()).unwrap();
        assert_eq!(store.get(&job()), Some(&value("cached")));
        assert!(store.complete(refresh, value("cached")).is_none());
        assert!(store.wanted(&[job()]).is_empty());
        store.changed(job());
        assert_eq!(store.wanted(&[job()]), [job()]);
        assert_eq!(store.get(&job()), Some(&value("cached")));
        let failed = store.begin(job()).unwrap();
        let failure = |message| -> LoadedOutput { Err(ToolError::failed(message).into()) };
        assert_eq!(store.complete(failed, failure("unavailable")), Some(job()));
        assert!(store.wanted(&[job()]).is_empty());
        assert_eq!(store.get(&job()), Some(&failure("unavailable")));
        // The same failure again does not repaint; another does.
        let again = store.begin(job()).unwrap();
        assert!(store.complete(again, failure("unavailable")).is_none());
        let other = store.begin(job()).unwrap();
        assert_eq!(store.complete(other, failure("gone")), Some(job()));
        // Changes are read in the order they came.
        let other = JobId::new(2).unwrap();
        let loaded = store.begin(other).unwrap();
        store.complete(loaded, value("other"));
        store.changed(job());
        store.changed(other);
        assert_eq!(store.wanted(&[other, job()]), [job(), other]);
    }
}
