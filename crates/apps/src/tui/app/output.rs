//! The App side of job output: which shown cards load, as load slots allow.
use super::App;

impl App {
    /// Load output for the open job cards among `shown` entries, the first
    /// shown first, as load slots allow.
    pub(crate) fn load_shown_output(&mut self, shown: std::ops::Range<usize>) {
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
    use crate::tui::model;
    use crate::tui::model::MAX_LOADS;
    use serde_json::json;
    use skyhook_worker::job::JobId;

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
        app.command(crate::tui::commands::Command::Details);
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
        app.command(crate::tui::commands::Command::Details);
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
        std::fs::write(app.launch.workspace().join("release"), "").unwrap();
        shows(&mut app, &|app| card(app).contains("more stdout")).await;
        assert!(!app.snapshot.jobs[&job].state.is_terminal());
        std::fs::write(app.launch.workspace().join("done"), "").unwrap();
        let result = |app: &App| {
            let (output, _) = app.outputs.get(&job)?.as_ref().ok()?;
            output.result.clone()
        };
        shows(&mut app, &|app| result(app).is_some()).await;
        let complete = result(&app).unwrap();
        let expected = json!({"exit": {"code": 1}, "stdout": "live stdout\nmore stdout\n", "stderr": "live stderr\n"});
        for field in ["exit", "stdout", "stderr"] {
            assert_eq!(complete[field], expected[field]);
        }
        session.shutdown().await.unwrap();
    }
}
