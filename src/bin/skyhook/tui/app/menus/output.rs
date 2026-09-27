//! Choosing which part of a job's saved output its card shows.
use super::{Item, MenuKind, MenuLoaded};
use crate::tui::app::{App, Work};
use skyhook::identity::JobId;
use skyhook::job::{Continuation, FieldPointer, JobOutputQuery, OutputFields};

const OUTPUT_SEARCH_CONTEXT: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputAction {
    Automatic,
    Field(FieldPointer),
    /// Show `field` from its child at `index`, which lists the fields from there.
    MoreFields {
        field: FieldPointer,
        index: usize,
    },
    Search,
    Next,
}

/// The fields one level below `parent`, a way back up, and the page actions.
fn output_items(parent: &FieldPointer, listed: OutputFields) -> Vec<Item<OutputAction>> {
    let mut items: Vec<_> = (listed.fields.into_iter())
        .map(|field| Item::new(OutputAction::Field(field.clone()), field.to_string(), ""))
        .collect();
    if let Some(index) = listed.next_index {
        let more = OutputAction::MoreFields {
            field: parent.clone(),
            index,
        };
        items.push(Item::new(more, "more fields", format!("from {index}")));
    }
    if let Some(up) = parent
        .parent()
        .filter(|_| *parent != FieldPointer::result())
    {
        let label = up.to_string();
        items.push(Item::new(OutputAction::Field(up), label, "parent field"));
    }
    items.extend([
        Item::new(
            OutputAction::Automatic,
            "automatic output",
            "structured result and live captures",
        ),
        Item::new(
            OutputAction::Field(FieldPointer::root()),
            "complete result",
            "",
        ),
        Item::new(OutputAction::Search, "Search this field", "regex"),
        Item::new(OutputAction::Next, "Next page", ""),
    ]);
    items
}

impl App {
    pub(in crate::tui::app) fn output_menu(&mut self) {
        let row = self.view().row;
        let Some(job) = self.entries().get(row).and_then(|e| e.job_id()) else {
            return;
        };
        // Fields are listed a level at a time, below the one shown and from the
        // page of its members or elements shown; a query's index counts matches.
        let query = self.outputs.query(job);
        let index = (query.filter(|query| query.query.is_none()))
            .and_then(|query| query.index)
            .unwrap_or(0);
        let parent = (query.and_then(|query| query.field.clone()))
            .filter(|field| !field.is_root())
            .unwrap_or_else(FieldPointer::result);
        let none = OutputFields {
            fields: Vec::new(),
            next_index: None,
        };
        self.open(
            "Saved output",
            MenuKind::Output(job, output_items(&parent, none)),
        );
        let Some(session) = self.session().cloned() else {
            return;
        };
        let id = self.menu().unwrap().id;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            // Discover saved pointers, not paths through presentation-only wrappers.
            let result = session
                .inspect_output_fields(job, parent.clone(), index)
                .await
                .map(|listed| output_items(&parent, listed))
                .map_err(|error| error.to_string());
            let _ = tx.send(Work::MenuLoaded(MenuLoaded::Output(id, job, result)));
        });
    }
    pub(super) fn choose_output(&mut self, job: JobId, action: &OutputAction) {
        match action {
            OutputAction::Automatic => {
                self.outputs.clear_query(job);
                self.fetch_output(job);
            }
            OutputAction::Search => {
                self.open("Search saved output (regex)", MenuKind::OutputSearch(job));
            }
            OutputAction::Next => {
                let output = self.outputs.get(&job);
                let next = output.and_then(|output| output.as_ref().ok()?.continuation());
                let Some(next) = next else {
                    return;
                };
                let mut query =
                    (self.outputs.query(job).cloned()).unwrap_or_else(|| JobOutputQuery::new(job));
                if let Some(field) = next.field() {
                    query.field = Some(field.clone());
                }
                (query.start, query.offset, query.index) = match next {
                    Continuation::Lines { start, offset, .. } => (Some(start), offset, None),
                    Continuation::Index { index, .. } => (None, None, Some(index)),
                };
                self.set_output_query(query);
            }
            OutputAction::Field(field) => {
                let mut query = JobOutputQuery::new(job);
                query.field = Some(field.clone());
                self.set_output_query(query);
            }
            OutputAction::MoreFields { field, index } => {
                let mut query = JobOutputQuery::new(job);
                (query.field, query.index) = (Some(field.clone()), Some(*index));
                self.set_output_query(query);
            }
        }
    }
    pub(super) fn search_output(&mut self, job: JobId, pattern: &str) {
        let field = self.outputs.query(job).and_then(|q| q.field.clone());
        let mut query = JobOutputQuery::new(job);
        query.field = Some(field.unwrap_or_default());
        query.pattern = Some(pattern.to_owned());
        query.context = Some(OUTPUT_SEARCH_CONTEXT);
        self.set_output_query(query);
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::tests::*;
    use super::*;
    use crate::tui::keys::Command;
    use skyhook::job::OutputPreview;

    fn offered(app: &mut App, field: &str) -> Option<usize> {
        let MenuKind::Output(_, items) = &app.menu_mut().unwrap().kind else {
            panic!("output menu")
        };
        let field = OutputAction::Field(field.parse().unwrap());
        items.iter().position(|item| item.value == field)
    }

    #[tokio::test]
    async fn output_menu_lists_saved_fields_a_level_below_the_shown_one() {
        let (_root, mut app) = fixture().await;
        std::fs::write(app.launch.workspace.join("child.txt"), "child data").unwrap();
        let source =
            "return {custom: {'a/b~c': [42]}, child: await tool.read({path: 'child.txt'})};";
        run_script(&mut app, source).await;
        app.command(Command::Details);
        draw(&mut app);
        let job = job_named(&app, "script");
        select_job(&mut app, job);
        for (shown, listed, absent) in [
            // A nested job view offers its saved result, and the way back up.
            (
                "/result/value/child",
                "/result/value/child/result",
                "/result/value/child/result/content",
            ),
            (
                "/result/value/custom/a~1b~0c",
                "/result/value/custom/a~1b~0c/0",
                "/result/value/child",
            ),
        ] {
            let mut query = JobOutputQuery::new(job);
            query.field = Some(shown.parse().unwrap());
            app.outputs.set_query(query);
            let page = fetch_output(&mut app, job).await;
            let mut rx = capture_work(&mut app);
            app.output_menu();
            let work = recv(&mut rx).await;
            assert!(matches!(
                work,
                Work::MenuLoaded(MenuLoaded::Output(_, _, Ok(_)))
            ));
            app.work(work);
            // Discovery must not replace the displayed page.
            assert_eq!(app.outputs.get(&job), Some(&Ok(page)));
            let parent = FieldPointer::parent(&shown.parse().unwrap()).unwrap();
            assert!(offered(&mut app, parent.as_str()).is_some());
            assert!(offered(&mut app, listed).is_some());
            assert!(offered(&mut app, absent).is_none());
        }
        // Choosing a field refreshes the card with that field's page.
        let mut rx = capture_work(&mut app);
        let element = "/result/value/custom/a~1b~0c/0";
        let chosen = offered(&mut app, element).unwrap();
        app.menu_mut().unwrap().selected = chosen;
        app.choose();
        assert_eq!(
            app.outputs.query(job).unwrap().field,
            Some(element.parse().unwrap())
        );
        let work = recv(&mut rx).await;
        app.work(work);
        let selected = app.outputs.get(&job).unwrap().as_ref().unwrap();
        let preview = selected.presentation().and_then(|p| p.preview());
        let Some(OutputPreview::Lines(page)) = preview else {
            panic!("a scalar pages as text: {preview:?}")
        };
        assert_eq!(page.lines(), ["42"]);
    }

    #[tokio::test]
    async fn more_fields_continue_the_listing_past_its_limit() {
        let (_root, mut app) = fixture().await;
        let source =
            "return Object.fromEntries(Array.from({length: 101}, (_, i) => [`k${i}`, i]));";
        run_script(&mut app, source).await;
        app.command(Command::Details);
        draw(&mut app);
        let job = job_named(&app, "script");
        select_job(&mut app, job);
        let value: FieldPointer = "/result/value".parse().unwrap();
        let mut query = JobOutputQuery::new(job);
        query.field = Some(value.clone());
        app.outputs.set_query(query);
        fetch_output(&mut app, job).await;
        let open_menu = async |app: &mut App| {
            let mut rx = capture_work(app);
            app.output_menu();
            let work = recv(&mut rx).await;
            app.work(work);
        };
        open_menu(&mut app).await;
        assert!(offered(&mut app, "/result/value/k99").is_some());
        assert!(offered(&mut app, "/result/value/k100").is_none());
        let MenuKind::Output(_, items) = &app.menu_mut().unwrap().kind else {
            panic!("output menu")
        };
        let more = OutputAction::MoreFields {
            field: value.clone(),
            index: 100,
        };
        let chosen = items.iter().position(|item| item.value == more).unwrap();
        let mut rx = capture_work(&mut app);
        app.menu_mut().unwrap().selected = chosen;
        app.choose();
        let work = recv(&mut rx).await;
        app.work(work);
        open_menu(&mut app).await;
        assert!(offered(&mut app, "/result/value/k100").is_some());
        assert!(offered(&mut app, "/result/value/k0").is_none());
    }
}
