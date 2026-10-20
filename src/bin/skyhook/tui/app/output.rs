//! One owner for a job's cached presentation, selection and asynchronous refresh.
use super::*;
use skyhook::job::JobView;
use std::collections::VecDeque;
use std::sync::Arc;

/// A job's presented output, or why it could not be loaded.
pub type LoadedOutput = Result<JobView, String>;

/// Loads in flight at once; the cards shown first take the slots.
pub(super) const MAX_LOADS: usize = 6;

/// The outputs of the cards the window shows; the window bounds which those are.
#[derive(Default)]
pub struct OutputStore {
    entries: HashMap<JobId, OutputEntry>,
    /// The jobs whose output is to be read again, in the order it changed, a chosen
    /// selection first: a card that keeps changing waits behind the others.
    wanted: VecDeque<JobId>,
    /// Held by each load in flight, until its completion is handled.
    loads: Arc<()>,
}
#[derive(Default)]
struct OutputEntry {
    /// The output last loaded, and the query chosen when it was asked for.
    cached: Option<(Option<JobOutputQuery>, LoadedOutput)>,
    query: Option<JobOutputQuery>,
    /// The identity of the load in flight, and the query chosen for it.
    pending: Option<(Token, Option<JobOutputQuery>)>,
}
/// A refresh is tied to its job and exact attempt; a selection change clears
/// the pending attempt, so a stale completion can never match again.
pub struct OutputAttempt {
    job: JobId,
    identity: Token,
    _load: Arc<()>,
}
impl OutputStore {
    pub fn get(&self, job: &JobId) -> Option<&LoadedOutput> {
        Some(&self.entries.get(job)?.cached.as_ref()?.1)
    }
    /// `job`'s output, while it was loaded for the query chosen now.
    pub fn current(&self, job: &JobId) -> Option<&LoadedOutput> {
        let entry = self.entries.get(job)?;
        let (chosen, output) = entry.cached.as_ref()?;
        (*chosen == entry.query).then_some(output)
    }
    /// Each job's output as loaded, or else the query chosen to load it by.
    pub fn chosen(&self) -> HashMap<JobId, Result<LoadedOutput, JobOutputQuery>> {
        let chosen = self.entries.iter().filter_map(|(job, entry)| {
            let chosen = match (&entry.cached, &entry.query) {
                (Some((_, output)), _) => Ok(output.clone()),
                (None, query) => Err(query.clone()?),
            };
            Some((*job, chosen))
        });
        chosen.collect()
    }
    pub fn query(&self, job: JobId) -> Option<&JobOutputQuery> {
        self.entries.get(&job)?.query.as_ref()
    }
    pub fn set_query(&mut self, query: JobOutputQuery) {
        let job = query.job;
        self.entries.entry(job).or_default().query = Some(query);
        self.choose(job);
    }
    pub fn clear_query(&mut self, job: JobId) {
        self.entries.entry(job).or_default().query = None;
        self.choose(job);
    }
    /// A selection chosen for `job` abandons its load in flight and is read first.
    fn choose(&mut self, job: JobId) {
        self.entries.entry(job).or_default().pending = None;
        self.wanted.retain(|wanted| *wanted != job);
        self.wanted.push_front(job);
    }
    fn want(&mut self, job: JobId) {
        if !self.wanted.contains(&job) {
            self.wanted.push_back(job);
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
            if entry.cached.is_none() && entry.pending.is_none() {
                self.want(job);
            }
        }
        let wanted = self.wanted.iter().filter(|job| shown.contains(job));
        wanted.copied().collect()
    }
    /// Let go of the outputs of jobs no longer `shown`, keeping a chosen selection.
    pub fn retain(&mut self, shown: impl Fn(&JobId) -> bool) {
        self.entries.retain(|job, entry| {
            let shown = shown(job);
            if !shown {
                entry.cached = None;
            }
            shown || entry.query.is_some() || entry.pending.is_some()
        });
        let entries = &self.entries;
        self.wanted.retain(|job| entries.contains_key(job));
    }
    pub fn begin(&mut self, job: JobId) -> Option<(OutputAttempt, JobOutputQuery)> {
        if Arc::strong_count(&self.loads) > MAX_LOADS {
            return None;
        }
        let entry = self.entries.entry(job).or_default();
        if entry.pending.is_some() {
            return None;
        }
        self.wanted.retain(|wanted| *wanted != job);
        let attempt = OutputAttempt {
            job,
            identity: Token::default(),
            _load: self.loads.clone(),
        };
        entry.pending = Some((attempt.identity.clone(), entry.query.clone()));
        let query = entry
            .query
            .clone()
            .unwrap_or_else(|| JobOutputQuery::new(job));
        Some((attempt, query))
    }
    /// Install a load's result. Returns the job when its shown output changed: not
    /// when its result is stale or unchanged. Stale completion cannot clear a newer
    /// refresh, replace a newer query's output, repaint it, or install output into
    /// another session.
    pub fn complete(&mut self, attempt: OutputAttempt, output: LoadedOutput) -> Option<JobId> {
        let job = attempt.job;
        let entry = self.entries.get_mut(&job)?;
        let matches = |(pending, _): &mut (Token, _)| pending.matches(&attempt.identity);
        let (_, chosen) = entry.pending.take_if(matches)?;
        let changed = entry.cached.as_ref().map(|(_, cached)| cached) != Some(&output);
        entry.cached = Some((chosen, output));
        changed.then_some(job)
    }
}

/// A store holding exactly these outputs, as if each had loaded.
impl FromIterator<(JobId, LoadedOutput)> for OutputStore {
    fn from_iter<I: IntoIterator<Item = (JobId, LoadedOutput)>>(outputs: I) -> Self {
        let entries = outputs.into_iter().map(|(job, output)| {
            let entry = OutputEntry {
                cached: Some((None, output)),
                ..OutputEntry::default()
            };
            (job, entry)
        });
        Self {
            entries: entries.collect(),
            ..Self::default()
        }
    }
}

impl App {
    /// Load output for the open job cards among `shown` entries, the first
    /// shown first, as load slots allow.
    pub(in crate::tui) fn load_shown_output(&mut self, shown: std::ops::Range<usize>) {
        let entries = &self.content_cache.entries()[shown];
        let cards = entries.iter().filter(|entry| entry.open());
        let jobs = cards.filter_map(|entry| entry.job_id());
        let (projection, live, window) = (&self.projection, &self.snapshot, self.window.as_ref());
        let jobs = jobs.filter(|job| projection.job(live, window, job).is_some());
        let shown: Vec<_> = jobs.collect();
        for job in self.outputs.wanted(&shown) {
            self.fetch_output(job);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::tool_view::tests::view;
    use serde_json::json;

    impl OutputAttempt {
        pub(in crate::tui) fn job(&self) -> JobId {
            self.job
        }
    }

    impl OutputStore {
        /// How many loads are in flight.
        pub(in crate::tui) fn loading(&self) -> usize {
            Arc::strong_count(&self.loads) - 1
        }
    }

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
        assert!(store.complete(old, value("old")).is_none());
        assert!(store.begin(job()).is_none() && store.get(&job()).is_none());
        assert_eq!(store.complete(current, value("new")), Some(job()));
        assert!(store.wanted(&[job()]).is_empty());
        assert_eq!(store.get(&job()), Some(&value("new")));
        // Another query's page shows until that query's loads, but continues nothing.
        store.clear_query(job());
        assert!(store.get(&job()).is_some() && store.current(&job()).is_none());
    }
    #[test]
    fn cache_survives_refresh_equal_results_do_not_repaint_and_errors_are_shown() {
        let mut store = OutputStore::default();
        let (first, _) = store.begin(job()).unwrap();
        assert_eq!(store.complete(first, value("cached")), Some(job()));
        let (refresh, _) = store.begin(job()).unwrap();
        assert_eq!(store.get(&job()), Some(&value("cached")));
        assert!(store.complete(refresh, value("cached")).is_none());
        assert!(store.wanted(&[job()]).is_empty());
        store.clear_query(job());
        assert_eq!(store.wanted(&[job()]), [job()]);
        assert_eq!(store.get(&job()), Some(&value("cached")));
        let (failed, _) = store.begin(job()).unwrap();
        let failure = || Err("unavailable".into());
        assert_eq!(store.complete(failed, failure()), Some(job()));
        assert!(store.wanted(&[job()]).is_empty());
        assert_eq!(store.get(&job()), Some(&failure()));
        // Changes are read in the order they came, a chosen selection first.
        let other = JobId::new(2).unwrap();
        let (loaded, _) = store.begin(other).unwrap();
        store.complete(loaded, value("other"));
        store.changed(job());
        store.changed(other);
        assert_eq!(store.wanted(&[other, job()]), [job(), other]);
        store.clear_query(other);
        assert_eq!(store.wanted(&[other, job()]), [other, job()]);
    }
    /// With every card open, a frame loads the output of at most `MAX_LOADS` of
    /// those shown, however many are.
    #[tokio::test]
    async fn open_cards_load_at_most_the_capped_outputs_at_once() {
        use crate::tui::app::tests::*;
        let (_root, app) = fixture().await;
        let session = app.session().unwrap().clone();
        for _ in 0..2 * MAX_LOADS {
            bounded(session.run_script("return 1;")).await.unwrap();
        }
        let (_viewer, mut app) = viewing(&session, model::Tab::Jobs).await;
        let _work = capture_work(&mut app);
        app.command(crate::tui::keys::Command::Details);
        draw_sized(&mut app, 80, 200);
        let open = |entry: &crate::tui::model::Entry| entry.open();
        let shown = crate::tui::render::visible(&app);
        assert!(
            app.entries()[shown]
                .iter()
                .filter(|entry| open(entry))
                .count()
                > MAX_LOADS
        );
        assert_eq!(app.outputs.loading(), MAX_LOADS);
        draw_sized(&mut app, 80, 200);
        assert_eq!(app.outputs.loading(), MAX_LOADS);
    }
    /// A shown card follows what its running job writes, with nothing but the
    /// session's notices driving it, then shows the job's structured result.
    #[tokio::test]
    async fn a_shown_card_follows_its_output_then_its_result() {
        use crate::tui::app::tests::*;
        let (_root, mut app, session) = approving_fixture().await;
        // The command never spells out what it prints, which its card also shows.
        let command = "printf 'live %s\\n' stdout; printf 'live %s\\n' stderr >&2; \
            while [ ! -e release ]; do sleep 0.01; done; printf 'more %s\\n' stdout; \
            while [ ! -e done ]; do sleep 0.01; done; exit 1";
        let call = json!({"command": command, "timeout": 10, "bg": true});
        let launched = session.run_script(format!("return await tool.exec({call});"));
        let launched = launched.await.unwrap();
        let job: JobId = serde_json::from_value(launched.value["value"]["id"].clone()).unwrap();
        app.command(crate::tui::keys::Command::Details);
        let mut work = capture_work(&mut app);
        let mut shows = async |app: &mut App, shown: &dyn Fn(&App) -> bool| {
            bounded(async {
                loop {
                    draw(app);
                    if shown(app) {
                        break;
                    }
                    tokio::select! {
                        event = app.recv_observation() => {
                            let records = app.observe(event.unwrap());
                            app.observed(records);
                        }
                        Some(done) = work.recv() => app.work(done),
                    }
                }
            })
            .await
        };
        let card = |app: &App| {
            let mut entries = app.entries().iter();
            let card = entries.find(|entry| entry.job_id() == Some(job));
            card.map_or_else(String::new, |card| card.text().to_owned())
        };
        shows(&mut app, &|app| {
            let card = card(app);
            card.contains("live stdout") && card.contains("live stderr")
        })
        .await;
        std::fs::write(app.launch.workspace.join("release"), "").unwrap();
        shows(&mut app, &|app| card(app).contains("more stdout")).await;
        assert!(!app.snapshot.jobs[&job].state.is_terminal());
        std::fs::write(app.launch.workspace.join("done"), "").unwrap();
        let result = |app: &App| {
            let output = app.outputs.get(&job)?.as_ref().ok()?;
            output.result.clone()
        };
        shows(&mut app, &|app| result(app).is_some()).await;
        let complete = result(&app).unwrap();
        let expected = json!({"exit_code": 1, "stdout": "live stdout\nmore stdout\n", "stderr": "live stderr\n"});
        for field in ["exit_code", "stdout", "stderr"] {
            assert_eq!(complete[field], expected[field]);
        }

        let stderr: skyhook::job::FieldPointer = "/result/stderr".parse().unwrap();
        let mut query = JobOutputQuery::new(job);
        query.field = Some(stderr.clone());
        app.outputs.set_query(query);
        let selected = fetch_output(&mut app, job).await;
        let presentation = selected.presentation.unwrap();
        assert_eq!(
            presentation.preview.as_ref().unwrap().field(),
            Some(&stderr)
        );
        let captures = presentation.captures();
        assert!(captures.iter().all(|capture| capture.output.is_none()));
        assert_eq!(app.outputs.query(job).unwrap().field, Some(stderr));
        session.shutdown().await.unwrap();
    }
}
