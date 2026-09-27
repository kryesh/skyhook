use super::*;
use skyhook::{
    agent::{Question, QuestionAnswer},
    remote::PromptAnswer,
};
use std::ops::Deref;

/// Editor and view travel with their prompt (and with each question page).
#[derive(Default)]
pub struct PromptInput {
    pub editor: Editor,
    pub choice: usize,
    pub body_scroll: usize,
    pub option_scroll: usize,
    /// Set by manual option scrolling; otherwise rendering reveals the choice.
    pub options_scrolled: bool,
}
impl PromptInput {
    fn reset_view(&mut self) {
        self.body_scroll = 0;
        self.option_scroll = 0;
        self.options_scrolled = false;
    }
}

fn summary(answer: &QuestionAnswer) -> String {
    match answer {
        QuestionAnswer::Text(text) => text.clone(),
        QuestionAnswer::Commented { answer, comment } => format!("{answer} — {comment}"),
    }
}

#[derive(Default)]
struct QuestionDraft {
    input: PromptInput,
    answer: Option<QuestionAnswer>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum QuestionPage {
    Question(usize),
    Review,
}
struct QuestionBatchState {
    page: QuestionPage,
    drafts: Vec<QuestionDraft>,
    review: PromptInput,
    editing: bool,
}
impl QuestionBatchState {
    fn new(count: usize) -> Self {
        Self {
            page: if count == 0 {
                QuestionPage::Review
            } else {
                QuestionPage::Question(0)
            },
            drafts: (0..count).map(|_| QuestionDraft::default()).collect(),
            review: PromptInput::default(),
            editing: false,
        }
    }
    fn index(&self) -> usize {
        match self.page {
            QuestionPage::Question(index) => index,
            QuestionPage::Review => self.drafts.len(),
        }
    }
    fn input(&self) -> &PromptInput {
        match self.page {
            QuestionPage::Question(index) => &self.drafts[index].input,
            QuestionPage::Review => &self.review,
        }
    }
    fn input_mut(&mut self) -> &mut PromptInput {
        match self.page {
            QuestionPage::Question(index) => &mut self.drafts[index].input,
            QuestionPage::Review => &mut self.review,
        }
    }
    fn select(&mut self, index: usize) {
        if index > self.drafts.len() || index == self.index() {
            return;
        }
        self.page = if index == self.drafts.len() {
            self.review.choice = 0;
            QuestionPage::Review
        } else {
            QuestionPage::Question(index)
        };
        self.editing = false;
        self.input_mut().reset_view();
    }
    fn missing(&self) -> Option<usize> {
        self.drafts.iter().position(|draft| draft.answer.is_none())
    }
}

/// Authentication owns a separate input, never an approval draft or a
/// question answer.
enum PromptState {
    Approval(PromptInput),
    Questions(QuestionBatchState),
    Authentication(PromptInput),
}
pub struct UiPrompt {
    request: Prompt,
    state: PromptState,
    /// In view in place of the composer; dismissing it keeps it pending.
    shown: bool,
    /// The focus of an ordinary prompt suspended by authentication.
    resume: Option<Focus>,
}
// Read-only projection preserves request consumers without exposing a way to
// replace its category/questions independently of the associated draft.
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
                PromptState::Questions(QuestionBatchState::new(questions.len()))
            }
            PromptKind::Authentication { .. } => {
                PromptState::Authentication(PromptInput::default())
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
            PromptState::Authentication(input) | PromptState::Approval(input) => input,
            PromptState::Questions(batch) => batch.input(),
        }
    }
    fn input_mut(&mut self) -> &mut PromptInput {
        match &mut self.state {
            PromptState::Authentication(input) | PromptState::Approval(input) => input,
            PromptState::Questions(batch) => batch.input_mut(),
        }
    }
    fn batch(&self) -> Option<&QuestionBatchState> {
        match &self.state {
            PromptState::Questions(batch) => Some(batch),
            _ => None,
        }
    }
    fn batch_mut(&mut self) -> Option<&mut QuestionBatchState> {
        match &mut self.state {
            PromptState::Questions(batch) => Some(batch),
            _ => None,
        }
    }
    pub(super) fn reject(self, error: String) {
        self.request.reject(error);
    }
}
impl Drop for PromptState {
    fn drop(&mut self) {
        if let Self::Authentication(input) = self {
            input.editor.clear_sensitive();
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
/// The approval document's tool arguments, then where the call comes from, so
/// the brief preview spends its characters on the arguments.
fn approval_preview(document: &serde_json::Value) -> String {
    use crate::tui::format::pretty;
    let mut parts = document.as_object().cloned().unwrap_or_default();
    let arguments = parts
        .shift_remove("tool")
        .map(|arguments| pretty(&arguments));
    let context = parts
        .iter()
        .map(|(part, value)| format!("{part}: {}", pretty(value)));
    let text: Vec<_> = arguments.into_iter().chain(context).collect();
    crate::text::brief(&text.join("\n"), 240)
}
/// Host-key and agent-key confirmations are answered by choice, not by text.
fn confirmation_items() -> Vec<(PromptAnswer, &'static str)> {
    vec![
        (PromptAnswer::Confirmed, "Confirm"),
        (PromptAnswer::Rejected, "Decline"),
    ]
}
#[derive(Clone, Copy)]
enum QuestionAction {
    Choice(usize),
    Write,
    Submit,
    Review,
}
fn question_items(question: Option<&Question>) -> Vec<(QuestionAction, &str)> {
    if let Some(question) = question {
        let options = question.options.iter().enumerate();
        options
            .map(|(index, option)| (QuestionAction::Choice(index), option.label.as_str()))
            .chain(std::iter::once((QuestionAction::Write, "Write an answer…")))
            .collect()
    } else {
        vec![
            (QuestionAction::Submit, "Submit answers"),
            (QuestionAction::Review, "Review again"),
        ]
    }
}

impl App {
    pub fn prompt_input(&self) -> &PromptInput {
        self.prompts.front().expect("active prompt").input()
    }
    pub fn prompt_input_mut(&mut self) -> &mut PromptInput {
        self.prompts.front_mut().expect("active prompt").input_mut()
    }
    pub fn question_index(&self) -> usize {
        self.prompts
            .front()
            .and_then(UiPrompt::batch)
            .map_or(0, QuestionBatchState::index)
    }
    pub fn question_editing(&self) -> bool {
        self.prompts
            .front()
            .and_then(UiPrompt::batch)
            .is_some_and(|batch| batch.editing)
    }
    pub(super) fn set_question_editing(&mut self, editing: bool) {
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
    pub fn prompt(&mut self, prompt: Prompt) {
        if matches!(prompt.kind, PromptKind::Authentication { .. }) {
            // Authentication is FIFO before ordinary prompts. Ordinary state
            // stays attached to its own queued request, including every page.
            let index = self
                .prompts
                .iter()
                .take_while(|p| matches!(p.kind, PromptKind::Authentication { .. }))
                .count();
            if index == 0
                && let Some(previous) = self.prompts.front_mut()
            {
                previous.resume = Some(self.focus);
            }
            self.prompts.insert(index, UiPrompt::new(prompt, false));
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
            Some(PromptKind::Authentication { .. }) => "authentication cancelled",
            Some(PromptKind::Questions {
                background: true, ..
            }) => "questions cancelled",
            // Dismissal is not cancellation: request and owned draft persist.
            Some(_) => {
                self.prompts[0].shown = false;
                return;
            }
            None => return,
        };
        let front = self.prompts[0].id;
        for prompt in self.take_prompts(|prompt| prompt.id == front) {
            prompt.reject(error.into());
        }
    }
    pub(super) fn reject_pending_questions(&mut self) {
        let questions = |prompt: &UiPrompt| matches!(prompt.kind, PromptKind::Questions { .. });
        for prompt in self.take_prompts(questions) {
            prompt.reject("questions cancelled by a new user prompt".into());
        }
    }
    pub(super) fn activate_prompt(&mut self) {
        let Some(prompt) = self.prompts.front_mut() else {
            return;
        };
        prompt.shown = true;
        self.focus = Focus::Composer;
        self.leader = None;
        self.overlay = None;
    }
    pub(super) fn scroll_prompt(&mut self, options: bool, delta: isize) {
        if self.prompts.is_empty() {
            return;
        }
        if options {
            let max = self
                .prompt_option_rows
                .saturating_sub(self.prompt_options_rect.height as usize);
            let input = self.prompt_input_mut();
            input.option_scroll = input.option_scroll.saturating_add_signed(delta).min(max);
            input.options_scrolled = true;
        } else {
            let max = self
                .prompt_body_rows
                .saturating_sub(self.prompt_body_rect.height as usize);
            let input = self.prompt_input_mut();
            input.body_scroll = input.body_scroll.saturating_add_signed(delta).min(max);
        }
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
            PromptKind::Questions { questions, .. } => {
                labels(question_items(questions.get(self.question_index())))
            }
            PromptKind::Authentication { prompt, .. } if prompt.kind.is_confirmation() => {
                labels(confirmation_items())
            }
            PromptKind::Authentication { .. } => vec![],
        }
    }
    pub fn prompt_text(&self) -> String {
        let Some(prompt) = self.prompts.front() else {
            return String::new();
        };
        match &prompt.kind {
            PromptKind::Approval { request: r, .. } => format!(
                "Permission · agent {} · {}\n{}",
                crate::tui::format::agent_label(&r.agent),
                r.tool,
                approval_preview(&r.arguments)
            ),
            PromptKind::Questions {
                agent, questions, ..
            } => questions.get(self.question_index()).map_or_else(
                || {
                    let batch = prompt.batch().unwrap();
                    let answers = questions
                        .iter()
                        .zip(&batch.drafts)
                        .filter_map(|(question, draft)| {
                            draft
                                .answer
                                .as_ref()
                                .map(|answer| format!("{}: {}", question.id, summary(answer)))
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    format!("Review answers\n{answers}")
                },
                |q| {
                    format!(
                        "Agent {} · question {}/{}\n{}",
                        crate::tui::format::agent_label(agent),
                        self.question_index() + 1,
                        questions.len(),
                        q.prompt
                    )
                },
            ),
            PromptKind::Authentication { prompt, .. } => {
                let target = (prompt.target.as_ref())
                    .map_or_else(String::new, |target| format!(" · {target}"));
                let origin = crate::tui::model::target_suffix(&prompt.origin);
                format!("Authentication{target}{origin}\n{}", prompt.message)
            }
        }
    }
    pub fn multiple_questions(&self) -> bool {
        matches!(self.prompts.front().map(|p| &p.kind), Some(PromptKind::Questions { questions, .. }) if questions.len() > 1)
    }
    pub(super) fn select_question(&mut self, index: usize) {
        if let Some(batch) = self.prompts.front_mut().and_then(UiPrompt::batch_mut) {
            batch.select(index);
        }
    }
    pub(super) fn switch_question(&mut self, delta: isize) {
        let Some(batch) = self.prompts.front().and_then(UiPrompt::batch) else {
            return;
        };
        let count = batch.drafts.len();
        if count > 1 {
            let index = batch.index();
            if index >= count && delta > 0 {
                return;
            }
            self.select_question(index.saturating_add_signed(delta).min(count - 1));
        }
    }
    pub(super) fn invalidate_question_answer(&mut self) {
        if let Some(batch) = self.prompts.front_mut().and_then(UiPrompt::batch_mut)
            && let QuestionPage::Question(index) = batch.page
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
            (PromptKind::Questions { questions, .. }, PromptState::Questions(batch)) => {
                let items = question_items(questions.get(batch.index()));
                let Some(&(action, _)) = items.get(batch.input().choice) else {
                    return;
                };
                match (batch.page, action) {
                    (QuestionPage::Question(index), QuestionAction::Choice(option)) => {
                        let draft = &mut batch.drafts[index];
                        let text = draft.input.editor.text();
                        let answer = questions[index].options[option].label.clone();
                        draft.answer = Some(if text.trim().is_empty() {
                            QuestionAnswer::Text(answer)
                        } else {
                            let comment = text.to_owned();
                            QuestionAnswer::Commented { answer, comment }
                        });
                    }
                    (QuestionPage::Question(index), QuestionAction::Write) => {
                        let draft = &mut batch.drafts[index];
                        let text = draft.input.editor.text();
                        if text.trim().is_empty() {
                            return;
                        }
                        draft.answer = Some(QuestionAnswer::Text(text.to_owned()));
                    }
                    (QuestionPage::Review, QuestionAction::Review) => {
                        batch.select(0);
                        return;
                    }
                    (QuestionPage::Review, QuestionAction::Submit) => {
                        if let Some(index) = batch.missing() {
                            batch.select(index);
                            return;
                        }
                    }
                    _ => return,
                }
                if let QuestionPage::Question(index) = batch.page
                    && questions.len() > 1
                {
                    let next = if index + 1 < questions.len() {
                        index + 1
                    } else {
                        batch.missing().unwrap_or(questions.len())
                    };
                    batch.select(next);
                    return;
                }
            }
            (PromptKind::Authentication { .. }, PromptState::Authentication(_)) => {}
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
                PromptState::Questions(batch),
            ) => {
                let answers = batch.drafts.iter_mut();
                let answers = answers.map(|draft| draft.answer.take().expect("confirmed answer"));
                let ids = questions.into_iter().map(|question| question.id);
                let _ = reply.send(Ok(ids.zip(answers).collect()));
            }
            (PromptKind::Authentication { prompt, reply }, PromptState::Authentication(input)) => {
                let answer = if prompt.kind.is_confirmation() {
                    confirmation_items().swap_remove(input.choice).0
                } else {
                    PromptAnswer::Secret(input.editor.take_sensitive())
                };
                let _ = reply.send(Ok(answer));
            }
            _ => unreachable!("request and draft are constructed together"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    use KeyCode::{Char, Down, Enter, Esc, Left, Right, Tab, Up};
    use skyhook::remote::SensitivePromptKind;

    fn confirmed_answers(app: &App) -> QuestionReply {
        let Some(prompt) = app.prompts.front() else {
            return QuestionReply::new();
        };
        let PromptKind::Questions { questions, .. } = &prompt.kind else {
            return QuestionReply::new();
        };
        let drafts = &prompt.batch().unwrap().drafts;
        let answers = questions.iter().zip(drafts);
        answers
            .filter_map(|(q, draft)| draft.answer.clone().map(|answer| (q.id.clone(), answer)))
            .collect()
    }
    fn has_suspended_prompt(app: &App) -> bool {
        app.prompts.iter().any(|prompt| prompt.resume.is_some())
    }
    fn suggestions() -> Vec<QuestionOption> {
        ["First", "Second"]
            .map(|label| QuestionOption {
                label: label.into(),
                description: format!("Use {label}"),
            })
            .into()
    }
    fn paste(app: &mut App, text: &str) {
        app.event(Event::Paste(text.into()));
    }
    fn authentication(
        app: &mut App,
        id: u64,
        kind: skyhook::remote::SensitivePromptKind,
    ) -> oneshot::Receiver<Result<PromptAnswer, String>> {
        let (reply, response) = oneshot::channel();
        let prompt = skyhook::remote::SensitivePrompt {
            kind,
            message: format!("SSH prompt {id}"),
            target: None,
            origin: skyhook::target::TargetRef::Root,
        };
        let kind = PromptKind::Authentication { prompt, reply };
        app.prompt(Prompt { id, kind });
        response
    }

    #[tokio::test]
    async fn questions_open_and_accept_answers_while_inspecting_agents() {
        let (_root, mut app) = fixture().await;
        let child = push_child(&mut app, 1).await;
        app.select(child.clone());
        app.editor.set("preserved draft".into());

        for focus in [Focus::Tree, Focus::Content] {
            app.focus = focus;
            let option = QuestionOption {
                label: "Continue".into(),
                description: "Keep working".into(),
            };
            let answer = question(&mut app, "Choose a direction".into(), vec![option]);
            assert!(app.prompt_shown());
            let screen = draw(&mut app);
            assert!(app.tree_rect.height > 0);
            assert!(screen.contains("Choose a direction") && screen.contains("Continue"));
            key(&mut app, Enter, M::NONE);
            let reply = answer.await.unwrap().unwrap();
            assert_eq!(reply, texts(&[("answer", "Continue")]));
            assert_eq!(app.selected, child);
            assert!(app.focus == focus);
            assert_eq!(app.editor.text(), "preserved draft");
        }
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

        assert!(draw(&mut app).contains("Question 1/3 · ←→ switch · Tab edit"));
        key(&mut app, Left, M::NONE);
        assert_eq!(app.question_index(), 0);
        key(&mut app, Up, M::NONE);
        assert_eq!(app.prompt_input_mut().choice, 2);
        key(&mut app, Down, M::NONE);
        assert_eq!(app.prompt_input_mut().choice, 0);
        key(&mut app, Down, M::NONE);
        paste(&mut app, "comment");
        key(&mut app, Left, M::NONE);
        assert_eq!(app.question_index(), 0);
        assert_eq!(app.prompt_input_mut().editor.cursor(), 6);
        let screen = draw(&mut app);
        assert!(screen.contains("←→ cursor · Tab switch questions") && screen.contains("comment"));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 24)).unwrap();
        terminal
            .draw(|frame| crate::tui::render::draw(frame, &mut app))
            .unwrap();
        let cursor = terminal.get_cursor_position().unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(cursor.x, cursor.y)].symbol(), "t");
        let bold = ratatui::style::Modifier::BOLD;
        assert_eq!(
            buffer[(2, app.prompt_body_rect.y)].fg,
            crate::tui::theme::THEME.warning
        );
        let title = &buffer[(2, app.prompt_body_rect.y + 1)];
        assert!(title.fg == crate::tui::theme::THEME.primary && title.modifier.contains(bold));
        let label = &buffer[(4, app.prompt_options_rect.y)];
        assert_eq!(
            (label.symbol(), label.fg),
            ("1", crate::tui::theme::THEME.fg)
        );
        assert!(!label.modifier.contains(bold));
        key(&mut app, Tab, M::NONE);
        app.prompt_input_mut().body_scroll = 5;
        app.prompt_input_mut().option_scroll = 5;
        key(&mut app, Right, M::NONE);
        let input = app.prompt_input_mut();
        assert_eq!((input.body_scroll, input.option_scroll), (0, 0));
        paste(&mut app, "draft");
        press(&mut app, &[Tab, Right, Right]);
        assert_eq!(app.question_index(), 2);
        assert!(app.prompt_input_mut().editor.text().is_empty());
        assert!(confirmed_answers(&app).is_empty());
        assert!(pending(&mut response));
        key(&mut app, Esc, M::NONE);
        assert!(!app.prompt_shown());
        assert_eq!(app.prompts.len(), 1);
        assert!(pending(&mut response));
        assert!(!draw(&mut app).contains("/attention"));
        chord(&mut app, KeyCode::Char('r'));
        assert!(app.prompt_shown());
        assert_eq!(app.question_index(), 2);
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

        app.select_question(2); // Review is a bounded page, not an invalid question index.
        app.prompt_input_mut().choice = 2;
        app.answer();
        assert_eq!(app.question_index(), 2);
        app.prompt_input_mut().choice = 0;
        app.answer(); // Submit returns to the first missing answer instead.
        assert_eq!(app.question_index(), 0);
        assert!(confirmed_answers(&app).is_empty());
        assert!(pending(&mut response));
        app.prompt_input_mut().editor.set(String::new());

        key(&mut app, Right, M::NONE);
        paste(&mut app, "second");
        key(&mut app, Enter, M::NONE);
        assert_eq!(app.question_index(), 0);
        assert_eq!(confirmed_answers(&app).len(), 1);
        key(&mut app, Enter, M::NONE); // Empty free-form stays unanswered.
        assert_eq!(app.question_index(), 0);
        assert!(pending(&mut response));
        paste(&mut app, "first");
        key(&mut app, Enter, M::NONE);
        assert_eq!(app.prompt_input_mut().editor.text(), "second");
        key(&mut app, Enter, M::NONE);
        assert_eq!(app.question_index(), 2); // Review, not submission.
        assert!(pending(&mut response));
        key(&mut app, Right, M::NONE);
        assert_eq!(app.question_index(), 2);
        key(&mut app, Left, M::NONE); // Review can return to last question.
        assert_eq!(app.question_index(), 1);
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
    async fn single_question_and_authentication_editing_are_plain_and_secret() {
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
        let ssh = authentication(&mut app, 100, SensitivePromptKind::Password);
        assert!(app.prompt_options().is_empty());
        app.prompt_input_mut().editor.insert("sec");
        paste(&mut app, "ret");
        key(&mut app, Left, M::NONE);
        assert_eq!(app.prompt_input_mut().editor.cursor(), 5);
        assert!(!draw(&mut app).contains("secret"));
        key(&mut app, Enter, M::NONE);
        let PromptAnswer::Secret(secret) = ssh.await.unwrap().unwrap() else {
            panic!("expected a secret")
        };
        assert_eq!(secret.expose(), "secret");
        assert!(app.prompts.is_empty());
        // Confirmations offer a choice instead of an editor: typed or pasted text
        // has nowhere to go, so it can never pass for an answer.
        for (kind, presses, expected) in [
            (
                SensitivePromptKind::AgentConfirmation,
                &[Enter][..],
                "Confirmed",
            ),
            (
                SensitivePromptKind::HostConfirmation,
                &[Down, Enter][..],
                "Rejected",
            ),
        ] {
            let confirmation = authentication(&mut app, 101, kind);
            assert_eq!(app.prompt_options(), ["Confirm", "Decline"]);
            assert!(draw(&mut app).contains("SSH prompt 101"));
            press(&mut app, &[Char('n'), Char('o')]);
            paste(&mut app, "no");
            assert!(app.prompt_input().editor.text().is_empty());
            press(&mut app, presses);
            let answer = format!("{:?}", confirmation.await.unwrap().unwrap());
            assert_eq!(answer, expected);
        }
        assert!(app.prompts.is_empty());
    }

    #[tokio::test]
    async fn question_comments_survive_ssh_preemption_and_batch_review() {
        let (_root, mut app) = fixture().await;
        let batch = vec![
            ("answer", "Choose", suggestions()),
            ("next", "Anything else?", vec![]),
        ];
        let response = questions(&mut app, false, batch);

        key(&mut app, Down, M::NONE);
        paste(&mut app, "my comment");
        // An authentication prompt pre-empts the question and restores its draft.
        let ssh = authentication(&mut app, 100, SensitivePromptKind::Password);
        key(&mut app, Esc, M::NONE);
        assert!(ssh.await.unwrap().is_err());
        let input = app.prompt_input_mut();
        assert_eq!((input.choice, input.editor.text()), (1, "my comment"));
        key(&mut app, Enter, M::NONE);
        assert!(app.prompt_input_mut().editor.text().is_empty());
        paste(&mut app, "freeform");
        key(&mut app, Enter, M::NONE);
        assert!(app.prompt_text().contains("my comment"));
        press(&mut app, &[Down, Enter]); // Review again.
        assert_eq!(app.question_index(), 0);
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
        expected.insert("answer".into(), commented);
        assert_eq!(response.await.unwrap().unwrap(), expected);
        // A blank comment is dropped, free-form text is untrimmed and
        // whitespace-only free-form input never confirms.
        let response = question(&mut app, "Choose".into(), suggestions());
        app.prompt_input_mut().choice = 2;
        app.prompt_input_mut().editor.set(" free form ".into());
        app.answer();
        let reply = response.await.unwrap().unwrap();
        assert_eq!(reply, texts(&[("answer", " free form ")]));
        let mut response = question(&mut app, "Write".into(), vec![]);
        app.prompt_input_mut().editor.set(" ".into());
        app.answer();
        assert!(confirmed_answers(&app).is_empty());
        assert!(pending(&mut response));
    }

    #[tokio::test]
    async fn authentication_preempts_overlays_and_restores_partial_question_batches() {
        let (_root, mut app) = fixture().await;
        for finish in 0..3 {
            app.editor.set("composer draft".into());
            app.focus = Focus::Tree;
            let batch = vec![
                ("answer", "First question", vec![]),
                ("second", "Second question", vec![]),
            ];
            let answer = questions(&mut app, false, batch);
            paste(&mut app, "first answer");
            key(&mut app, Enter, M::NONE);
            paste(&mut app, "unfinished answer");
            let input = app.prompt_input_mut();
            (
                input.body_scroll,
                input.option_scroll,
                input.options_scrolled,
            ) = (3, 2, true);
            app.info("Details", "An open menu".into());
            app.overlay = Some(Overlay::Search(Editor::default()));
            let response = authentication(&mut app, 100, SensitivePromptKind::Password);
            assert!(matches!(app.input_target(), InputTarget::Prompt));
            assert!(app.overlay.is_none());
            assert_eq!(app.prompts.front().unwrap().id, 100);
            assert!(app.prompt_input_mut().editor.text().is_empty());
            assert!(confirmed_answers(&app).is_empty());
            paste(&mut app, "ssh secret");
            let screen = draw(&mut app);
            assert!(screen.contains("SSH prompt 100") && !screen.contains("ssh secret"));
            match finish {
                0 => {
                    key(&mut app, Enter, M::NONE);
                    let PromptAnswer::Secret(secret) = response.await.unwrap().unwrap() else {
                        panic!("expected a secret")
                    };
                    assert_eq!(secret.expose(), "ssh secret");
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
            assert!(app.prompt_shown() && app.focus == Focus::Tree);
            assert_eq!(app.question_index(), 1);
            let input = app.prompt_input_mut();
            assert_eq!(input.editor.text(), "unfinished answer");
            assert_eq!(
                (
                    input.body_scroll,
                    input.option_scroll,
                    input.options_scrolled
                ),
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
    async fn authentication_is_fifo_and_takes_priority_over_dismissed_requests() {
        let (_root, mut app) = fixture().await;
        let answer = question(&mut app, "Question".into(), vec![]);
        paste(&mut app, "saved answer");
        key(&mut app, Esc, M::NONE);
        let first = authentication(&mut app, 100, SensitivePromptKind::Password);
        paste(&mut app, "first secret");
        let second = authentication(&mut app, 101, SensitivePromptKind::Password);
        assert!(app.prompt_shown());
        assert_eq!(
            app.prompts.iter().map(|p| p.id).collect::<Vec<_>>(),
            [100, 101, 1]
        );
        assert_eq!(app.prompt_input_mut().editor.text(), "first secret");
        key(&mut app, Esc, M::NONE);
        assert!(first.await.unwrap().is_err());
        assert!(app.prompt_shown());
        assert!(app.prompt_input_mut().editor.text().is_empty());
        assert_eq!(app.prompts.front().unwrap().id, 101);
        key(&mut app, Esc, M::NONE);
        assert!(second.await.unwrap().is_err());
        assert!(!app.prompt_shown());
        assert_eq!(app.prompt_input_mut().editor.text(), "saved answer");
        chord(&mut app, KeyCode::Char('r'));
        key(&mut app, Enter, M::NONE);
        let reply = answer.await.unwrap().unwrap();
        assert_eq!(reply, texts(&[("answer", "saved answer")]));
    }

    #[tokio::test]
    async fn closing_the_only_prompt_redraws() {
        let (_root, mut app) = fixture().await;
        drop(question(&mut app, "Question".into(), vec![]));
        app.dirty = false;
        app.tick();
        assert!(app.prompts.is_empty());
        assert!(app.dirty);
    }

    #[tokio::test]
    async fn cancelled_suspended_questions_do_not_leak_into_later_prompts() {
        let (_root, mut app) = fixture().await;
        let answer = question(&mut app, "Question".into(), vec![]);
        paste(&mut app, "abandoned answer");
        let response = authentication(&mut app, 100, SensitivePromptKind::Password);
        drop(answer);
        app.tick();
        key(&mut app, Esc, M::NONE);
        assert!(response.await.unwrap().is_err());
        assert!(!has_suspended_prompt(&app));
        assert!(app.prompts.is_empty());
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
        app.paused = true;
        app.submit("A new direction".into());
        assert!(rejected.await.unwrap().is_err());
        assert!(app.prompts.is_empty());
        assert_eq!(app.queue.len(), 1);
        chord(&mut app, KeyCode::Char('r'));
        assert!(!app.prompt_shown());
    }

    #[tokio::test]
    async fn approval_preview_leads_with_the_tool_arguments() {
        let mut document = crate::interaction::tests::approval_request()
            .await
            .arguments;
        let arguments = crate::tui::format::pretty(&document["tool"]);
        document["network_origin"] = json!("https://redirect.test");
        let preview = approval_preview(&document);
        assert!(
            preview.starts_with(&crate::text::brief(&arguments, 240)),
            "{preview}"
        );
        assert!(
            preview.ends_with(r#"network_origin: "https://redirect.test""#),
            "{preview}"
        );
    }

    #[tokio::test]
    async fn approval_dispatch_uses_checked_actions_and_only_offers_proposed_grants() {
        use skyhook::tool::policy::ApprovalCoverage;
        let (_root, mut app) = draft_fixture().await;
        let original = crate::interaction::tests::approval_request().await;
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
            assert!(app.menu().is_some());
            assert_eq!(app.prompts.len(), 1);
            assert!(pending(&mut response));
            app.overlay = None;
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
