use super::*;
use skyhook::agent::Question;
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum Answer {
    Text(String),
    Choice {
        label: String,
        comment: Option<String>,
    },
}
impl Answer {
    fn into_value(self) -> Value {
        match self {
            Self::Text(text)
            | Self::Choice {
                label: text,
                comment: None,
            } => Value::String(text),
            Self::Choice {
                label,
                comment: Some(comment),
            } => {
                serde_json::json!({"answer": label, "comment": comment})
            }
        }
    }
    fn summary(&self) -> String {
        match self {
            Self::Text(text)
            | Self::Choice {
                label: text,
                comment: None,
            } => text.clone(),
            Self::Choice {
                label,
                comment: Some(comment),
            } => format!("{label} — {comment}"),
        }
    }
}

#[derive(Default)]
struct QuestionDraft {
    input: PromptInput,
    answer: Option<Answer>,
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
    /// Visibility and focus of an ordinary prompt suspended by authentication.
    resume: Option<(bool, Focus)>,
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
    fn new(request: Prompt) -> Self {
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

    #[cfg(test)]
    fn set_background(&mut self, value: bool) {
        if let PromptKind::Questions { background, .. } = &mut self.request.kind {
            *background = value;
        }
    }
    #[cfg(test)]
    fn push_question(&mut self, question: Question) {
        if let PromptKind::Questions { questions, .. } = &mut self.request.kind {
            questions.push(question);
            self.batch_mut()
                .unwrap()
                .drafts
                .push(QuestionDraft::default());
        } else {
            panic!("expected question prompt");
        }
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
) -> Vec<Item<ApprovalAction>> {
    let mut items = vec![
        Item::new(ApprovalAction::Allow, "Allow once", ""),
        Item::new(ApprovalAction::Deny, "Deny", ""),
        Item::new(ApprovalAction::Details, "Details", ""),
    ];
    if request
        .permissions
        .iter()
        .any(|p| p.proposed_grant.is_some())
    {
        items.push(Item::new(ApprovalAction::Grant, "Allow proposed scope", ""));
    }
    items
}
#[derive(Clone, Copy)]
enum QuestionAction {
    Choice(usize),
    Write,
    Submit,
    Review,
}
fn question_items(question: Option<&Question>) -> Vec<Item<QuestionAction>> {
    if let Some(question) = question {
        question
            .options
            .iter()
            .enumerate()
            .map(|(index, option)| {
                Item::new(
                    QuestionAction::Choice(index),
                    &option.label,
                    &option.description,
                )
            })
            .chain(std::iter::once(Item::new(
                QuestionAction::Write,
                "Write an answer…",
                "",
            )))
            .collect()
    } else {
        vec![
            Item::new(QuestionAction::Submit, "Submit answers", ""),
            Item::new(QuestionAction::Review, "Review again", ""),
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
    pub fn prompt(&mut self, prompt: Prompt) {
        let prompt = UiPrompt::new(prompt);
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
                previous.resume = Some((self.prompt_active, self.focus));
            }
            self.prompts.insert(index, prompt);
            self.activate_prompt();
        } else {
            let show = self.prompts.is_empty();
            self.prompts.push_back(prompt);
            if show {
                self.prompt_active = true;
                self.leader = None;
            }
        }
        self.dirty = true;
    }
    pub(super) fn cancel_prompt(&mut self) {
        let error = match self.prompts.front().map(|prompt| &prompt.kind) {
            Some(PromptKind::Authentication { .. }) => Some("authentication cancelled"),
            Some(PromptKind::Questions {
                background: true, ..
            }) => Some("questions cancelled"),
            _ => None,
        };
        if let Some(error) = error {
            if let Some(prompt) = self.prompts.pop_front() {
                prompt.reject(error.into());
            }
            self.reset_prompt();
        } else {
            // Dismissal is not cancellation: request and owned draft persist.
            self.prompt_active = false;
        }
        self.dirty = true;
    }
    pub(super) fn reject_pending_questions(&mut self) {
        let previous = self.prompts.front().map(|p| p.id);
        for prompt in std::mem::take(&mut self.prompts) {
            if matches!(prompt.kind, PromptKind::Questions { .. }) {
                prompt.reject("questions cancelled by a new user prompt".into());
            } else {
                self.prompts.push_back(prompt);
            }
        }
        if previous != self.prompts.front().map(|p| p.id) {
            self.reset_prompt();
        }
        if self.prompts.is_empty() {
            self.prompt_active = false;
        }
    }
    pub(super) fn activate_prompt(&mut self) {
        if self.prompts.is_empty() {
            return;
        }
        self.prompt_active = true;
        self.focus = Focus::Composer;
        self.leader = None;
        self.menu = None;
        self.search_editor = None;
    }
    pub(super) fn reset_prompt(&mut self) {
        // The new front already owns its input. Only suspended visibility and
        // pane focus need restoration; removing a stale item drops its draft.
        if let Some(prompt) = self.prompts.front_mut() {
            if let Some((active, focus)) = prompt.resume.take() {
                self.prompt_active = active;
                self.focus = focus;
            }
        } else {
            self.prompt_active = false;
        }
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
    pub fn prompt_options(&self) -> Vec<String> {
        let Some(prompt) = self.prompts.front() else {
            return vec![];
        };
        match &prompt.kind {
            PromptKind::Approval { request, .. } => approval_items(request)
                .into_iter()
                .map(|item| item.label)
                .collect(),
            PromptKind::Questions { questions, .. } => {
                question_items(questions.get(self.question_index()))
                    .into_iter()
                    .map(|item| {
                        if matches!(item.value, QuestionAction::Choice(_)) {
                            format!("{} — {}", item.label, item.detail)
                        } else {
                            item.label
                        }
                    })
                    .collect()
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
                crate::tui::format::brief(&crate::tui::format::pretty(&r.arguments), 240)
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
                                .map(|answer| format!("{}: {}", question.id, answer.summary()))
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
                format!("Authentication\n{}", prompt.message)
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
                let Some(action) = approval_items(request)
                    .get(input.choice)
                    .map(|item| item.value)
                else {
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
                let Some(action) = question_items(questions.get(batch.index()))
                    .get(batch.input().choice)
                    .map(|item| item.value)
                else {
                    return;
                };
                match (batch.page, action) {
                    (QuestionPage::Question(index), QuestionAction::Choice(option)) => {
                        let draft = &mut batch.drafts[index];
                        let text = draft.input.editor.text();
                        draft.answer = Some(Answer::Choice {
                            label: questions[index].options[option].label.clone(),
                            comment: (!text.trim().is_empty()).then(|| text.to_owned()),
                        });
                    }
                    (QuestionPage::Question(index), QuestionAction::Write) => {
                        let draft = &mut batch.drafts[index];
                        let text = draft.input.editor.text();
                        if text.trim().is_empty() {
                            return;
                        }
                        draft.answer = Some(Answer::Text(text.to_owned()));
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
        let prompt = self.prompts.pop_front().unwrap();
        let choice = prompt.input().choice;
        match prompt.request.kind {
            PromptKind::Approval { request, reply } => {
                let action = approval_items(&request)[choice].value;
                let answer = match action {
                    ApprovalAction::Allow => ApprovalReply::Allow,
                    ApprovalAction::Deny => ApprovalReply::Deny,
                    ApprovalAction::Grant => ApprovalReply::Grant,
                    ApprovalAction::Details => unreachable!("details does not finish a prompt"),
                };
                let _ = reply.send(Ok(answer));
            }
            PromptKind::Questions {
                questions, reply, ..
            } => {
                let mut state = prompt.state;
                let PromptState::Questions(batch) = &mut state else {
                    unreachable!("question draft")
                };
                let value = if questions.len() == 1 {
                    batch.drafts[0]
                        .answer
                        .take()
                        .expect("confirmed answer")
                        .into_value()
                } else {
                    Value::Object(
                        questions
                            .into_iter()
                            .zip(&mut batch.drafts)
                            .map(|(question, draft)| {
                                (
                                    question.id,
                                    draft.answer.take().expect("confirmed answer").into_value(),
                                )
                            })
                            .collect(),
                    )
                };
                let _ = reply.send(Ok(value));
            }
            PromptKind::Authentication { reply, .. } => {
                let mut state = prompt.state;
                let PromptState::Authentication(input) = &mut state else {
                    unreachable!("authentication input")
                };
                let _ = reply.send(Ok(input.editor.take_sensitive()));
            }
        }
        self.reset_prompt();
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    use KeyCode::{Down, Enter, Esc, Left, Right, Tab, Up};

    fn confirmed_answers(app: &App) -> HashMap<String, Answer> {
        let Some(prompt) = app.prompts.front() else {
            return HashMap::new();
        };
        let PromptKind::Questions { questions, .. } = &prompt.kind else {
            return HashMap::new();
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
    /// Append a free-form question to the front question batch.
    fn push_question(app: &mut App, id: &str, prompt: &str) {
        app.prompts.front_mut().unwrap().push_question(Question {
            id: id.into(),
            prompt: prompt.into(),
            options: vec![],
        });
    }
    fn paste(app: &mut App, text: &str) {
        app.event(Event::Paste(text.into()));
    }
    fn authentication(
        app: &mut App,
        id: u64,
    ) -> oneshot::Receiver<Result<skyhook::remote::SecretValue, String>> {
        let (reply, response) = oneshot::channel();
        let prompt = skyhook::remote::SensitivePrompt {
            kind: skyhook::remote::SensitivePromptKind::Password,
            message: format!("SSH password {id}"),
        };
        let kind = PromptKind::Authentication { prompt, reply };
        app.prompt(Prompt { id, kind });
        response
    }

    #[tokio::test]
    async fn questions_open_and_accept_answers_while_inspecting_agents() {
        let (_root, mut app) = fixture().await;
        let mut child = app.projection.agents[0].clone();
        child.id = child.id.child(1);
        child.name = "worker".into();
        app.projection.agents.push(child.clone());
        app.projection.reopen_agent(&child.id);
        app.select(child.id.clone());
        app.editor.set("preserved draft".into());

        for focus in [Focus::Tree, Focus::Content] {
            app.focus = focus;
            let option = QuestionOption {
                label: "Continue".into(),
                description: "Keep working".into(),
            };
            let answer = question(&mut app, "Choose a direction".into(), vec![option]);
            assert!(app.prompt_active);
            let screen = draw(&mut app);
            assert!(app.tree_rect.height > 0);
            assert!(screen.contains("Choose a direction") && screen.contains("Continue"));
            key(&mut app, Enter, M::NONE);
            assert_eq!(answer.await.unwrap().unwrap(), "Continue");
            assert_eq!(app.selected, child.id);
            assert!(app.focus == focus);
            assert_eq!(app.editor.text(), "preserved draft");
        }
        app.session().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn question_navigation_preserves_unanswered_drafts_and_clamps() {
        let (_root, mut app) = fixture().await;
        let mut response = question(&mut app, "Choose".into(), suggestions());
        for id in ["middle", "last"] {
            push_question(&mut app, id, id);
        }

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
        let p = crate::tui::render::Palette::new();
        let bold = ratatui::style::Modifier::BOLD;
        assert_eq!(buffer[(2, app.prompt_body_rect.y)].fg, p.warning);
        let title = &buffer[(2, app.prompt_body_rect.y + 1)];
        assert!(title.fg == p.accent && title.modifier.contains(bold));
        let label = &buffer[(4, app.prompt_options_rect.y)];
        assert_eq!((label.symbol(), label.fg), ("1", p.fg));
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
        assert!(!app.prompt_active);
        assert_eq!(app.prompts.len(), 1);
        assert!(pending(&mut response));
        assert!(!draw(&mut app).contains("/attention"));
        chord(&mut app, KeyCode::Char('r'));
        assert!(app.prompt_active);
        assert_eq!(app.question_index(), 2);
        key(&mut app, Left, M::NONE);
        assert_eq!(app.prompt_input_mut().editor.text(), "draft");
        key(&mut app, Left, M::NONE);
        let input = app.prompt_input_mut();
        assert_eq!((input.choice, input.editor.text()), (1, "comment"));
        assert_eq!(input.editor.cursor(), 6);
        let ssh = authentication(&mut app, 100);
        key(&mut app, Esc, M::NONE);
        assert!(ssh.await.unwrap().is_err());
        key(&mut app, Right, M::NONE);
        assert_eq!(app.prompt_input_mut().editor.text(), "draft");
        assert!(confirmed_answers(&app).is_empty());
        app.session().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn question_navigation_enter_wraps_skips_before_review_and_submit() {
        let (_root, mut app) = fixture().await;
        let mut response = question(&mut app, "First".into(), vec![]);
        push_question(&mut app, "last", "Last");

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
        let value = response.await.unwrap().unwrap();
        assert_eq!(value, json!({"answer": "first", "last": "second revised"}));
        app.session().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn single_question_and_authentication_editing_are_plain_and_secret() {
        let (_root, mut app) = fixture().await;
        let response = question(&mut app, "Single".into(), vec![]);
        paste(&mut app, "abc");
        key(&mut app, Left, M::NONE);
        assert_eq!(app.prompt_input_mut().editor.cursor(), 2);
        key(&mut app, Enter, M::NONE);
        assert_eq!(response.await.unwrap().unwrap(), "abc");
        let ssh = authentication(&mut app, 100);
        app.prompt_input_mut().editor.insert("sec");
        paste(&mut app, "ret");
        key(&mut app, Left, M::NONE);
        assert_eq!(app.prompt_input_mut().editor.cursor(), 5);
        assert!(!draw(&mut app).contains("secret"));
        key(&mut app, Enter, M::NONE);
        assert_eq!(ssh.await.unwrap().unwrap().expose(), "secret");
        assert!(app.prompts.is_empty());
        app.session().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn question_comments_survive_ssh_preemption_and_batch_review() {
        let (_root, mut app) = fixture().await;
        let response = question(&mut app, "Choose".into(), suggestions());
        push_question(&mut app, "next", "Anything else?");

        key(&mut app, Down, M::NONE);
        paste(&mut app, "my comment");
        let ssh = authentication(&mut app, 100);
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
        assert_eq!(
            response.await.unwrap().unwrap(),
            json!({"answer": {"answer": "Second", "comment": "my comment amended"}, "next": "freeform"})
        );
        // Wire grammar: a blank comment is dropped, free-form text is untrimmed
        // and whitespace-only free-form input never confirms.
        let response = question(&mut app, "Choose".into(), suggestions());
        app.prompt_input_mut().choice = 2;
        app.prompt_input_mut().editor.set(" free form ".into());
        app.answer();
        assert_eq!(response.await.unwrap().unwrap(), " free form ");
        let mut response = question(&mut app, "Write".into(), vec![]);
        app.prompt_input_mut().editor.set(" ".into());
        app.answer();
        assert!(confirmed_answers(&app).is_empty());
        assert!(pending(&mut response));
        app.session().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn authentication_preempts_overlays_and_restores_partial_question_batches() {
        let (_root, mut app) = fixture().await;
        for finish in 0..3 {
            app.editor.set("composer draft".into());
            app.focus = Focus::Tree;
            let answer = question(&mut app, "First question".into(), vec![]);
            push_question(&mut app, "second", "Second question");
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
            app.search_editor = Some(Editor::default());
            let response = authentication(&mut app, 100);
            assert!(matches!(app.input_target(), InputTarget::Prompt));
            assert!(app.menu.is_none() && app.search_editor.is_none());
            assert_eq!(app.prompts.front().unwrap().id, 100);
            assert!(app.prompt_input_mut().editor.text().is_empty());
            assert!(confirmed_answers(&app).is_empty());
            paste(&mut app, "ssh secret");
            let screen = draw(&mut app);
            assert!(screen.contains("SSH password 100") && !screen.contains("ssh secret"));
            match finish {
                0 => {
                    key(&mut app, Enter, M::NONE);
                    assert_eq!(response.await.unwrap().unwrap().expose(), "ssh secret");
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
            assert!(app.prompt_active && app.focus == Focus::Tree);
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
            let first = Answer::Text("first answer".into());
            assert_eq!(confirmed_answers(&app)["answer"], first);
            assert_eq!(app.editor.text(), "composer draft");
            assert!(!has_suspended_prompt(&app));
            press(&mut app, &[Enter, Enter]);
            let value = answer.await.unwrap().unwrap();
            assert_eq!(
                value,
                json!({"answer": "first answer", "second": "unfinished answer"})
            );
        }
        app.session().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn authentication_is_fifo_and_takes_priority_over_dismissed_requests() {
        let (_root, mut app) = fixture().await;
        let answer = question(&mut app, "Question".into(), vec![]);
        paste(&mut app, "saved answer");
        key(&mut app, Esc, M::NONE);
        let first = authentication(&mut app, 100);
        paste(&mut app, "first secret");
        let second = authentication(&mut app, 101);
        assert!(app.prompt_active);
        assert_eq!(
            app.prompts.iter().map(|p| p.id).collect::<Vec<_>>(),
            [100, 101, 1]
        );
        assert_eq!(app.prompt_input_mut().editor.text(), "first secret");
        key(&mut app, Esc, M::NONE);
        assert!(first.await.unwrap().is_err());
        assert!(app.prompt_active);
        assert!(app.prompt_input_mut().editor.text().is_empty());
        assert_eq!(app.prompts.front().unwrap().id, 101);
        key(&mut app, Esc, M::NONE);
        assert!(second.await.unwrap().is_err());
        assert!(!app.prompt_active);
        assert_eq!(app.prompt_input_mut().editor.text(), "saved answer");
        chord(&mut app, KeyCode::Char('r'));
        key(&mut app, Enter, M::NONE);
        assert_eq!(answer.await.unwrap().unwrap(), "saved answer");
        app.session().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_suspended_questions_do_not_leak_into_later_prompts() {
        let (_root, mut app) = fixture().await;
        let answer = question(&mut app, "Question".into(), vec![]);
        paste(&mut app, "abandoned answer");
        let response = authentication(&mut app, 100);
        drop(answer);
        app.tick();
        key(&mut app, Esc, M::NONE);
        assert!(response.await.unwrap().is_err());
        assert!(!has_suspended_prompt(&app));
        assert!(app.prompts.is_empty());
        let cancelled = question(&mut app, "Background question".into(), vec![]);
        app.prompts.front_mut().unwrap().set_background(true);
        push_question(&mut app, "second", "Second question");
        paste(&mut app, "partial answer");
        key(&mut app, Enter, M::NONE);
        assert_eq!(confirmed_answers(&app).len(), 1);
        key(&mut app, Esc, M::NONE);
        assert!(cancelled.await.unwrap().is_err());
        assert!(app.prompts.is_empty());
        assert!(confirmed_answers(&app).is_empty());
        chord(&mut app, KeyCode::Char('r'));
        assert!(!app.prompt_active);
        let rejected = question(&mut app, "Next question".into(), vec![]);
        assert!(app.prompt_input_mut().editor.text().is_empty());
        key(&mut app, Esc, M::NONE);
        app.paused = true;
        app.submit("A new direction".into());
        assert!(rejected.await.unwrap().is_err());
        assert!(app.prompts.is_empty());
        assert_eq!(app.queue.len(), 1);
        chord(&mut app, KeyCode::Char('r'));
        assert!(!app.prompt_active);
        app.session().unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn approval_dispatch_uses_checked_actions_and_only_offers_proposed_grants() {
        use skyhook::tool::policy::ApprovalGrant;
        let (_root, mut app) = draft_fixture().await;
        let original = crate::interaction::tests::approval_request().await;
        for proposed in [false, true] {
            let mut request = original.clone();
            for permission in &mut request.permissions {
                permission.proposed_grant = None;
            }
            if proposed {
                let permission = request.permissions.first_mut().unwrap();
                let resource = permission.resource.clone();
                permission.proposed_grant =
                    Some(ApprovalGrant::exact(permission.capability, resource));
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
