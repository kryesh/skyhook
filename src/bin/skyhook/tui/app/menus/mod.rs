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
pub use sessions::{SavedState, SessionRef};

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
/// How strongly a menu row is drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Emphasis {
    #[default]
    Normal,
    Strong,
}
/// A menu row. Its text is cleaned once, so it measures as it paints.
#[derive(Clone)]
pub struct Item<T> {
    pub value: T,
    pub label: String,
    /// Searchable metadata, rendered separately from the label.
    pub detail: String,
    /// Searchable metadata aligned at the right edge, such as a shortcut or a
    /// time. Short of room, the leftmost goes first.
    pub columns: Vec<String>,
    pub emphasis: Emphasis,
    /// The label, detail and columns lowercased once, for filtering on every key.
    search: String,
}
impl<T> Item<T> {
    pub(super) fn new(value: T, label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::columned(value, label, detail, Vec::new())
    }

    fn columned(
        value: T,
        label: impl Into<String>,
        detail: impl Into<String>,
        columns: Vec<String>,
    ) -> Self {
        let (label, detail) = (model::clean(&label.into()), model::clean(&detail.into()));
        let columns: Vec<_> = columns.iter().map(|value| model::clean(value)).collect();
        let search = format!("{label} {detail} {}", columns.join(" ")).to_lowercase();
        Self {
            value,
            label,
            detail,
            columns,
            emphasis: Emphasis::Normal,
            search,
        }
    }

    fn emphasized(self, emphasis: Emphasis) -> Self {
        Self { emphasis, ..self }
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
    pub columns: &'a [String],
    pub emphasis: Emphasis,
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
                    columns: &item.columns,
                    emphasis: item.emphasis,
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
    /// The highlighted row among the filtered ones.
    selected: usize,
    /// The first filtered row in view. Each draw moves it only as far as it
    /// must to show the selection, so a pointer, which only selects rows in
    /// view, never moves it, while the keyboard, filtering and a shorter
    /// viewport do.
    top: usize,
}
impl Menu {
    fn new(id: MenuId, title: String, kind: MenuKind) -> Self {
        Self {
            id,
            title,
            kind,
            input: Editor::default(),
            selected: 0,
            top: 0,
        }
    }

    /// Replace the rows of an open menu, keeping the selected value selected.
    fn replace_items(&mut self, kind: MenuKind) {
        let index = self.selected_index();
        let previous = std::mem::replace(&mut self.kind, kind);
        let index = index.and_then(|index| self.kind.find(&previous, index));
        let selected =
            index.and_then(|index| self.filtered().iter().position(|row| row.index == index));
        self.select(selected.unwrap_or(0));
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Select a filtered row, which the next draw scrolls into view.
    pub fn select(&mut self, index: usize) {
        self.selected = index;
    }

    /// Select the filtered row under the pointer, leaving the rows in view where
    /// they are. Returns whether the selection changed.
    pub fn point(&mut self, index: usize) -> bool {
        let changed = !self.page() && index < self.filtered().len() && self.selected != index;
        if changed {
            self.selected = index;
        }
        changed
    }

    /// A text page has nothing to pick: its selection is the first visible line.
    pub fn page(&self) -> bool {
        matches!(self.kind, MenuKind::Info(_))
    }

    /// The first filtered row in view when `rows` fit, showing the selection.
    pub fn viewport(&mut self, rows: usize) -> usize {
        let count = self.filtered().len();
        self.top = if self.page() {
            self.selected
        } else {
            crate::tui::render::scroll_to(self.top, self.selected, rows.max(1))
        };
        self.top = self.top.min(count.saturating_sub(rows));
        if self.page() {
            self.selected = self.top;
        }
        self.top
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
        self.overlay = Some(Overlay::Menu(Menu::new(
            self.next_menu_id,
            title.into(),
            kind,
        )));
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
            menu.select(selected);
            return;
        }
        match key.code {
            KeyCode::Esc => self.overlay = None,
            KeyCode::Enter | KeyCode::Tab => self.choose(),
            KeyCode::Delete => self.delete_draft_item(),
            _ => {
                menu.input.handle(key);
                menu.select(0);
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
            (MenuKind::Sessions(items), Some(index)) => match items[index].value {
                SessionRef::Live(key) => self.host = Some(HostRequest::Activate(key)),
                // A lock may have been let go since the list was read: the host,
                // which knows its own closing sessions, decides.
                SessionRef::Saved(id, _) => self.host = Some(HostRequest::Open(id)),
            },
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
    use skyhook::provider::dialect::ProviderSettings;
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
        let ProviderSettings::Compatible(provider) = &mut config.providers["test"].settings else {
            panic!("compatible fixture");
        };
        let models = &mut provider.models;
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
        let selected = |app: &App| app.menu().unwrap().selected();
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

    /// The pointer picks among the rows shown without moving them, in a palette
    /// and on a text page, which it never selects beneath; the keyboard still
    /// scrolls to what it selects.
    #[tokio::test]
    async fn pointer_keeps_a_scrolled_menu_in_place_while_keys_reveal() {
        use crossterm::event::MouseButton;
        let (_root, mut app) = fixture().await;
        for index in 0..40 {
            app.local_notice(format!("Message number {index} beneath the menu"));
        }
        let rows = |app: &mut App| -> Vec<(usize, Rect)> {
            draw(app);
            let hits = app.hits.iter();
            let rows = hits.filter_map(|(rect, hit)| match hit {
                Hit::Menu(index) => Some((*index, *rect)),
                _ => None,
            });
            rows.collect()
        };
        let top = |app: &mut App| rows(app)[0].0;
        let left = MouseButton::Left;
        let drag = |app: &mut App, rows: &[(usize, Rect)]| {
            for (index, kind) in [
                (1, MouseEventKind::Moved),
                (3, MouseEventKind::Down(left)),
                (6, MouseEventKind::Drag(left)),
                (6, MouseEventKind::Up(left)),
            ] {
                mouse(app, rows[index].1, kind);
            }
        };

        let models = (0..30).map(|index| {
            let name = format!("test/model-{index}");
            Item::new(name.parse().unwrap(), name, "")
        });
        app.open("Models", MenuKind::Models(models.collect()));
        key(&mut app, KeyCode::End, M::NONE);
        let shown = rows(&mut app);
        let first = shown[0].0;
        assert!(first > 0 && shown.last().unwrap().0 == 29);
        for row in [1, 4] {
            mouse(&mut app, shown[row].1, MouseEventKind::Moved);
            mouse(&mut app, shown[row].1, MouseEventKind::Drag(left));
            assert_eq!(top(&mut app), first);
            assert_eq!(app.menu().unwrap().selected(), first + row);
        }
        // Up moves within the rows shown, then scrolls one at a time.
        for _ in 0..5 {
            key(&mut app, KeyCode::Up, M::NONE);
        }
        assert_eq!(top(&mut app), first - 1);
        key(&mut app, KeyCode::Home, M::NONE);
        assert_eq!(top(&mut app), 0);
        key(&mut app, KeyCode::End, M::NONE);
        assert_eq!(top(&mut app), first);
        // Clicking a row still chooses it.
        let shown = rows(&mut app);
        click(&mut app, shown[2].1);
        assert!(app.menu().is_none());
        assert_eq!(app.model.to_string(), format!("test/model-{}", first + 2));

        let lines: Vec<_> = (0..60).map(|line| format!("line {line}")).collect();
        app.info("Page", lines.join("\n"));
        key(&mut app, KeyCode::PageDown, M::NONE);
        let shown = rows(&mut app);
        assert_eq!(shown[0].0, MENU_PAGE);
        drag(&mut app, &shown);
        assert_eq!(top(&mut app), MENU_PAGE);
        assert!(app.menu().is_some_and(Menu::page));
        assert!(app.selection.is_none() && app.clipboard.is_none());
        key(&mut app, KeyCode::Down, M::NONE);
        assert_eq!(top(&mut app), MENU_PAGE + 1);
        // At the end of the page, Up scrolls back at once.
        key(&mut app, KeyCode::End, M::NONE);
        let last = top(&mut app);
        assert_eq!(last + shown.len(), lines.len());
        key(&mut app, KeyCode::Up, M::NONE);
        assert_eq!(top(&mut app), last - 1);
    }

    /// A shorter viewport scrolls to keep the selection in view, so Enter never
    /// chooses a row that is not shown.
    #[tokio::test]
    async fn a_shrinking_palette_keeps_its_selection_in_view() {
        let (_root, mut app) = fixture().await;
        let models = (0..30).map(|index| {
            let name = format!("test/model-{index}");
            Item::new(name.parse().unwrap(), name, "")
        });
        app.open("Models", MenuKind::Models(models.collect()));
        key(&mut app, KeyCode::End, M::NONE);
        let shown = |app: &mut App, height| {
            draw_sized_buffer(app, 80, height);
            let hits = app.hits.iter();
            let rows = hits.filter_map(|(_, hit)| match hit {
                Hit::Menu(index) => Some(*index),
                _ => None,
            });
            rows.collect::<Vec<_>>()
        };
        let tall = shown(&mut app, 32);
        let short = shown(&mut app, 22);
        assert!(short.len() < tall.len());
        assert_eq!(app.menu().unwrap().selected(), 29);
        assert_eq!(short.last(), Some(&29));
        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(app.model.to_string(), "test/model-29");
    }
}
