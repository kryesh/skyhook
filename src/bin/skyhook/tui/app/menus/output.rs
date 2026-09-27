//! Choosing which part of a job's saved output its card shows.
use super::{Item, MenuKind, MenuLoaded};
use crate::tui::app::{App, Work};
use skyhook::identity::JobId;
use skyhook::job::{FieldPointer, JobOutputQuery};

const OUTPUT_SEARCH_CONTEXT: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputAction {
    Automatic,
    Field(FieldPointer),
    Search,
    Next,
}

fn output_items(fields: Vec<FieldPointer>) -> Vec<Item<OutputAction>> {
    let mut items: Vec<_> = fields
        .into_iter()
        .map(|field| Item::new(OutputAction::Field(field.clone()), field.to_string(), ""))
        .collect();
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
        self.open(
            "Saved output",
            MenuKind::Output(job, output_items(Vec::new())),
        );
        let Some(session) = self.session().cloned() else {
            return;
        };
        let id = self.menu().unwrap().id;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            // Discover saved pointers, not paths through presentation-only
            // wrappers or a currently selected single-field page.
            let result = session
                .inspect_output_fields(job)
                .await
                .map(output_items)
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
                if let Some((field, start, offset)) = next {
                    let mut query = self
                        .outputs
                        .query(job)
                        .cloned()
                        .unwrap_or_else(|| JobOutputQuery::new(job));
                    if let Some(field) = field {
                        query.field = Some(field.clone());
                    }
                    query.start = Some(start);
                    query.offset = offset;
                    self.set_output_query(query);
                }
            }
            OutputAction::Field(field) => {
                let mut query = JobOutputQuery::new(job);
                query.field = Some(field.clone());
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

    #[tokio::test]
    async fn output_menu_chooses_a_saved_pointer_without_replacing_the_shown_page() {
        let (_root, mut app) = fixture().await;
        std::fs::write(app.launch.workspace.join("child.txt"), "child data").unwrap();
        let source =
            "return {custom: {'a/b~c': [42]}, child: await tool.read({path: 'child.txt'})};";
        run_script(&mut app, source).await;
        app.command(Command::Details);
        draw(&mut app);
        let job = job_named(&app, "script");
        select_job(&mut app, job);
        let mut query = JobOutputQuery::new(job);
        let shown: FieldPointer = "/result/value/custom".parse().unwrap();
        query.field = Some(shown.clone());
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
        assert_eq!(app.outputs.query(job).unwrap().field, Some(shown));
        let custom: FieldPointer = "/result/value/custom/a~1b~0c/0".parse().unwrap();
        let menu = app.menu_mut().unwrap();
        let MenuKind::Output(_, items) = &menu.kind else {
            panic!("output menu")
        };
        // A nested job view offers its saved result, not presentation wrappers.
        let offered = |field: &str| {
            let field = OutputAction::Field(field.parse().unwrap());
            items.iter().any(|item| item.value == field)
        };
        assert!(offered("/result/value/child/result/content"));
        assert!(!offered("/result/value/child/content"));
        let field = OutputAction::Field(custom.clone());
        menu.selected = items.iter().position(|item| item.value == field).unwrap();
        app.choose();
        assert_eq!(app.outputs.query(job).unwrap().field, Some(custom));
        // Choosing refreshes the card with the chosen field's page.
        let work = recv(&mut rx).await;
        app.work(work);
        let selected = app.outputs.get(&job).unwrap().as_ref().unwrap();
        let preview = selected.presentation().and_then(|p| p.preview()).unwrap();
        assert_eq!(preview.lines(), ["42"]);
    }
}
