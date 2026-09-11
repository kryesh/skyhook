use super::*;

#[derive(Default)]
pub(super) struct QuestionDraft {
    editor: Editor,
    choice: usize,
}
// Only non-authentication drafts are suspended; secrets never enter this state.
pub(super) struct SuspendedPrompt {
    id: u64,
    editor: Editor,
    choice: usize,
    body_scroll: usize,
    option_scroll: usize,
    question_index: usize,
    question_drafts: HashMap<usize, QuestionDraft>,
    question_editing: bool,
    answers: serde_json::Map<String, Value>,
    active: bool,
    focus: Focus,
}

impl App {
    pub fn prompt(&mut self, prompt: Prompt) {
        if matches!(prompt.kind, PromptKind::Authentication(_)) {
            // SSH may be blocking other work. Keep authentication FIFO, but put
            // it ahead of ordinary questions/permissions, even dismissed ones.
            let index = self
                .prompts
                .iter()
                .take_while(|p| matches!(p.kind, PromptKind::Authentication(_)))
                .count();
            if index == 0 {
                if let Some(previous) = self.prompts.front() {
                    self.suspended_prompt = Some(SuspendedPrompt {
                        id: previous.id,
                        editor: std::mem::take(&mut self.prompt_editor),
                        choice: self.prompt_choice,
                        body_scroll: self.prompt_body_scroll,
                        option_scroll: self.prompt_option_scroll,
                        question_index: self.question_index,
                        question_drafts: std::mem::take(&mut self.question_drafts),
                        question_editing: self.question_editing,
                        answers: std::mem::take(&mut self.answers),
                        active: self.prompt_active,
                        focus: self.focus,
                    });
                }
                self.prompts.push_front(prompt);
                self.reset_prompt();
            } else {
                self.prompts.insert(index, prompt);
            }
            self.activate_prompt();
        } else {
            // Ordinary requests do not depend on pane focus, but don't steal
            // input from an open menu/search or reopen dismissed requests.
            let show = self.prompts.is_empty();
            self.prompts.push_back(prompt);
            if show {
                self.prompt_active = true;
                self.leader = None;
            }
        }
        self.dirty = true;
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
        self.preview_theme();
    }
    pub(super) fn reset_prompt(&mut self) {
        self.prompt_editor.clear_sensitive();
        self.prompt_editor = Editor::default();
        self.prompt_choice = 0;
        self.reset_prompt_view();
        self.question_index = 0;
        self.question_drafts.clear();
        self.question_editing = false;
        self.answers.clear();
        if self
            .suspended_prompt
            .as_ref()
            .is_some_and(|saved| self.prompts.front().is_some_and(|p| p.id == saved.id))
        {
            let saved = self.suspended_prompt.take().unwrap();
            self.prompt_editor = saved.editor;
            self.prompt_choice = saved.choice;
            self.prompt_body_scroll = saved.body_scroll;
            self.prompt_option_scroll = saved.option_scroll;
            self.question_index = saved.question_index;
            self.question_drafts = saved.question_drafts;
            self.question_editing = saved.question_editing;
            self.answers = saved.answers;
            self.prompt_active = saved.active;
            self.focus = saved.focus;
        } else if self
            .suspended_prompt
            .as_ref()
            .is_some_and(|saved| !self.prompts.iter().any(|p| p.id == saved.id))
        {
            self.suspended_prompt = None;
        }
    }
    pub(super) fn reset_prompt_view(&mut self) {
        self.prompt_body_scroll = 0;
        self.prompt_option_scroll = 0;
        self.prompt_reveal = true;
    }
    pub(super) fn scroll_prompt(&mut self, options: bool, delta: isize) {
        if options {
            let max = self
                .prompt_option_rows
                .saturating_sub(self.prompt_options_rect.height as usize);
            self.prompt_option_scroll = self
                .prompt_option_scroll
                .saturating_add_signed(delta)
                .min(max);
            self.prompt_reveal = false;
        } else {
            let max = self
                .prompt_body_rows
                .saturating_sub(self.prompt_body_rect.height as usize);
            self.prompt_body_scroll = self
                .prompt_body_scroll
                .saturating_add_signed(delta)
                .min(max);
        }
    }
    pub fn prompt_options(&self) -> Vec<String> {
        let Some(prompt) = self.prompts.front() else {
            return vec![];
        };
        match &prompt.kind {
            PromptKind::Approval(request) => {
                let mut options = vec!["Allow once".into(), "Deny".into(), "Details".into()];
                if request
                    .permissions
                    .iter()
                    .any(|p| p.proposed_grant.is_some())
                {
                    options.push("Allow proposed scope".into());
                }
                options
            }
            PromptKind::Questions { questions, .. } => {
                if let Some(question) = questions.get(self.question_index) {
                    question
                        .options
                        .iter()
                        .map(|o| format!("{} — {}", o.label, o.description))
                        .chain(std::iter::once("Write an answer…".into()))
                        .collect()
                } else {
                    vec!["Submit answers".into(), "Review again".into()]
                }
            }
            PromptKind::Authentication(_) => vec![],
        }
    }
    pub fn prompt_text(&self) -> String {
        let Some(prompt) = self.prompts.front() else {
            return String::new();
        };
        match &prompt.kind {
            PromptKind::Approval(r) => format!(
                "Permission · agent {} · {}\n{}",
                crate::tui::format::agent_label(&r.agent),
                r.tool,
                crate::tui::format::brief(&model::pretty(&r.arguments), 240)
            ),
            PromptKind::Questions { agent, questions } => {
                questions.get(self.question_index).map_or_else(
                    || format!("Review answers\n{}", model::pretty(&self.answers)),
                    |q| {
                        format!(
                            "Agent {} · question {}/{}\n{}",
                            crate::tui::format::agent_label(agent),
                            self.question_index + 1,
                            questions.len(),
                            q.prompt
                        )
                    },
                )
            }
            PromptKind::Authentication(p) => format!("Authentication\n{}", p.message),
        }
    }
    pub fn multiple_questions(&self) -> bool {
        matches!(self.prompts.front().map(|p| &p.kind),
            Some(PromptKind::Questions { questions, .. }) if questions.len() > 1)
    }
    pub(super) fn select_question(&mut self, index: usize) {
        if index == self.question_index {
            return;
        }
        // Drafts are independent of confirmed answers, including untouched and
        // unanswered questions. Moving never confirms an answer.
        if matches!(self.prompts.front().map(|p| &p.kind),
            Some(PromptKind::Questions { questions, .. }) if self.question_index < questions.len())
        {
            self.question_drafts.insert(
                self.question_index,
                QuestionDraft {
                    editor: std::mem::take(&mut self.prompt_editor),
                    choice: self.prompt_choice,
                },
            );
        } else {
            self.prompt_editor = Editor::default();
        }
        self.question_index = index;
        self.prompt_choice = 0;
        self.question_editing = false;
        self.reset_prompt_view();
        self.restore_question_answer();
    }
    pub(super) fn switch_question(&mut self, delta: isize) {
        let Some(PromptKind::Questions { questions, .. }) = self.prompts.front().map(|p| &p.kind)
        else {
            return;
        };
        if questions.len() > 1 {
            if self.question_index >= questions.len() && delta > 0 {
                return;
            }
            let index = self
                .question_index
                .saturating_add_signed(delta)
                .min(questions.len() - 1);
            self.select_question(index);
        }
    }
    pub(super) fn invalidate_question_answer(&mut self) {
        if let Some(PromptKind::Questions { questions, .. }) = self.prompts.front().map(|p| &p.kind)
            && let Some(question) = questions.get(self.question_index)
        {
            self.answers.remove(&question.id);
        }
    }
    pub(super) fn restore_question_answer(&mut self) {
        if let Some(draft) = self.question_drafts.remove(&self.question_index) {
            self.prompt_editor = draft.editor;
            self.prompt_choice = draft.choice;
            return;
        }
        let Some(PromptKind::Questions { questions, .. }) = self.prompts.front().map(|p| &p.kind)
        else {
            return;
        };
        let Some(question) = questions.get(self.question_index) else {
            return;
        };
        let Some(answer) = self.answers.get(&question.id) else {
            return;
        };
        let (label, comment) = if let Some(text) = answer.as_str() {
            (text, "")
        } else {
            (
                answer
                    .get("answer")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                answer
                    .get("comment")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
        };
        self.prompt_choice = question
            .options
            .iter()
            .position(|o| o.label == label)
            .unwrap_or(question.options.len());
        self.prompt_editor
            .set(if self.prompt_choice < question.options.len() {
                comment.to_owned()
            } else {
                label.to_owned()
            });
    }
    pub(super) fn answer(&mut self) {
        let Some(prompt) = self.prompts.front() else {
            return;
        };
        let value = match &prompt.kind {
            PromptKind::Approval(request) => match self.prompt_choice {
                0 => Some(PromptResponse::Approval(ApprovalReply::Allow)),
                1 => Some(PromptResponse::Approval(ApprovalReply::Deny)),
                2 => {
                    let text = format!(
                        "Agent {}\nTool {}\nArguments\n{}\nPermissions\n{:#?}",
                        request.agent,
                        request.tool,
                        model::pretty(&request.arguments),
                        request.permissions
                    );
                    self.info("Permission details", text);
                    return;
                }
                _ => Some(PromptResponse::Approval(ApprovalReply::Grant)),
            },
            PromptKind::Authentication(_) => Some(PromptResponse::Authentication(
                self.prompt_editor.take_sensitive(),
            )),
            PromptKind::Questions { questions, .. } => {
                if let Some(question) = questions.get(self.question_index) {
                    let text = &self.prompt_editor.text;
                    let answer = if let Some(option) = question.options.get(self.prompt_choice) {
                        if text.trim().is_empty() {
                            Value::String(option.label.clone())
                        } else {
                            serde_json::json!({"answer": option.label, "comment": text})
                        }
                    } else {
                        if text.trim().is_empty() {
                            return;
                        }
                        Value::String(text.clone())
                    };
                    self.answers.insert(question.id.clone(), answer);
                    if questions.len() == 1 {
                        Some(PromptResponse::Questions(
                            self.answers.values().next().cloned().unwrap_or(Value::Null),
                        ))
                    } else {
                        // Keep sequential review, but wrap to skipped questions
                        // before offering submission at the end of the batch.
                        let next = if self.question_index + 1 < questions.len() {
                            self.question_index + 1
                        } else {
                            questions
                                .iter()
                                .position(|question| !self.answers.contains_key(&question.id))
                                .unwrap_or(questions.len())
                        };
                        self.select_question(next);
                        None
                    }
                } else if self.prompt_choice == 1 {
                    self.select_question(0);
                    None
                } else if let Some(index) = questions
                    .iter()
                    .position(|question| !self.answers.contains_key(&question.id))
                {
                    self.select_question(index);
                    None
                } else {
                    Some(PromptResponse::Questions(Value::Object(
                        self.answers.clone(),
                    )))
                }
            }
        };
        if let Some(value) = value {
            if let Some(prompt) = self.prompts.pop_front() {
                let _ = prompt.reply.send(Ok(value));
            }
            self.reset_prompt();
            if self.prompts.is_empty() {
                self.prompt_active = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    fn suggestions() -> Vec<QuestionOption> {
        ["First", "Second"]
            .into_iter()
            .map(|label| QuestionOption {
                label: label.into(),
                description: format!("Use {label}"),
            })
            .collect()
    }
    fn authentication(app: &mut App, id: u64) -> oneshot::Receiver<Result<PromptResponse, String>> {
        let (reply, response) = oneshot::channel();
        app.prompt(Prompt {
            id,
            kind: PromptKind::Authentication(skyhook::remote::SensitivePrompt {
                kind: skyhook::remote::SensitivePromptKind::Password,
                message: format!("SSH password {id}"),
            }),
            reply,
        });
        response
    }
    #[tokio::test]
    async fn questions_open_and_accept_answers_while_inspecting_agents() {
        let (_root, mut app) = fixture().await;
        let mut child = app.projection.agents[0].clone();
        child.id = child.id.child(1);
        child.name = "worker".into();
        child.terminal = false;
        app.projection.agents.push(child.clone());
        app.select(child.id.clone());
        app.editor.set("preserved draft".into());

        for focus in [Focus::Tree, Focus::Content] {
            app.focus = focus;
            let answer = question(
                &mut app,
                "Choose a direction".into(),
                vec![QuestionOption {
                    label: "Continue".into(),
                    description: "Keep working".into(),
                }],
            );
            assert!(app.prompt_active);
            let screen = draw(&mut app);
            assert!(app.tree_rect.height > 0);
            assert!(screen.contains("Choose a direction"));
            assert!(screen.contains("Continue"));
            key(&mut app, KeyCode::Enter, M::NONE);
            assert!(matches!(
                answer.await.unwrap().unwrap(),
                PromptResponse::Questions(Value::String(value)) if value == "Continue"
            ));
            assert_eq!(app.selected, child.id);
            assert!(app.focus == focus);
            assert_eq!(app.editor.text, "preserved draft");
        }
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn question_navigation_preserves_unanswered_drafts_and_clamps() {
        let (_root, mut app) = fixture().await;
        let mut response = question(&mut app, "Choose".into(), suggestions());
        if let PromptKind::Questions { questions, .. } = &mut app.prompts.front_mut().unwrap().kind
        {
            for id in ["middle", "last"] {
                questions.push(Question {
                    id: id.into(),
                    prompt: id.into(),
                    options: vec![],
                });
            }
        }
        assert!(draw(&mut app).contains("Question 1/3 · ←→ switch · Tab edit"));
        key(&mut app, KeyCode::Left, M::NONE);
        assert_eq!(app.question_index, 0);
        key(&mut app, KeyCode::Down, M::NONE);
        app.event(Event::Paste("comment".into()));
        key(&mut app, KeyCode::Left, M::NONE);
        assert_eq!(app.question_index, 0);
        assert_eq!(app.prompt_editor.cursor, 6);
        let screen = draw(&mut app);
        assert!(screen.contains("←→ cursor · Tab switch questions"));
        assert!(screen.contains("commen▏t"));
        key(&mut app, KeyCode::Tab, M::NONE);
        app.prompt_body_scroll = 5;
        app.prompt_option_scroll = 5;
        key(&mut app, KeyCode::Right, M::NONE);
        assert_eq!(app.prompt_body_scroll, 0);
        assert_eq!(app.prompt_option_scroll, 0);
        app.event(Event::Paste("draft".into()));
        key(&mut app, KeyCode::Tab, M::NONE);
        key(&mut app, KeyCode::Right, M::NONE);
        key(&mut app, KeyCode::Right, M::NONE);
        assert_eq!(app.question_index, 2);
        assert!(app.prompt_editor.text.is_empty());
        assert!(app.answers.is_empty());
        assert!(matches!(
            response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        key(&mut app, KeyCode::Left, M::NONE);
        assert_eq!(app.prompt_editor.text, "draft");
        key(&mut app, KeyCode::Left, M::NONE);
        assert_eq!(app.prompt_choice, 1);
        assert_eq!(app.prompt_editor.text, "comment");
        assert_eq!(app.prompt_editor.cursor, 6);
        let ssh = authentication(&mut app, 100);
        key(&mut app, KeyCode::Esc, M::NONE);
        assert!(ssh.await.unwrap().is_err());
        key(&mut app, KeyCode::Right, M::NONE);
        assert_eq!(app.prompt_editor.text, "draft");
        assert!(app.answers.is_empty());
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn question_navigation_enter_wraps_skips_before_review_and_submit() {
        let (_root, mut app) = fixture().await;
        let mut response = question(&mut app, "First".into(), vec![]);
        if let PromptKind::Questions { questions, .. } = &mut app.prompts.front_mut().unwrap().kind
        {
            questions.push(Question {
                id: "last".into(),
                prompt: "Last".into(),
                options: vec![],
            });
        }
        key(&mut app, KeyCode::Right, M::NONE);
        app.event(Event::Paste("second".into()));
        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(app.question_index, 0);
        assert_eq!(app.answers.len(), 1);
        key(&mut app, KeyCode::Enter, M::NONE); // Empty free-form stays unanswered.
        assert_eq!(app.question_index, 0);
        assert!(matches!(
            response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        app.event(Event::Paste("first".into()));
        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(app.prompt_editor.text, "second");
        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(app.question_index, 2); // Review, not submission.
        assert!(matches!(
            response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        key(&mut app, KeyCode::Right, M::NONE);
        assert_eq!(app.question_index, 2);
        key(&mut app, KeyCode::Left, M::NONE); // Review can return to last question.
        assert_eq!(app.question_index, 1);
        app.event(Event::Paste(" revised".into()));
        assert!(!app.answers.contains_key("last"));
        key(&mut app, KeyCode::Tab, M::NONE);
        key(&mut app, KeyCode::Left, M::NONE);
        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(app.prompt_editor.text, "second revised");
        key(&mut app, KeyCode::Enter, M::NONE);
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(
            matches!(response.await.unwrap().unwrap(), PromptResponse::Questions(value)
            if value == json!({"answer": "first", "last": "second revised"}))
        );
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn question_navigation_does_not_change_single_question_or_auth_editing() {
        let (_root, mut app) = fixture().await;
        let response = question(&mut app, "Single".into(), vec![]);
        app.event(Event::Paste("abc".into()));
        key(&mut app, KeyCode::Left, M::NONE);
        assert_eq!(app.prompt_editor.cursor, 2);
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(
            matches!(response.await.unwrap().unwrap(), PromptResponse::Questions(value) if value == "abc")
        );
        let ssh = authentication(&mut app, 100);
        app.event(Event::Paste("secret".into()));
        key(&mut app, KeyCode::Left, M::NONE);
        assert_eq!(app.prompt_editor.cursor, 5);
        assert!(!draw(&mut app).contains("secret"));
        key(&mut app, KeyCode::Esc, M::NONE);
        assert!(ssh.await.unwrap().is_err());
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn question_comments_survive_ssh_preemption_and_batch_review() {
        let (_root, mut app) = fixture().await;
        let response = question(&mut app, "Choose".into(), suggestions());
        if let PromptKind::Questions { questions, .. } = &mut app.prompts.front_mut().unwrap().kind
        {
            questions.push(Question {
                id: "next".into(),
                prompt: "Anything else?".into(),
                options: vec![],
            });
        }
        key(&mut app, KeyCode::Down, M::NONE);
        app.event(Event::Paste("my comment".into()));
        let ssh = authentication(&mut app, 100);
        key(&mut app, KeyCode::Esc, M::NONE);
        assert!(ssh.await.unwrap().is_err());
        assert_eq!(app.prompt_choice, 1);
        assert_eq!(app.prompt_editor.text, "my comment");
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(app.prompt_editor.text.is_empty());
        app.event(Event::Paste("freeform".into()));
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(app.prompt_text().contains("my comment"));
        key(&mut app, KeyCode::Down, M::NONE); // Review again.
        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(app.question_index, 0);
        assert_eq!(app.prompt_choice, 1);
        assert_eq!(app.prompt_editor.text, "my comment");
        app.event(Event::Paste(" amended".into()));
        key(&mut app, KeyCode::Enter, M::NONE);
        assert_eq!(app.prompt_editor.text, "freeform");
        key(&mut app, KeyCode::Enter, M::NONE);
        key(&mut app, KeyCode::Enter, M::NONE);
        let PromptResponse::Questions(value) = response.await.unwrap().unwrap() else {
            panic!("wrong response")
        };
        assert_eq!(
            value,
            serde_json::json!({"answer": {"answer": "Second", "comment": "my comment amended"}, "next": "freeform"})
        );
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn authentication_preempts_overlays_and_restores_partial_question_batches() {
        let (_root, mut app) = fixture().await;
        for finish in 0..3 {
            app.editor.set("composer draft".into());
            app.focus = Focus::Tree;
            let answer = question(&mut app, "First question".into(), vec![]);
            if let PromptKind::Questions { questions, .. } =
                &mut app.prompts.front_mut().unwrap().kind
            {
                questions.push(Question {
                    id: "second".into(),
                    prompt: "Second question".into(),
                    options: vec![],
                });
            }
            app.event(Event::Paste("first answer".into()));
            key(&mut app, KeyCode::Enter, M::NONE);
            app.event(Event::Paste("unfinished answer".into()));
            app.prompt_body_scroll = 3;
            app.prompt_option_scroll = 2;
            app.info("Details", "An open menu".into());
            app.search_editor = Some(Editor::default());
            let response = authentication(&mut app, 100);
            assert!(matches!(app.input_target(), InputTarget::Prompt));
            assert!(app.menu.is_none());
            assert!(app.search_editor.is_none());
            assert_eq!(app.prompts.front().unwrap().id, 100);
            assert!(app.prompt_editor.text.is_empty());
            assert!(app.answers.is_empty());
            app.event(Event::Paste("ssh secret".into()));
            let screen = draw(&mut app);
            assert!(screen.contains("SSH password 100"));
            assert!(!screen.contains("ssh secret"));
            match finish {
                0 => {
                    key(&mut app, KeyCode::Enter, M::NONE);
                    let PromptResponse::Authentication(secret) = response.await.unwrap().unwrap()
                    else {
                        panic!("wrong response kind")
                    };
                    assert_eq!(secret.expose(), "ssh secret");
                }
                1 => {
                    key(&mut app, KeyCode::Esc, M::NONE);
                    assert!(response.await.unwrap().is_err());
                }
                _ => {
                    drop(response);
                    app.tick();
                }
            }
            assert!(app.prompt_active);
            assert!(app.focus == Focus::Tree);
            assert_eq!(app.question_index, 1);
            assert_eq!(app.prompt_editor.text, "unfinished answer");
            assert_eq!(app.prompt_body_scroll, 3);
            assert_eq!(app.prompt_option_scroll, 2);
            assert_eq!(app.answers["answer"], "first answer");
            assert_eq!(app.editor.text, "composer draft");
            assert!(app.suspended_prompt.is_none());
            key(&mut app, KeyCode::Enter, M::NONE);
            key(&mut app, KeyCode::Enter, M::NONE);
            let PromptResponse::Questions(value) = answer.await.unwrap().unwrap() else {
                panic!("wrong response kind")
            };
            assert_eq!(value["answer"], "first answer");
            assert_eq!(value["second"], "unfinished answer");
        }
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn authentication_is_fifo_and_takes_priority_over_dismissed_requests() {
        let (_root, mut app) = fixture().await;
        let answer = question(&mut app, "Question".into(), vec![]);
        app.event(Event::Paste("saved answer".into()));
        key(&mut app, KeyCode::Esc, M::NONE);
        let first = authentication(&mut app, 100);
        app.event(Event::Paste("first secret".into()));
        let second = authentication(&mut app, 101);
        assert!(app.prompt_active);
        assert_eq!(
            app.prompts.iter().map(|p| p.id).collect::<Vec<_>>(),
            vec![100, 101, 1]
        );
        assert_eq!(app.prompt_editor.text, "first secret");
        key(&mut app, KeyCode::Esc, M::NONE);
        assert!(first.await.unwrap().is_err());
        assert!(app.prompt_active);
        assert!(app.prompt_editor.text.is_empty());
        assert_eq!(app.prompts.front().unwrap().id, 101);
        key(&mut app, KeyCode::Esc, M::NONE);
        assert!(second.await.unwrap().is_err());
        assert!(!app.prompt_active);
        assert_eq!(app.prompt_editor.text, "saved answer");
        app.command("attention");
        key(&mut app, KeyCode::Enter, M::NONE);
        assert!(
            matches!(answer.await.unwrap().unwrap(), PromptResponse::Questions(Value::String(value)) if value == "saved answer")
        );
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn cancelled_suspended_questions_do_not_leak_into_later_prompts() {
        let (_root, mut app) = fixture().await;
        let answer = question(&mut app, "Question".into(), vec![]);
        app.event(Event::Paste("abandoned answer".into()));
        let response = authentication(&mut app, 100);
        drop(answer);
        app.tick();
        key(&mut app, KeyCode::Esc, M::NONE);
        assert!(response.await.unwrap().is_err());
        assert!(app.suspended_prompt.is_none());
        assert!(app.prompt_editor.text.is_empty());
        let _next = question(&mut app, "Next question".into(), vec![]);
        assert!(app.prompt_editor.text.is_empty());
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn authentication_moves_secret_and_clears_editor() {
        let (_root, mut app) = fixture().await;
        let (reply, response) = oneshot::channel();
        app.prompt(Prompt {
            id: 1,
            kind: PromptKind::Authentication(skyhook::remote::SensitivePrompt {
                kind: skyhook::remote::SensitivePromptKind::Password,
                message: "Password".into(),
            }),
            reply,
        });
        app.prompt_editor.insert("secret");
        app.answer();
        let PromptResponse::Authentication(secret) = response.await.unwrap().unwrap() else {
            panic!("wrong response kind")
        };
        assert_eq!(secret.expose(), "secret");
        assert!(app.prompt_editor.text.is_empty());
        assert!(app.prompts.is_empty());
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }
}
