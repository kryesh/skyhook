//! Palettes over the conversation, one module per kind that needs more than a row.
use super::{App, HostRequest, QueuedInputId};
use crate::tui::editor::{Editor, TextField};
use crate::tui::keys::Command;
use crate::tui::model;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers as M};
use skyhook::identity::{AgentId, JobId};
use skyhook::provider::profile::ModelRef;
use skyhook::tool::policy::ModeName;
use std::path::PathBuf;

mod commands;
mod draft;
mod files;
mod output;
mod sessions;

pub use draft::DraftItem;
pub use output::OutputAction;
pub use sessions::SessionRef;

/// Rows PageUp and PageDown move a menu's selection by.
const MENU_PAGE: usize = 10;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MenuId(u64);

// Only asynchronous menu kinds can cross the Work boundary. A completion names
// the menu it was loaded for (and, for output, its job) and only fills that
// menu while it is still the open one of the same kind.
pub enum MenuLoaded {
    Sessions(MenuId, Result<Vec<Item<SessionRef>>, String>),
    Files(MenuId, Vec<Item<PathBuf>>),
    Output(MenuId, JobId, Result<Vec<Item<OutputAction>>, String>),
}

#[derive(Clone)]
pub enum ConfirmAction {
    Exit,
    CloseSession,
    CancelJob(JobId),
}
#[derive(Clone)]
pub enum ConfirmChoice {
    KeepWorking,
    Proceed(ConfirmAction),
}
#[derive(Clone)]
pub struct Item<T> {
    pub value: T,
    pub label: String,
    /// Searchable metadata, rendered separately from the label.
    pub detail: String,
    /// The label and detail lowercased once, for filtering on every key.
    search: String,
}
impl<T> Item<T> {
    pub(super) fn new(value: T, label: impl Into<String>, detail: impl Into<String>) -> Self {
        let (label, detail) = (label.into(), detail.into());
        let search = format!("{label} {detail}").to_lowercase();
        Self {
            value,
            label,
            detail,
            search,
        }
    }
}
#[derive(Clone)]
pub enum MenuKind {
    Commands(Vec<Item<Command>>),
    Models(Vec<Item<ModelRef>>),
    Modes(Vec<Item<ModeName>>),
    Agents(Vec<Item<AgentId>>),
    Sessions(Vec<Item<SessionRef>>),
    /// Workspace files, plus the composer offset just after the `@` that opened
    /// the picker. A chosen file replaces that `@`; cancelling keeps it.
    Files(Vec<Item<PathBuf>>, Option<usize>),
    Attachments(Vec<Item<DraftItem>>),
    Queue(Vec<Item<QueuedInputId>>),
    Confirm(Vec<Item<ConfirmChoice>>),
    Output(JobId, Vec<Item<OutputAction>>),
    OutputSearch(JobId),
    Info(Vec<Item<()>>),
}
/// Rendering and filtering borrow only presentation data; selection stays typed.
pub struct ItemRef<'a> {
    pub index: usize,
    pub label: &'a str,
    pub detail: &'a str,
    pub search: &'a str,
}
impl MenuKind {
    pub fn items(&self) -> Vec<ItemRef<'_>> {
        fn rows<T>(items: &[Item<T>]) -> Vec<ItemRef<'_>> {
            items
                .iter()
                .enumerate()
                .map(|(index, item)| ItemRef {
                    index,
                    label: &item.label,
                    detail: &item.detail,
                    search: &item.search,
                })
                .collect()
        }
        match self {
            Self::Commands(items) => rows(items),
            Self::Models(items) => rows(items),
            Self::Modes(items) => rows(items),
            Self::Agents(items) => rows(items),
            Self::Sessions(items) => rows(items),
            Self::Files(items, _) => rows(items),
            Self::Attachments(items) => rows(items),
            Self::Queue(items) => rows(items),
            Self::Confirm(items) => rows(items),
            Self::Output(_, items) => rows(items),
            Self::Info(items) => rows(items),
            Self::OutputSearch(_) => vec![],
        }
    }
    /// Where the row at `index` of `previous` sits among these rows of the same kind.
    fn find(&self, previous: &Self, index: usize) -> Option<usize> {
        fn find<T: PartialEq>(items: &[Item<T>], row: &Item<T>) -> Option<usize> {
            items.iter().position(|item| item.value == row.value)
        }
        match (self, previous) {
            (Self::Agents(items), Self::Agents(previous)) => find(items, &previous[index]),
            (Self::Attachments(items), Self::Attachments(rows)) => find(items, &rows[index]),
            (Self::Queue(items), Self::Queue(previous)) => find(items, &previous[index]),
            _ => None,
        }
    }
}
pub struct Menu {
    pub(super) id: MenuId,
    pub title: String,
    pub kind: MenuKind,
    pub input: Editor,
    pub selected: usize,
}
impl Menu {
    /// Replace the rows of an open menu, keeping the selected value selected.
    fn replace_items(&mut self, kind: MenuKind) {
        let index = self.selected_index();
        let previous = std::mem::replace(&mut self.kind, kind);
        let index = index.and_then(|index| self.kind.find(&previous, index));
        let selected =
            index.and_then(|index| self.filtered().iter().position(|row| row.index == index));
        self.selected = selected.unwrap_or(0);
    }

    pub fn filtered(&self) -> Vec<ItemRef<'_>> {
        let query = self.input.text().to_lowercase();
        let commands = if let MenuKind::Commands(items) = &self.kind {
            Some(items)
        } else {
            None
        };
        let query = if commands.is_some() {
            query.trim_start_matches('/')
        } else {
            &query
        };
        let query = commands
            .and_then(|_| query.parse::<Command>().ok())
            .map_or(query, |command| command.id());
        let mut items: Vec<_> = self
            .kind
            .items()
            .into_iter()
            .filter(|item| {
                commands.is_some_and(|commands| commands[item.index].value.id().contains(query))
                    || item.search.contains(query)
            })
            .collect();
        if let Some(commands) = commands {
            // A typed command id selects that command ahead of label matches.
            items.sort_by_key(|item| commands[item.index].value.id() != query);
        }
        items
    }
    pub(super) fn selected_index(&self) -> Option<usize> {
        self.filtered().get(self.selected).map(|item| item.index)
    }
}

/// What sits over the conversation and takes the keys.
pub enum Overlay {
    Menu(Menu),
    /// The query being typed for `/` search.
    Search(Editor),
}

impl App {
    pub fn menu(&self) -> Option<&Menu> {
        match &self.overlay {
            Some(Overlay::Menu(menu)) => Some(menu),
            _ => None,
        }
    }
    pub fn menu_mut(&mut self) -> Option<&mut Menu> {
        match &mut self.overlay {
            Some(Overlay::Menu(menu)) => Some(menu),
            _ => None,
        }
    }
    /// Rows name their agent; the menu draws its live status and statistics.
    /// The status is also searchable, so the rows follow every change to it.
    fn agent_items(&self) -> Vec<Item<AgentId>> {
        let agents = self.projection.agents.iter();
        let items = agents.map(|agent| {
            let label = format!("{}{}", agent.name, model::target_suffix(&agent.target));
            Item::new(agent.id.clone(), label, self.agent_status(agent).label())
        });
        items.collect()
    }
    /// Rebuild an open menu that lists live state, keeping its selection.
    pub(in crate::tui) fn refresh_menu(&mut self) {
        let kind = match self.menu().map(|menu| &menu.kind) {
            Some(MenuKind::Agents(_)) => MenuKind::Agents(self.agent_items()),
            Some(MenuKind::Attachments(_)) => MenuKind::Attachments(self.attachment_items()),
            Some(MenuKind::Queue(_)) => MenuKind::Queue(self.queue_items()),
            _ => return,
        };
        self.menu_mut().unwrap().replace_items(kind);
    }
    pub(super) fn open(&mut self, title: &str, kind: MenuKind) {
        self.next_menu_id.0 = self.next_menu_id.0.wrapping_add(1);
        self.overlay = Some(Overlay::Menu(Menu {
            id: self.next_menu_id,
            title: title.into(),
            kind,
            input: Editor::default(),
            selected: 0,
        }));
    }
    pub(super) fn menu_loaded(&mut self, loaded: MenuLoaded) -> bool {
        let Some(menu) = self.menu_mut() else {
            return false;
        };
        let (id, result) = match (loaded, &menu.kind) {
            (MenuLoaded::Sessions(id, result), MenuKind::Sessions(_)) => {
                (id, result.map(MenuKind::Sessions))
            }
            (MenuLoaded::Files(id, items), &MenuKind::Files(_, at)) => {
                (id, Ok(MenuKind::Files(items, at)))
            }
            // The menu id is minted per open, so it already identifies the job.
            (MenuLoaded::Output(id, job, result), MenuKind::Output(..)) => {
                (id, result.map(|items| MenuKind::Output(job, items)))
            }
            _ => return false,
        };
        if id != menu.id {
            return false;
        }
        match result {
            Ok(kind) => menu.kind = kind,
            Err(error) => self.notice(error),
        }
        true
    }

    pub(super) fn info(&mut self, title: &str, text: String) {
        self.open(
            title,
            MenuKind::Info(text.lines().map(|line| Item::new((), line, "")).collect()),
        );
    }
    pub(super) fn confirm(&mut self, action: ConfirmAction) {
        let items = vec![
            Item::new(ConfirmChoice::KeepWorking, "Keep working", ""),
            Item::new(ConfirmChoice::Proceed(action), "Stop work and continue", ""),
        ];
        self.open("Confirm action", MenuKind::Confirm(items));
    }
    pub(super) fn menu_key(&mut self, mut key: KeyEvent) {
        if key.modifiers.contains(M::CONTROL) {
            if key.code == KeyCode::Char('p') {
                key.code = KeyCode::Up;
            }
            if key.code == KeyCode::Char('n') {
                key.code = KeyCode::Down;
            }
        }
        let Some(menu) = self.menu_mut() else {
            return;
        };
        let last = menu.filtered().len().saturating_sub(1);
        let step = match key.code {
            KeyCode::PageUp | KeyCode::PageDown => MENU_PAGE,
            _ => 1,
        };
        let selected = match key.code {
            KeyCode::Home => Some(0),
            KeyCode::End => Some(last),
            KeyCode::Up | KeyCode::PageUp => Some(menu.selected.saturating_sub(step)),
            KeyCode::Down | KeyCode::PageDown => Some((menu.selected + step).min(last)),
            _ => None,
        };
        if let Some(selected) = selected {
            menu.selected = selected;
            return;
        }
        match key.code {
            KeyCode::Esc => self.overlay = None,
            KeyCode::Enter | KeyCode::Tab => self.choose(),
            KeyCode::Delete => self.delete_draft_item(),
            _ => {
                menu.input.handle(key);
                menu.selected = 0;
            }
        }
    }
    pub(super) fn choose(&mut self) {
        let menu = self
            .overlay
            .take_if(|overlay| matches!(overlay, Overlay::Menu(_)));
        let Some(Overlay::Menu(menu)) = menu else {
            return;
        };
        let index = menu.selected_index();
        match (menu.kind, index) {
            (MenuKind::OutputSearch(job), _) => self.search_output(job, menu.input.text()),
            (kind @ MenuKind::Info(_), _) => {
                self.overlay = Some(Overlay::Menu(Menu { kind, ..menu }))
            }
            (_, None) => {}
            (MenuKind::Commands(items), Some(index)) => self.command(items[index].value),
            (MenuKind::Models(items), Some(index)) => self.model.clone_from(&items[index].value),
            (MenuKind::Modes(items), Some(index)) => self.mode.clone_from(&items[index].value),
            (MenuKind::Agents(items), Some(index)) => self.select(items[index].value.clone()),
            (MenuKind::Sessions(items), Some(index)) => {
                self.host = Some(match items[index].value {
                    SessionRef::Live(key) => HostRequest::Activate(key),
                    SessionRef::Saved(id) => HostRequest::Open(id),
                });
            }
            (MenuKind::Files(items, at), Some(index)) => self.attach_file(&items[index].value, at),
            (MenuKind::Attachments(items), Some(index)) => {
                self.inspect_draft_item(items[index].value)
            }
            (MenuKind::Queue(items), Some(index)) => self.edit_queued(items[index].value),
            (MenuKind::Confirm(items), Some(index)) => match items[index].value.clone() {
                ConfirmChoice::KeepWorking => {}
                ConfirmChoice::Proceed(ConfirmAction::Exit) => self.host = Some(HostRequest::Quit),
                ConfirmChoice::Proceed(ConfirmAction::CloseSession) => self.shutdown(),
                ConfirmChoice::Proceed(ConfirmAction::CancelJob(id)) => {
                    let Some(session) = self.session().cloned() else {
                        return;
                    };
                    let notices = self.notifier();
                    tokio::spawn(async move {
                        notices.send(match session.cancel_job(id).await {
                            Ok(job) => format!("Job {id}: {}", model::state_name(job.state)),
                            Err(e) => e.to_string(),
                        });
                    });
                }
            },
            (MenuKind::Output(job, items), Some(index)) => {
                self.choose_output(job, &items[index].value)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    use crate::tui::app::{Hit, SlotKey, Work};
    use crossterm::event::MouseEventKind;
    use ratatui::layout::Rect;
    use tokio::sync::mpsc;

    fn menu_completion(menu: &Menu, error: Option<&str>) -> Work {
        fn result<T>(value: T, error: Option<&str>) -> Result<Vec<Item<T>>, String> {
            match error {
                Some(error) => Err(error.into()),
                None => Ok(vec![Item::new(value, "current", "loaded detail")]),
            }
        }
        Work::MenuLoaded(match &menu.kind {
            MenuKind::Sessions(_) => {
                MenuLoaded::Sessions(menu.id, result(SessionRef::Live(SlotKey::default()), error))
            }
            MenuKind::Files(..) => MenuLoaded::Files(
                menu.id,
                vec![Item::new(PathBuf::from("current"), "current", "")],
            ),
            MenuKind::Output(job, _) => {
                MenuLoaded::Output(menu.id, *job, result(OutputAction::Automatic, error))
            }
            _ => panic!("not an asynchronous menu"),
        })
    }

    #[tokio::test]
    async fn menu_loads_only_fill_the_originating_open_menu() {
        let (_root, mut app) = draft_fixture().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.status = crate::tui::status::StatusLog::new(tx);
        for (kind, fallible) in [
            (MenuKind::Sessions(vec![]), true),
            (MenuKind::Files(vec![], None), false),
            (MenuKind::Output(JobId::new(42).unwrap(), vec![]), true),
        ] {
            app.open("Loading", kind.clone());
            let complete = |app: &App, error| menu_completion(app.menu().unwrap(), error);
            let closed_success = complete(&app, None);
            let closed_failure = complete(&app, Some("obsolete failure"));
            let stale_success = complete(&app, None);
            key(&mut app, KeyCode::Esc, M::NONE);
            app.dirty = false;
            app.work(closed_success);
            assert!(app.menu().is_none());
            assert!(!app.dirty);

            app.open("Newer request", kind);
            app.menu_mut().unwrap().input.insert("query");
            app.work(closed_failure);
            app.work(stale_success);
            assert!(app.menu().unwrap().kind.items().is_empty());
            assert!(!app.dirty);
            // A current failure keeps the menu and reports a notice.
            if fallible {
                app.work(complete(&app, Some("current failure")));
                assert_eq!(app.menu().unwrap().title, "Newer request");
                assert!(app.menu().unwrap().kind.items().is_empty());
                app.status.flush().await;
                assert!(
                    matches!(rx.try_recv().unwrap(), Work::LocalStatus { message, .. } if message == "current failure")
                );
            }
            let (current, replaced) = (complete(&app, None), complete(&app, None));
            app.work(current);
            let menu = app.menu().unwrap();
            assert_eq!(menu.kind.items()[0].label, "current");
            assert_eq!(menu.input.text(), "query");

            app.info("Replacement overlay", "Keep me".into());
            app.work(replaced);
            assert_eq!(app.menu().unwrap().title, "Replacement overlay");
        }
        app.status.flush().await;
        assert!(
            rx.try_recv().is_err(),
            "stale failures must not emit notices"
        );
    }

    #[tokio::test]
    async fn palette_hover_owns_selection_without_background_or_stationary_updates() {
        let (_root, mut app) = fixture().await;
        // The palette's selected value must belong to the admitted catalog.
        let mut config = app.launch.model.config().config().clone();
        let models = &mut config.providers["test"].common.models;
        for name in ["second", "third"] {
            let first = models["first"].clone();
            models.insert(name.parse().unwrap(), first);
        }
        app.launch.model = config
            .into_runtime()
            .unwrap()
            .select_model(&"test/first".parse().unwrap())
            .unwrap();
        let models = |labels: [(&str, &str); 3]| {
            let items = labels.map(|(value, label)| Item::new(value.parse().unwrap(), label, ""));
            MenuKind::Models(items.into())
        };
        let row = |app: &mut App, index| {
            draw(app);
            let mut hits = app.hits.iter();
            hits.find_map(|(rect, hit)| matches!(hit, Hit::Menu(i) if *i == index).then_some(*rect))
                .unwrap()
        };
        app.open(
            "Models",
            models([
                ("test/first", "First"),
                ("test/second", "Second"),
                ("test/third", "Third"),
            ]),
        );
        let second = row(&mut app, 1);
        let selected = |app: &App| app.menu().unwrap().selected;
        mouse(&mut app, second, MouseEventKind::Moved);
        assert_eq!(selected(&app), 1);
        key(&mut app, KeyCode::Down, M::NONE);
        assert_eq!(selected(&app), 2);
        app.dirty = false;
        mouse(&mut app, second, MouseEventKind::Moved);
        assert_eq!(selected(&app), 2);
        assert!(!app.dirty);
        // A physical move inside the same row can take over from the keyboard.
        let moved = Rect {
            x: second.x + 1,
            ..second
        };
        mouse(&mut app, moved, MouseEventKind::Moved);
        assert_eq!(selected(&app), 1);
        let selected_agent = app.selected.clone();
        app.dirty = false;
        let tree_rect = app.tree_rect;
        mouse(&mut app, tree_rect, MouseEventKind::Moved);
        assert_eq!(selected(&app), 1);
        assert_eq!(app.selected, selected_agent);
        assert!(!app.dirty);
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(app.menu().is_none());
        assert_eq!(app.model, "test/second".parse().unwrap());
        let filtered = [
            ("test/hidden", "Hidden"),
            ("test/first", "Visible first"),
            ("test/second", "Visible second"),
        ];
        app.open("Models", models(filtered));
        app.menu_mut().unwrap().input.set("Visible".into());
        let visible_second = row(&mut app, 1);
        mouse(&mut app, visible_second, MouseEventKind::Moved);
        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(app.model, "test/second".parse().unwrap());
    }
}
