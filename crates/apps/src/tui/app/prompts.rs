use super::{App, Pane};
use crate::tui::editor::{Editor, TextField};
use crate::tui::viewport::Viewport;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers as M};
use skyhook::agent::{Question, QuestionAnswer, QuestionReply};
use skyhook::interaction::{ApprovalReply, Dismissed, Prompt, PromptKind};
use std::collections::VecDeque;
use std::ops::Deref;

/// Editor and view travel with their prompt (and with each question page).
#[derive(Default)]
pub struct PromptInput {
    pub editor: Editor,
    pub choice: usize,
    pub body: Viewport,
    pub options: Viewport,
    /// Set by manual option scrolling; otherwise rendering reveals the choice.
    pub options_scrolled: bool,
}
impl PromptInput {
    fn reset_view(&mut self) {
        self.body.top = 0;
        self.options.top = 0;
        self.options_scrolled = false;
    }
}

/// How a confirmed answer reads on the review page.
fn summary(answer: &QuestionAnswer) -> String {
    match answer {
        QuestionAnswer::Text(text) => text.clone(),
        QuestionAnswer::Commented { answer, comment } => format!("{answer} — {comment}"),
        QuestionAnswer::Selected(answers) => answers.join(", "),
        QuestionAnswer::SelectedCommented { answers, comment } => {
            format!("{} — {comment}", answers.join(", "))
        }
    }
}

struct QuestionDraft {
    input: PromptInput,
    /// Which options a question taking several has ticked, by index.
    ticked: Vec<bool>,
    answer: Option<QuestionAnswer>,
}

impl QuestionDraft {
    /// A page with nothing chosen yet: a recommendation is only tagged.
    fn new(question: &Question) -> Self {
        Self {
            input: PromptInput::default(),
            ticked: vec![false; question.options.len()],
            answer: None,
        }
    }

    /// The answer the page gives `question` as it stands: the focused suggestion or the
    /// written text, or for a question taking several the ticked suggestions and any
    /// written text. Text typed while a suggestion is focused comments on it.
    fn answer(&self, question: &Question) -> Option<QuestionAnswer> {
        let text = self.input.editor.text();
        let written = !text.trim().is_empty();
        let focused = question.options.get(self.input.choice);
        if !question.multiple {
            return Some(match focused {
                Some(option) if !written => QuestionAnswer::Text(option.label.to_string()),
                Some(option) => QuestionAnswer::Commented {
                    answer: option.label.to_string(),
                    comment: text.to_owned(),
                },
                None if !written => return None,
                None => QuestionAnswer::Text(text.to_owned()),
            });
        }
        let ticked = (question.options.iter().zip(&self.ticked)).filter(|(_, ticked)| **ticked);
        let mut answers: Vec<_> = ticked.map(|(option, _)| option.label.to_string()).collect();
        if focused.is_none() && written {
            answers.push(text.to_owned());
        }
        if answers.is_empty() {
            return None;
        }
        Some(match focused {
            Some(_) if written => QuestionAnswer::SelectedCommented {
                answers,
                comment: text.to_owned(),
            },
            _ => QuestionAnswer::Selected(answers),
        })
    }
}

/// A question batch's pages: its questions, then review.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BatchPage {
    Question(usize),
    Review,
}

struct Batch {
    page: BatchPage,
    drafts: Vec<QuestionDraft>,
    review: PromptInput,
    editing: bool,
}
impl Batch {
    fn new(questions: &[Question]) -> Self {
        let drafts: Vec<_> = questions.iter().map(QuestionDraft::new).collect();
        let page = if drafts.is_empty() {
            BatchPage::Review
        } else {
            BatchPage::Question(0)
        };
        Self {
            page,
            drafts,
            review: PromptInput::default(),
            editing: false,
        }
    }
    /// Pages before the review page.
    fn answerable(&self) -> usize {
        self.drafts.len()
    }
    fn page_at(&self, index: usize) -> BatchPage {
        if index < self.drafts.len() {
            BatchPage::Question(index)
        } else {
            BatchPage::Review
        }
    }
    fn index(&self) -> usize {
        match self.page {
            BatchPage::Question(index) => index,
            BatchPage::Review => self.answerable(),
        }
    }
    fn input(&self) -> &PromptInput {
        match self.page {
            BatchPage::Question(index) => &self.drafts[index].input,
            BatchPage::Review => &self.review,
        }
    }
    fn input_mut(&mut self) -> &mut PromptInput {
        match self.page {
            BatchPage::Question(index) => &mut self.drafts[index].input,
            BatchPage::Review => &mut self.review,
        }
    }
    fn select(&mut self, index: usize) {
        if index > self.answerable() || index == self.index() {
            return;
        }
        self.page = self.page_at(index);
        if self.page == BatchPage::Review {
            self.review.choice = 0;
        }
        self.editing = false;
        self.input_mut().reset_view();
    }
    fn missing(&self) -> Option<usize> {
        self.drafts.iter().position(|draft| draft.answer.is_none())
    }
    fn reply(&mut self, questions: Vec<Question>) -> QuestionReply {
        (questions.into_iter().zip(&mut self.drafts))
            .map(|(question, draft)| {
                (
                    question.header,
                    draft.answer.take().expect("confirmed answer"),
                )
            })
            .collect()
    }
}

enum PromptState {
    Approval(PromptInput),
    Questions(Box<Batch>),
    Rename(PromptInput),
}
pub struct UiPrompt {
    request: Prompt,
    state: PromptState,
    /// In view in place of the composer; dismissing it keeps it pending.
    shown: bool,
    /// The focus of an ordinary prompt suspended by a preempting one.
    resume: Option<Pane>,
}
// Read-only projection preserves request consumers without exposing a way to
// replace its category/fields independently of the associated draft.
impl Deref for UiPrompt {
    type Target = Prompt;
    fn deref(&self) -> &Prompt {
        &self.request
    }
}
impl UiPrompt {
    fn new(request: Prompt, shown: bool) -> Self {
        let state = match &request.kind {
            PromptKind::Approval { .. } => PromptState::Approval(PromptInput::default()),
            PromptKind::Questions { questions, .. } => {
                PromptState::Questions(Box::new(Batch::new(questions)))
            }
            PromptKind::Rename { current, .. } => {
                let mut input = PromptInput::default();
                input.editor.set(current.clone());
                PromptState::Rename(input)
            }
        };
        Self {
            request,
            state,
            shown,
            resume: None,
        }
    }
    fn input(&self) -> &PromptInput {
        match &self.state {
            PromptState::Approval(input) | PromptState::Rename(input) => input,
            PromptState::Questions(batch) => batch.input(),
        }
    }
    fn input_mut(&mut self) -> &mut PromptInput {
        match &mut self.state {
            PromptState::Approval(input) | PromptState::Rename(input) => input,
            PromptState::Questions(batch) => batch.input_mut(),
        }
    }
    fn batch(&self) -> Option<(&[Question], &Batch)> {
        match (&self.request.kind, &self.state) {
            (PromptKind::Questions { questions, .. }, PromptState::Questions(state)) => {
                Some((questions, state))
            }
            _ => None,
        }
    }
    fn batch_mut(&mut self) -> Option<&mut Batch> {
        match &mut self.state {
            PromptState::Questions(batch) => Some(batch),
            _ => None,
        }
    }
    /// The page shown when this question prompt is in view.
    fn page(&self) -> Option<QuestionPage<'_>> {
        let (questions, state) = self.batch()?;
        Some(match state.page {
            BatchPage::Question(index) => {
                let draft = &state.drafts[index];
                QuestionPage::Question(&questions[index], &draft.input, &draft.ticked)
            }
            BatchPage::Review => QuestionPage::Review(review(questions, state)),
        })
    }
    /// Whether typed text is part of the answer on the current page: a choice's
    /// comment or written answer, or a title. Approvals and the review page are
    /// answered by choice alone.
    pub fn takes_text(&self) -> bool {
        match &self.request.kind {
            PromptKind::Rename { .. } => true,
            PromptKind::Approval { .. } => false,
            PromptKind::Questions { .. } => matches!(self.page(), Some(QuestionPage::Question(..))),
        }
    }
    pub(super) fn reject(self, why: Dismissed) {
        self.request.reject(why);
    }
    /// Clear the typed text as one undoable edit.
    pub(super) fn clear_input(&mut self) {
        let editor = &mut self.input_mut().editor;
        editor.erase(0..editor.text().len());
    }
}

/// A question page as the panel lays it out.
pub enum QuestionPage<'a> {
    Question(&'a Question, &'a PromptInput, &'a [bool]),
    /// The summary of every question's answer so far, as Markdown.
    Review(String),
}
impl QuestionPage<'_> {
    /// The page's own Markdown, laid out after the prompt's header.
    pub fn prompt(&self) -> &str {
        match self {
            Self::Question(question, ..) => question.prompt.as_str(),
            Self::Review(summary) => summary,
        }
    }
    /// Labelled suggestions with their descriptions, then the written answer. A question
    /// taking several shows each suggestion's box; a recommended one says so.
    pub fn choices(&self) -> Vec<(String, String)> {
        let Self::Question(question, _, ticked) = self else {
            let review = REVIEW_ITEMS
                .iter()
                .map(|(_, label)| ((*label).to_owned(), String::new()));
            return review.collect();
        };
        (question.options.iter().zip(ticked.iter()).enumerate())
            .map(|(index, (option, ticked))| {
                let ticked = match (question.multiple, ticked) {
                    (false, _) => "",
                    (true, true) => "[x] ",
                    (true, false) => "[ ] ",
                };
                let recommended = if option.recommended {
                    " (recommended)"
                } else {
                    ""
                };
                let label = format!("{ticked}{}: {}{recommended}", index + 1, option.label);
                (label, option.description.to_string())
            })
            .chain([(WRITE_ANSWER.into(), String::new())])
            .collect()
    }
    /// Each suggestion's preview, and the focused one's index among them, when the
    /// question has any.
    pub fn previews(&self) -> Option<(Vec<Option<&str>>, usize)> {
        let Self::Question(question, input, _) = self else {
            return None;
        };
        let previews: Vec<_> = (question.options.iter())
            .map(|option| option.preview.as_ref().map(|preview| preview.as_str()))
            .collect();
        previews
            .iter()
            .any(Option::is_some)
            .then_some((previews, input.choice))
    }
    /// Whether the focused choice is a suggestion a question taking several ticks.
    pub fn tickable(&self) -> bool {
        matches!(self, Self::Question(question, input, _)
            if question.multiple && input.choice < question.options.len())
    }
    /// What the editor under the choices collects.
    pub fn input_label(&self) -> &'static str {
        match self {
            Self::Question(question, input, _) if input.choice < question.options.len() => {
                "Comment (optional):"
            }
            Self::Question(question, ..) if question.multiple => "Another answer (optional):",
            Self::Question(..) => "Answer:",
            Self::Review(_) => "",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ApprovalAction {
    Allow,
    Deny,
    Details,
    Grant,
}
fn approval_items(
    request: &skyhook::tool::policy::AuthorizationRequest,
) -> Vec<(ApprovalAction, &'static str)> {
    let mut items = vec![
        (ApprovalAction::Allow, "Allow once"),
        (ApprovalAction::Deny, "Deny"),
        (ApprovalAction::Details, "Details"),
    ];
    if request.permissions.iter().any(|p| p.proposed.is_some()) {
        items.push((ApprovalAction::Grant, "Allow proposed scope"));
    }
    items
}
/// The approval document's tool arguments in brief, then each of its other parts
/// whole on a line of its own: however long the arguments, every hop of a route
/// the call connects along shows.
fn approval_preview(document: &serde_json::Value) -> String {
    use crate::{text::brief, tui::format::pretty};
    let mut parts = document.as_object().cloned().unwrap_or_default();
    let arguments = parts
        .shift_remove("tool")
        .map(|arguments| brief(&pretty(&arguments), 240));
    let context = parts
        .iter()
        .map(|(part, value)| format!("{part}: {}", brief(&pretty(value), usize::MAX)));
    let text: Vec<_> = arguments.into_iter().chain(context).collect();
    text.join("\n")
}
#[derive(Clone, Copy)]
enum ReviewAction {
    Submit,
    Review,
}
/// The last choice on a question page: answer in writing instead of a suggestion.
const WRITE_ANSWER: &str = "Write an answer…";
const REVIEW_ITEMS: [(ReviewAction, &str); 2] = [
    (ReviewAction::Submit, "Submit answers"),
    (ReviewAction::Review, "Review again"),
];

impl App {
    /// A key for the prompt shown.
    pub(super) fn prompt_key(&mut self, key: KeyEvent) {
        let options = self.prompt_options().len();
        let takes_text = self.prompt_takes_text();
        let multiple = self.multiple_pages();
        let editing_page = multiple && self.page_index() < self.question_count();
        let old_choice = self.prompt_input().choice;
        let mut text_changed = false;
        let old_index = self.page_index();
        // Space ticks the focused suggestion of a question taking several until text
        // is typed, after which it is text.
        let ticks = key.code == KeyCode::Char(' ')
            && self.prompt_input().editor.text().is_empty()
            && self.question_page().is_some_and(|page| page.tickable());
        match key.code {
            _ if ticks => {
                text_changed = self.toggle_choice();
            }
            KeyCode::Esc => {
                self.cancel_prompt();
                return;
            }
            // As in the composer: clear typed text, and back out of an empty field.
            KeyCode::Char('c') if key.modifiers.contains(M::CONTROL) => {
                if takes_text && !self.prompt_input().editor.text().is_empty() {
                    self.prompts
                        .front_mut()
                        .expect("active prompt")
                        .clear_input();
                    text_changed = true;
                } else {
                    self.cancel_prompt();
                    return;
                }
            }
            KeyCode::PageUp | KeyCode::PageDown => {
                let (view, len) = self.prompt_view(key.modifiers.contains(M::CONTROL));
                view.page(key.code == KeyCode::PageDown, len);
            }
            KeyCode::Left | KeyCode::Right if multiple && !self.page_editing() => {
                self.switch_page(if key.code == KeyCode::Left { -1 } else { 1 });
            }
            KeyCode::Tab | KeyCode::BackTab if editing_page => {
                self.set_page_editing(!self.page_editing());
            }
            KeyCode::Up | KeyCode::BackTab | KeyCode::Down | KeyCode::Tab => {
                self.set_page_editing(false);
                if options > 0 {
                    let back = matches!(key.code, KeyCode::Up | KeyCode::BackTab);
                    let input = self.prompt_input_mut();
                    let step = if back { options - 1 } else { 1 };
                    input.choice = (input.choice + step) % options;
                    input.options_scrolled = false;
                }
            }
            KeyCode::Enter => self.answer(),
            // A choice-only prompt has no text to edit.
            _ if !takes_text => {}
            _ => {
                if editing_page
                    && matches!(
                        key.code,
                        KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete
                    )
                {
                    self.set_page_editing(true);
                }
                text_changed = self.prompt_input_mut().editor.handle(key).text_changed();
            }
        }
        if editing_page
            && key.code != KeyCode::Enter
            && self.page_index() == old_index
            && (self.prompt_input().choice != old_choice || text_changed)
        {
            self.invalidate_page();
        }
    }
}

impl App {
    pub fn prompt_input(&self) -> &PromptInput {
        self.prompts.front().expect("active prompt").input()
    }
    pub fn prompt_input_mut(&mut self) -> &mut PromptInput {
        self.prompts.front_mut().expect("active prompt").input_mut()
    }
    /// The question page in view, if the front prompt asks questions.
    pub fn question_page(&self) -> Option<QuestionPage<'_>> {
        self.prompts.front().and_then(UiPrompt::page)
    }
    pub fn page_index(&self) -> usize {
        self.prompts
            .front()
            .and_then(UiPrompt::batch)
            .map_or(0, |(_, state)| state.index())
    }
    /// Pages the user answers, before review; more than one lets ←→ switch.
    pub fn question_count(&self) -> usize {
        self.prompts
            .front()
            .and_then(UiPrompt::batch)
            .map_or(0, |(_, state)| state.answerable())
    }
    pub fn page_editing(&self) -> bool {
        self.prompts
            .front()
            .and_then(UiPrompt::batch)
            .is_some_and(|(_, state)| state.editing)
    }
    pub(super) fn set_page_editing(&mut self, editing: bool) {
        if let Some(batch) = self.prompts.front_mut().and_then(UiPrompt::batch_mut) {
            batch.editing = editing;
        }
    }
    pub(super) fn reveal_prompt(&mut self) {
        if let Some(prompt) = self.prompts.front_mut() {
            prompt.input_mut().options_scrolled = false;
        }
    }
    pub fn prompt_shown(&self) -> bool {
        self.prompts.front().is_some_and(|prompt| prompt.shown)
    }
    pub fn prompt_takes_text(&self) -> bool {
        self.prompts.front().is_some_and(UiPrompt::takes_text)
    }
    pub fn prompt(&mut self, prompt: Prompt) {
        if prompt.preempts() {
            // Ordinary state stays attached to its own queued request, including every page.
            if let Some(previous) = self.prompts.front_mut() {
                previous.resume = Some(self.focus);
            }
            self.prompts.push_front(UiPrompt::new(prompt, false));
            self.activate_prompt();
        } else {
            let shown = self.prompts.is_empty();
            self.prompts.push_back(UiPrompt::new(prompt, shown));
            if shown {
                self.leader = None;
            }
        }
        self.refresh_menu();
        self.dirty = true;
    }
    /// Take out the prompts `remove` picks. A new front prompt restores the
    /// focus it was suspended with, or else stays in view if the old one was.
    pub(super) fn take_prompts(
        &mut self,
        remove: impl Fn(&UiPrompt) -> bool,
    ) -> VecDeque<UiPrompt> {
        let front = self.prompts.front().map(|prompt| (prompt.id, prompt.shown));
        let prompts = std::mem::take(&mut self.prompts).into_iter();
        let (removed, kept): (VecDeque<_>, _) = prompts.partition(|prompt| remove(prompt));
        self.prompts = kept;
        if !removed.is_empty() {
            self.refresh_menu();
        }
        let Some((id, shown)) = front else {
            return removed;
        };
        match self.prompts.front_mut() {
            Some(prompt) if prompt.id == id => return removed,
            Some(prompt) => match prompt.resume.take() {
                Some(focus) => self.focus = focus,
                None => prompt.shown = shown,
            },
            None => {}
        }
        self.dirty = true;
        removed
    }
    pub(super) fn cancel_prompt(&mut self) {
        let error = match self.prompts.front().map(|prompt| &prompt.kind) {
            Some(PromptKind::Rename { .. }) => Dismissed::Rename,
            Some(PromptKind::Questions {
                background: true, ..
            }) => Dismissed::Questions,
            // Dismissal is not cancellation: request and owned draft persist.
            Some(_) => {
                self.prompts[0].shown = false;
                return;
            }
            None => return,
        };
        let front = self.prompts[0].id;
        for prompt in self.take_prompts(|prompt| prompt.id == front) {
            prompt.reject(error);
        }
    }
    pub(super) fn reject_pending_questions(&mut self) {
        let questions = |prompt: &UiPrompt| matches!(prompt.kind, PromptKind::Questions { .. });
        for prompt in self.take_prompts(questions) {
            prompt.reject(Dismissed::Superseded);
        }
    }
    pub(super) fn activate_prompt(&mut self) {
        let Some(prompt) = self.prompts.front_mut() else {
            return;
        };
        prompt.shown = true;
        self.focus = Pane::Composer;
        self.leader = None;
        self.menu = None;
    }
    /// The view of the prompt's choices or else its text, to scroll by hand,
    /// and how many rows that part lays out.
    pub(super) fn prompt_view(&mut self, options: bool) -> (&mut Viewport, usize) {
        let len = if options {
            self.render.prompt_option_rows
        } else {
            self.render.prompt_body_rows
        };
        let input = self.prompt_input_mut();
        input.options_scrolled |= options;
        let view = if options {
            &mut input.options
        } else {
            &mut input.body
        };
        (view, len)
    }
    pub fn prompt_options(&self) -> Vec<&str> {
        let Some(prompt) = self.prompts.front() else {
            return vec![];
        };
        fn labels<T>(items: Vec<(T, &str)>) -> Vec<&str> {
            items.into_iter().map(|(_, label)| label).collect()
        }
        match &prompt.kind {
            PromptKind::Approval { request, .. } => labels(approval_items(request)),
            PromptKind::Questions { .. } => match prompt.page() {
                Some(QuestionPage::Question(question, ..)) => (question.options.iter())
                    .map(|option| option.label.as_str())
                    .chain([WRITE_ANSWER])
                    .collect(),
                Some(QuestionPage::Review(_)) => labels(REVIEW_ITEMS.to_vec()),
                None => unreachable!("a question prompt has a page"),
            },
            PromptKind::Rename { .. } => vec![],
        }
    }
    /// The prompt's plain text; a question page's own prompt is Markdown the panel
    /// lays out after it.
    pub fn prompt_text(&self) -> String {
        let Some(prompt) = self.prompts.front() else {
            return String::new();
        };
        match &prompt.kind {
            PromptKind::Approval { request: r, .. } => format!(
                "Permission · agent {} · {}\n{}",
                r.agent.label(),
                r.tool,
                approval_preview(&r.arguments)
            ),
            PromptKind::Questions { agent, .. } => question_header(agent),
            PromptKind::Rename { .. } => {
                "Rename session\nLeave empty to use the automatic title".into()
            }
        }
    }
    /// What heads the front question batch's pages.
    pub fn question_header(&self) -> Option<String> {
        match &self.prompts.front()?.kind {
            PromptKind::Questions { agent, .. } => Some(question_header(agent)),
            _ => None,
        }
    }
    /// The front question batch's question pages, as each would be shown.
    pub fn question_pages(&self) -> Vec<QuestionPage<'_>> {
        let Some((questions, batch)) = self.prompts.front().and_then(UiPrompt::batch) else {
            return Vec::new();
        };
        (questions.iter().zip(&batch.drafts))
            .map(|(question, draft)| QuestionPage::Question(question, &draft.input, &draft.ticked))
            .collect()
    }
    /// The front question batch's tabs: each question's header, then review when there
    /// are several, each with where it stands.
    pub fn question_tabs(&self) -> Vec<(String, TabState)> {
        let Some((questions, batch)) = self.prompts.front().and_then(UiPrompt::batch) else {
            return Vec::new();
        };
        let state = |page, answered| match (batch.page == page, answered) {
            (true, _) => TabState::Current,
            (false, true) => TabState::Answered,
            (false, false) => TabState::Open,
        };
        let pages = (questions.iter().zip(&batch.drafts)).enumerate();
        let mut tabs: Vec<_> = pages
            .map(|(index, (question, draft))| {
                let state = state(BatchPage::Question(index), draft.answer.is_some());
                (question.header.to_string(), state)
            })
            .collect();
        if questions.len() > 1 {
            tabs.push(("Review".to_owned(), state(BatchPage::Review, false)));
        }
        tabs
    }
    /// Tick or untick the focused suggestion of a question taking several; false when
    /// the focus is on no such suggestion.
    pub(super) fn toggle_choice(&mut self) -> bool {
        if !self.question_page().is_some_and(|page| page.tickable()) {
            return false;
        }
        let Some(batch) = self.prompts.front_mut().and_then(UiPrompt::batch_mut) else {
            return false;
        };
        let BatchPage::Question(index) = batch.page else {
            return false;
        };
        let draft = &mut batch.drafts[index];
        let Some(ticked) = draft.ticked.get_mut(draft.input.choice) else {
            return false;
        };
        *ticked = !*ticked;
        true
    }
    /// Whether the front prompt asks questions on pages to switch between.
    pub fn multiple_pages(&self) -> bool {
        self.question_count() > 1
    }
    pub(crate) fn select_page(&mut self, index: usize) {
        if let Some(batch) = self.prompts.front_mut().and_then(UiPrompt::batch_mut) {
            batch.select(index);
        }
    }
    pub(super) fn switch_page(&mut self, delta: isize) {
        let Some((_, batch)) = self.prompts.front().and_then(UiPrompt::batch) else {
            return;
        };
        // Review follows the last question, open however many are answered.
        let count = batch.answerable();
        if count > 1 {
            let index = batch.index();
            self.select_page(index.saturating_add_signed(delta).min(count));
        }
    }
    /// An edited page must be confirmed again before submission.
    pub(super) fn invalidate_page(&mut self) {
        if let Some(batch) = self.prompts.front_mut().and_then(UiPrompt::batch_mut)
            && let BatchPage::Question(index) = batch.page
        {
            batch.drafts[index].answer = None;
        }
    }
    pub(super) fn answer(&mut self) {
        let Some(prompt) = self.prompts.front_mut() else {
            return;
        };
        // Readiness changes only this prompt. A ready prompt is then removed,
        // and its category-specific reply is consumed exactly once below.
        match (&prompt.request.kind, &mut prompt.state) {
            (PromptKind::Approval { request, .. }, PromptState::Approval(input)) => {
                let Some(&(action, _)) = approval_items(request).get(input.choice) else {
                    return;
                };
                if action == ApprovalAction::Details {
                    let text = format!(
                        "Agent {}\nTool {}\nArguments\n{}\nPermissions\n{:#?}",
                        request.agent,
                        request.tool,
                        crate::tui::format::pretty(&request.arguments),
                        request.permissions
                    );
                    self.info("Permission details", text);
                    return;
                }
            }
            (PromptKind::Questions { questions, .. }, PromptState::Questions(state)) => {
                match state.page {
                    BatchPage::Question(index) => {
                        let draft = &mut state.drafts[index];
                        let Some(answer) = draft.answer(&questions[index]) else {
                            return;
                        };
                        draft.answer = Some(answer);
                    }
                    BatchPage::Review => match REVIEW_ITEMS.get(state.review.choice) {
                        Some((ReviewAction::Review, _)) => {
                            state.select(0);
                            return;
                        }
                        Some((ReviewAction::Submit, _)) => {
                            if let Some(index) = state.missing() {
                                state.select(index);
                                return;
                            }
                        }
                        None => return,
                    },
                }
                // A single page submits at once; several go through review.
                if state.page != BatchPage::Review && state.answerable() > 1 {
                    let next = if state.index() + 1 < state.answerable() {
                        state.index() + 1
                    } else {
                        state.missing().unwrap_or(state.answerable())
                    };
                    state.select(next);
                    return;
                }
            }
            (PromptKind::Rename { .. }, PromptState::Rename(_)) => {}
            _ => unreachable!("request and draft are constructed together and cannot be replaced"),
        }
        let front = self.prompts[0].id;
        let removed = self.take_prompts(|prompt| prompt.id == front);
        let UiPrompt {
            request, mut state, ..
        } = removed.into_iter().next().unwrap();
        match (request.kind, &mut state) {
            (PromptKind::Approval { request, reply }, PromptState::Approval(input)) => {
                let answer = match approval_items(&request)[input.choice].0 {
                    ApprovalAction::Allow => ApprovalReply::Allow,
                    ApprovalAction::Deny => ApprovalReply::Deny,
                    ApprovalAction::Grant => ApprovalReply::Grant,
                    ApprovalAction::Details => unreachable!("details does not finish a prompt"),
                };
                let _ = reply.send(Ok(answer));
            }
            (
                PromptKind::Questions {
                    questions, reply, ..
                },
                PromptState::Questions(state),
            ) => {
                let _ = reply.send(Ok(state.reply(questions)));
            }
            (PromptKind::Rename { reply, .. }, PromptState::Rename(input)) => {
                let _ = reply.send(Ok(input.editor.text().to_owned()));
            }
            _ => unreachable!("request and draft are constructed together"),
        }
    }
}

/// Every question's answer so far, unanswered ones marked so.
fn review(questions: &[Question], batch: &Batch) -> String {
    let line = |(question, draft): (&Question, &QuestionDraft)| {
        let answer = (draft.answer.as_ref()).map_or_else(|| "*unanswered*".to_owned(), summary);
        format!("- **{}**: {answer}", question.header)
    };
    let lines: Vec<_> = questions.iter().zip(&batch.drafts).map(line).collect();
    format!("Review answers\n\n{}", lines.join("\n"))
}

/// What heads a question batch from `agent`.
fn question_header(agent: &skyhook::identity::AgentId) -> String {
    format!("Agent {}", agent.label())
}

/// Where a question batch's tab stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabState {
    /// The page in view.
    Current,
    Answered,
    Open,
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    use crate::tui::app::input::InputTarget;
    use crate::tui::app::queue::Paused;
    use KeyCode::{Char, Down, Enter, Esc, Left, Right, Tab, Up};
    use crossterm::event::Event;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyModifiers as M;
    use skyhook::session::SessionEvent;

    /// A root agent's request to run `true` in the workspace.
    fn approval_request() -> skyhook::tool::policy::AuthorizationRequest {
        use skyhook::tool::policy::{Capability, PermissionUse, ResourceId};
        let session = skyhook::identity::SessionId::from_bytes([1; 16]);
        let item = ResourceId::workspace(&"root".parse().unwrap(), &"item".into());
        skyhook::tool::policy::AuthorizationRequest {
            agent: skyhook::identity::AgentId::root(session),
            job: skyhook_worker::job::JobId::new(1).unwrap(),
            parent: None,
            tool: "exec".into(),
            permissions: vec![PermissionUse::exact(Capability::Exec, item)],
            arguments: json!({"tool": {"command": ["true"]}}),
        }
    }

    fn confirmed_answers(app: &App) -> QuestionReply {
        let Some((questions, state)) = app.prompts.front().and_then(UiPrompt::batch) else {
            return QuestionReply::new();
        };
        (questions.iter().zip(&state.drafts))
            .filter_map(|(question, draft)| Some((question.header.clone(), draft.answer.clone()?)))
            .collect()
    }
    fn has_suspended_prompt(app: &App) -> bool {
        app.prompts.iter().any(|prompt| prompt.resume.is_some())
    }
    fn suggestions() -> Vec<QuestionOption> {
        ["First", "Second"]
            .map(|label| option(label, &format!("Use {label}")))
            .into()
    }
    fn paste(app: &mut App, text: &str) {
        app.event(Event::Paste(text.into()));
    }
    /// A rename prompt, which preempts ordinary prompts.
    fn rename(app: &mut App, id: u64) -> oneshot::Receiver<Result<String, Dismissed>> {
        let (reply, response) = oneshot::channel();
        let kind = PromptKind::Rename {
            current: String::new(),
            reply,
        };
        app.prompt(Prompt { id, kind });
        response
    }

    #[tokio::test]
    async fn questions_open_and_accept_answers_while_inspecting_agents() {
        let (_root, mut app) = fixture().await;
        let child = app.root_agent().child(1);
        app.select(child.clone());
        app.editor.set("preserved draft".into());

        for focus in [Pane::Tree, Pane::Content] {
            app.focus = focus;
            let option = option("Continue", "Keep working");
            let answer = question(&mut app, "Choose a direction".into(), vec![option]);
            assert!(app.prompt_shown());
            let screen = draw(&mut app);
            assert!(app.render.tree_rect.height > 0);
            assert!(screen.contains("Choose a direction") && screen.contains("Continue"));
            key(&mut app, Enter, M::NONE);
            let reply = answer.await.unwrap().unwrap();
            assert_eq!(reply, texts(&[("answer", "Continue")]));
            assert_eq!(app.selected, child);
            assert!(app.focus == focus);
            assert_eq!(app.editor.text(), "preserved draft");
        }
    }

    /// Space ticks the focused suggestion of a question taking several until text is
    /// typed. Text written on the last choice is another answer; text written on a
    /// suggestion comments on the ticked ones.
    #[tokio::test]
    async fn space_ticks_several() {
        let (_root, mut app) = fixture().await;
        let recommended = |label, recommended| QuestionOption {
            recommended,
            ..option(label, "Why")
        };
        let options = || {
            vec![
                recommended("a", false),
                recommended("b", true),
                recommended("c", true),
            ]
        };
        let several = || Question {
            multiple: true,
            ..titled("several", "Pick several", options())
        };
        let selected = |answers: &[&str]| answers.iter().map(|answer| answer.to_string()).collect();
        let reply = |answer| QuestionReply::from([("several".parse().unwrap(), answer)]);
        // Nothing starts ticked; Space ticks `a` and `b`; written text is another answer.
        let answer = asking(&mut app, false, vec![several()]);
        let screen = draw(&mut app);
        assert!(screen.contains("[ ] 2: b (recommended)"), "{screen}");
        assert!(screen.contains("Space tick"), "{screen}");
        // Nothing ticked or written answers nothing.
        key(&mut app, Enter, M::NONE);
        assert!(app.prompt_shown());
        press(&mut app, &[Char(' '), Down, Char(' '), Down, Down]);
        paste(&mut app, "d e");
        key(&mut app, Enter, M::NONE);
        let expected = QuestionAnswer::Selected(selected(&["a", "b", "d e"]));
        assert_eq!(answer.await.unwrap().unwrap(), reply(expected));
        // Typed on a suggestion, text is a comment, and Space is part of it.
        let answer = asking(&mut app, false, vec![several()]);
        press(&mut app, &[Char(' '), Char('n'), Char(' '), Char('b')]);
        key(&mut app, Enter, M::NONE);
        let expected = QuestionAnswer::SelectedCommented {
            answers: selected(&["a"]),
            comment: "n b".into(),
        };
        assert_eq!(answer.await.unwrap().unwrap(), reply(expected));
    }

    /// A batch's header names each question as a tab: the one in view, those answered,
    /// those still open, then review.
    #[tokio::test]
    async fn a_batch_shows_its_questions_as_tabs() {
        let (_root, mut app) = fixture().await;
        let batch = vec![
            ("auth", "How?", suggestions()),
            ("deploy", "Where?", suggestions()),
        ];
        let _answer = questions(&mut app, false, batch);
        let tabs = |states: [TabState; 3]| {
            let headers = ["auth", "deploy", "Review"].map(str::to_owned);
            headers.into_iter().zip(states).collect::<Vec<_>>()
        };
        use TabState::{Answered, Current, Open};
        assert_eq!(app.question_tabs(), tabs([Current, Open, Open]));
        key(&mut app, Enter, M::NONE);
        assert_eq!(app.question_tabs(), tabs([Answered, Current, Open]));
    }

    #[tokio::test]
    async fn question_navigation_preserves_unanswered_drafts_and_clamps() {
        let (_root, mut app) = fixture().await;
        let batch = vec![
            ("answer", "Choose", suggestions()),
            ("middle", "middle", vec![]),
            ("last", "last", vec![]),
        ];
        let mut response = questions(&mut app, false, batch);

        key(&mut app, Left, M::NONE);
        assert_eq!(app.page_index(), 0);
        key(&mut app, Up, M::NONE);
        assert_eq!(app.prompt_input_mut().choice, 2);
        key(&mut app, Down, M::NONE);
        assert_eq!(app.prompt_input_mut().choice, 0);
        key(&mut app, Down, M::NONE);
        paste(&mut app, "comment");
        key(&mut app, Left, M::NONE);
        assert_eq!(app.page_index(), 0);
        assert_eq!(app.prompt_input_mut().editor.cursor(), 6);
        assert!(draw(&mut app).contains("comment"));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 24)).unwrap();
        terminal
            .draw(|frame| crate::tui::render::draw(frame, &mut app))
            .unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(cursor.x, cursor.y)].symbol(), "t");
        key(&mut app, Tab, M::NONE);
        app.prompt_input_mut().body.top = 5;
        app.prompt_input_mut().options.top = 5;
        key(&mut app, Right, M::NONE);
        let input = app.prompt_input_mut();
        assert_eq!((input.body.top, input.options.top), (0, 0));
        paste(&mut app, "draft");
        // Review follows the last question, open with answers missing, and clamps.
        press(&mut app, &[Tab, Right, Right, Right]);
        assert_eq!(app.page_index(), 3);
        let review = app.question_page().unwrap().prompt().to_owned();
        assert!(review.contains("**middle**: *unanswered*"), "{review}");
        key(&mut app, Left, M::NONE);
        assert_eq!(app.page_index(), 2);
        assert!(app.prompt_input_mut().editor.text().is_empty());
        assert!(confirmed_answers(&app).is_empty());
        assert!(pending(&mut response));
        key(&mut app, Esc, M::NONE);
        assert!(!app.prompt_shown());
        assert_eq!(app.prompts.len(), 1);
        assert!(pending(&mut response));
        chord(&mut app, KeyCode::Char('r'));
        assert!(app.prompt_shown());
        assert_eq!(app.page_index(), 2);
        key(&mut app, Left, M::NONE);
        assert_eq!(app.prompt_input_mut().editor.text(), "draft");
        key(&mut app, Left, M::NONE);
        let input = app.prompt_input_mut();
        assert_eq!((input.choice, input.editor.text()), (1, "comment"));
        assert_eq!(input.editor.cursor(), 6);
        assert!(confirmed_answers(&app).is_empty());
    }

    #[tokio::test]
    async fn question_navigation_enter_wraps_skips_before_review_and_submit() {
        let (_root, mut app) = fixture().await;
        let batch = vec![("answer", "First", vec![]), ("last", "Last", vec![])];
        let mut response = questions(&mut app, false, batch);

        app.select_page(2); // Review is a bounded page, not an invalid question index.
        app.prompt_input_mut().choice = 2;
        app.answer();
        assert_eq!(app.page_index(), 2);
        app.prompt_input_mut().choice = 0;
        app.answer(); // Submit returns to the first missing answer instead.
        assert_eq!(app.page_index(), 0);
        assert!(confirmed_answers(&app).is_empty());
        assert!(pending(&mut response));
        app.prompt_input_mut().editor.set(String::new());

        key(&mut app, Right, M::NONE);
        paste(&mut app, "second");
        key(&mut app, Enter, M::NONE);
        assert_eq!(app.page_index(), 0);
        assert_eq!(confirmed_answers(&app).len(), 1);
        key(&mut app, Enter, M::NONE); // Empty free-batch stays unanswered.
        assert_eq!(app.page_index(), 0);
        assert!(pending(&mut response));
        paste(&mut app, "first");
        key(&mut app, Enter, M::NONE);
        assert_eq!(app.prompt_input_mut().editor.text(), "second");
        key(&mut app, Enter, M::NONE);
        assert_eq!(app.page_index(), 2); // Review, not submission.
        assert!(pending(&mut response));
        key(&mut app, Right, M::NONE);
        assert_eq!(app.page_index(), 2);
        key(&mut app, Left, M::NONE); // Review can return to last question.
        assert_eq!(app.page_index(), 1);
        paste(&mut app, " revised");
        assert!(!confirmed_answers(&app).contains_key("last"));
        press(&mut app, &[Tab, Left, Enter]);
        assert_eq!(app.prompt_input_mut().editor.text(), "second revised");
        press(&mut app, &[Enter, Enter]);
        let reply = response.await.unwrap().unwrap();
        assert_eq!(
            reply,
            texts(&[("answer", "first"), ("last", "second revised")])
        );
    }

    #[tokio::test]
    async fn single_question_editing_is_plain_text() {
        let (_root, mut app) = fixture().await;
        let response = question(&mut app, "Single".into(), vec![]);
        paste(&mut app, "abc");
        key(&mut app, Left, M::NONE);
        assert_eq!(app.prompt_input_mut().editor.cursor(), 2);
        key(&mut app, Enter, M::NONE);
        assert_eq!(
            response.await.unwrap().unwrap(),
            texts(&[("answer", "abc")])
        );
        assert!(app.prompts.is_empty());
    }

    /// Ctrl+c clears a prompt's text as it clears the composer's, then backs out of
    /// the empty prompt; the composer's draft is never touched.
    #[tokio::test]
    async fn ctrl_c_clears_prompt_text_then_cancels() {
        let (_root, mut app) = fixture().await;
        paste(&mut app, "composer draft");
        let title = rename(&mut app, 100);
        paste(&mut app, "title");
        key(&mut app, Char('c'), M::CONTROL);
        assert!(app.prompt_input().editor.text().is_empty());
        assert_eq!(app.prompts.len(), 1);
        key(&mut app, Char('c'), M::CONTROL);
        assert!(title.await.unwrap().is_err());
        assert!(app.prompts.is_empty());
        assert_eq!(app.editor.text(), "composer draft");
    }

    /// A rename edits the title in its own input. It starts from the user's title,
    /// or empty from an automatic one. Esc keeps the title; Enter saves a user
    /// title, or clears it when empty.
    #[tokio::test]
    async fn rename_prompt_edits_the_title() {
        use skyhook::session::{SessionTitle, TitleSource};
        let (_root, mut app) = fixture().await;
        let title = |text: &str, source| {
            let text = text.into();
            Some(SessionTitle { text, source })
        };
        app.title = title("first prompt", TitleSource::Prompt);
        app.rename_session();
        assert!(app.prompt_shown());
        assert!(app.prompt_input().editor.text().is_empty());
        assert!(draw(&mut app).contains("Rename session"));
        paste(&mut app, "discarded");
        key(&mut app, Esc, M::NONE);
        assert!(app.prompts.is_empty());
        app.rename_session();
        app.prompt_input_mut().editor.set(" My \n title ".into());
        key(&mut app, Enter, M::NONE);
        // Were the cancelled rename saved, its title would be observed first.
        let renamed = SessionEvent::TitleSet {
            title: "My title".into(),
            source: TitleSource::User,
        };
        assert_eq!(next_title_change(&mut app).await, renamed);
        app.title = title("My title", TitleSource::User);
        app.rename_session();
        assert_eq!(app.prompt_input().editor.text(), "My title");
        key(&mut app, Char('c'), M::CONTROL);
        key(&mut app, Enter, M::NONE);
        assert_eq!(
            next_title_change(&mut app).await,
            SessionEvent::TitleCleared
        );
        assert!(app.prompts.is_empty());
    }

    #[tokio::test]
    async fn question_comments_survive_batch_review() {
        let (_root, mut app) = fixture().await;
        let batch = vec![
            ("answer", "Choose", suggestions()),
            ("next", "Anything else?", vec![]),
        ];
        let response = questions(&mut app, false, batch);

        key(&mut app, Down, M::NONE);
        paste(&mut app, "my comment");
        key(&mut app, Enter, M::NONE);
        assert!(app.prompt_input_mut().editor.text().is_empty());
        paste(&mut app, "freeform");
        key(&mut app, Enter, M::NONE);
        let review = app.question_page().unwrap().prompt().to_owned();
        assert!(review.contains("my comment"), "{review}");
        press(&mut app, &[Down, Enter]); // Review again.
        assert_eq!(app.page_index(), 0);
        let input = app.prompt_input_mut();
        assert_eq!((input.choice, input.editor.text()), (1, "my comment"));
        paste(&mut app, " amended");
        key(&mut app, Enter, M::NONE);
        assert_eq!(app.prompt_input_mut().editor.text(), "freeform");
        press(&mut app, &[Enter, Enter]);
        let mut expected = texts(&[("next", "freeform")]);
        let commented = QuestionAnswer::Commented {
            answer: "Second".into(),
            comment: "my comment amended".into(),
        };
        expected.insert("answer".parse().unwrap(), commented);
        assert_eq!(response.await.unwrap().unwrap(), expected);
        // A blank comment is dropped, free-batch text is untrimmed and
        // whitespace-only free-batch input never confirms.
        let response = question(&mut app, "Choose".into(), suggestions());
        app.prompt_input_mut().choice = 2;
        app.prompt_input_mut().editor.set(" free batch ".into());
        app.answer();
        let reply = response.await.unwrap().unwrap();
        assert_eq!(reply, texts(&[("answer", " free batch ")]));
        let mut response = question(&mut app, "Write".into(), vec![]);
        app.prompt_input_mut().editor.set(" ".into());
        app.answer();
        assert!(confirmed_answers(&app).is_empty());
        assert!(pending(&mut response));
    }

    #[tokio::test]
    async fn rename_preempts_overlays_and_restores_partial_question_batches() {
        let (_root, mut app) = fixture().await;
        for finish in 0..3 {
            app.editor.set("composer draft".into());
            app.focus = Pane::Tree;
            let batch = vec![
                ("answer", "First question", vec![]),
                ("second", "Second question", vec![]),
            ];
            let answer = questions(&mut app, false, batch);
            paste(&mut app, "first answer");
            key(&mut app, Enter, M::NONE);
            paste(&mut app, "unfinished answer");
            let input = app.prompt_input_mut();
            (input.body.top, input.options.top, input.options_scrolled) = (3, 2, true);
            app.info("Details", "An open menu".into());
            let response = rename(&mut app, 100);
            assert!(matches!(app.input_target(), InputTarget::Prompt));
            assert!(app.menu.is_none());
            assert_eq!(app.prompts.front().unwrap().id, 100);
            assert!(app.prompt_input_mut().editor.text().is_empty());
            assert!(confirmed_answers(&app).is_empty());
            paste(&mut app, "new title");
            assert!(draw(&mut app).contains("Rename session"));
            match finish {
                0 => {
                    key(&mut app, Enter, M::NONE);
                    assert_eq!(response.await.unwrap().unwrap(), "new title");
                }
                1 => {
                    key(&mut app, Esc, M::NONE);
                    assert!(response.await.unwrap().is_err());
                }
                _ => {
                    drop(response);
                    app.tick();
                }
            }
            assert!(app.prompt_shown() && app.focus == Pane::Tree);
            assert_eq!(app.page_index(), 1);
            let input = app.prompt_input_mut();
            assert_eq!(input.editor.text(), "unfinished answer");
            assert_eq!(
                (input.body.top, input.options.top, input.options_scrolled),
                (3, 2, true)
            );
            assert_eq!(
                confirmed_answers(&app),
                texts(&[("answer", "first answer")])
            );
            assert_eq!(app.editor.text(), "composer draft");
            assert!(!has_suspended_prompt(&app));
            press(&mut app, &[Enter, Enter]);
            let reply = answer.await.unwrap().unwrap();
            let expected = [("answer", "first answer"), ("second", "unfinished answer")];
            assert_eq!(reply, texts(&expected));
        }
    }

    #[tokio::test]
    async fn rename_takes_priority_over_dismissed_requests() {
        let (_root, mut app) = fixture().await;
        let answer = question(&mut app, "Question".into(), vec![]);
        paste(&mut app, "saved answer");
        key(&mut app, Esc, M::NONE);
        let title = rename(&mut app, 100);
        assert!(app.prompt_shown());
        assert_eq!(
            app.prompts.iter().map(|p| p.id).collect::<Vec<_>>(),
            [100, 1]
        );
        key(&mut app, Esc, M::NONE);
        assert!(title.await.unwrap().is_err());
        assert!(!app.prompt_shown());
        assert_eq!(app.prompt_input_mut().editor.text(), "saved answer");
        chord(&mut app, KeyCode::Char('r'));
        key(&mut app, Enter, M::NONE);
        let reply = answer.await.unwrap().unwrap();
        assert_eq!(reply, texts(&[("answer", "saved answer")]));
    }

    #[tokio::test]
    async fn cancelled_suspended_questions_do_not_leak_into_later_prompts() {
        let (_root, mut app) = fixture().await;
        let answer = question(&mut app, "Question".into(), vec![]);
        paste(&mut app, "abandoned answer");
        let response = rename(&mut app, 100);
        drop(answer);
        app.tick();
        key(&mut app, Esc, M::NONE);
        assert!(response.await.unwrap().is_err());
        assert!(!has_suspended_prompt(&app));
        assert!(app.prompts.is_empty());
        // Closing the only prompt redraws.
        drop(question(&mut app, "Question".into(), vec![]));
        app.dirty = false;
        app.tick();
        assert!(app.prompts.is_empty());
        assert!(app.dirty);
        let batch = vec![
            ("answer", "Background question", vec![]),
            ("second", "Second question", vec![]),
        ];
        let cancelled = questions(&mut app, true, batch);
        paste(&mut app, "partial answer");
        key(&mut app, Enter, M::NONE);
        assert_eq!(confirmed_answers(&app).len(), 1);
        key(&mut app, Esc, M::NONE);
        assert!(cancelled.await.unwrap().is_err());
        assert!(app.prompts.is_empty());
        assert!(confirmed_answers(&app).is_empty());
        chord(&mut app, KeyCode::Char('r'));
        assert!(!app.prompt_shown());
        let rejected = question(&mut app, "Next question".into(), vec![]);
        assert!(app.prompt_input_mut().editor.text().is_empty());
        key(&mut app, Esc, M::NONE);
        app.paused = Some(Paused::Stopped);
        app.submit("A new direction".into());
        assert!(rejected.await.unwrap().is_err());
        assert!(app.prompts.is_empty());
        assert_eq!(app.queue.len(), 1);
        chord(&mut app, KeyCode::Char('r'));
        assert!(!app.prompt_shown());
    }

    /// However long the tool's arguments, they are shown in brief and every hop of
    /// the route after them whole.
    #[test]
    fn approval_preview_leads_with_the_tool_arguments() {
        let mut document = approval_request().arguments;
        document["tool"]["command"] = json!(["x".repeat(500)]);
        let arguments = crate::tui::format::pretty(&document["tool"]);
        let hop = |name: &str| json!({"name": name, "host": format!("{name}.example.com")});
        document["route"] = json!({"route": [hop("edge"), hop("build")]});
        let preview = approval_preview(&document);
        let (shown, route) = preview.split_once('\n').unwrap();
        assert_eq!(shown, crate::text::brief(&arguments, 240));
        assert_eq!(
            route,
            r#"route: { "route": [ { "name": "edge", "host": "edge.example.com" }, { "name": "build", "host": "build.example.com" } ] }"#
        );
    }

    #[tokio::test]
    async fn approval_dispatch_uses_checked_actions_and_only_offers_proposed_grants() {
        use skyhook::tool::policy::ApprovalCoverage;
        let (_root, mut app) = draft_fixture().await;
        let original = approval_request();
        for proposed in [false, true] {
            let mut request = original.clone();
            for permission in &mut request.permissions {
                permission.proposed = None;
            }
            if proposed {
                request.permissions[0].proposed = Some(ApprovalCoverage::Exact);
            }
            let (reply, mut response) = oneshot::channel();
            let kind = PromptKind::Approval { request, reply };
            app.prompt(Prompt { id: 20, kind });
            assert_eq!(app.prompt_options().len(), if proposed { 4 } else { 3 });
            app.prompt_input_mut().choice = 2;
            app.answer();
            assert!(app.menu.is_some());
            assert_eq!(app.prompts.len(), 1);
            assert!(pending(&mut response));
            app.menu = None;
            app.prompt_input_mut().choice = if proposed { 3 } else { 1 };
            app.answer();
            let expected = if proposed {
                ApprovalReply::Grant
            } else {
                ApprovalReply::Deny
            };
            assert_eq!(response.await.unwrap().unwrap(), expected);
            assert!(app.prompts.is_empty());
        }
    }
}
