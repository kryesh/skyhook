//! Batches questions and routes answers through host handlers or stable agent jobs.

use futures_util::{StreamExt, stream::FuturesUnordered};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::{Mutex, oneshot};

use crate::{
    agent::{HarnessError, Question, QuestionError, QuestionHandler},
    identity::{AgentId, JobId},
    job::JobManager,
    provider::protocol::ToolCall,
};

pub(super) struct QuestionCoordinator {
    jobs: JobManager,
    handler: Option<Arc<dyn QuestionHandler>>,
    pending: Mutex<HashMap<JobId, PendingQuestion>>,
    batches: Mutex<HashMap<AgentId, QuestionBatch>>,
}

#[derive(Clone)]
struct PendingQuestion {
    ask_jobs: Vec<(String, JobId)>,
    questions: Vec<Question>,
    merged: bool,
    answering: HashSet<JobId>,
}

struct PendingAsk {
    context: crate::tool::ToolContext,
    question: Question,
    result: oneshot::Sender<Result<serde_json::Value, String>>,
}

#[derive(Default)]
struct QuestionBatch {
    expected: Option<usize>,
    pending: Vec<PendingAsk>,
}

impl QuestionCoordinator {
    pub(super) fn new(jobs: JobManager, handler: Option<Arc<dyn QuestionHandler>>) -> Self {
        Self {
            jobs,
            handler,
            pending: Mutex::new(HashMap::new()),
            batches: Mutex::new(HashMap::new()),
        }
    }

    async fn owning_agent_job(&self, mut job: JobId) -> Result<JobId, HarnessError> {
        loop {
            let envelope = self.jobs.snapshot(job).await?;
            if envelope.tool == "agent" {
                return Ok(job);
            }
            job = envelope.parent.ok_or_else(|| {
                HarnessError::Agent("child question has no owning agent job".to_owned())
            })?;
        }
    }

    pub(super) async fn coordinate_question(
        self: &Arc<Self>,
        context: crate::tool::ToolContext,
        question: Question,
    ) -> Result<serde_json::Value, crate::tool::ToolError> {
        // Enforce before batching or waiting on a job input channel: omitting the
        // host handler alone would leave background root questions waiting forever.
        if context.agent.parent().is_none()
            && !context
                .capabilities
                .contains(crate::tool::policy::Capability::Interactive)
        {
            return Err(crate::tool::ToolError::Denied(
                "root questions require the interactive capability".to_owned(),
            ));
        }
        let (result, received) = oneshot::channel();
        let agent = context.agent.clone();
        let launch = {
            let mut batches = self.batches.lock().await;
            let batch = batches.entry(agent.clone()).or_default();
            let first = batch.pending.is_empty();
            batch.pending.push(PendingAsk {
                context,
                question,
                result,
            });
            batch
                .expected
                .map_or(first, |expected| batch.pending.len() >= expected)
        };
        if launch {
            let runtime = self.clone();
            tokio::spawn(async move {
                tokio::task::yield_now().await;
                let batch = runtime
                    .batches
                    .lock()
                    .await
                    .remove(&agent)
                    .unwrap_or_default()
                    .pending;
                runtime.present_question_batch(agent, batch).await;
            });
        }
        received
            .await
            .map_err(|_| crate::tool::ToolError::Failed("question batch stopped".to_owned()))?
            .map_err(crate::tool::ToolError::Failed)
    }

    pub(super) async fn prepare_question_batch(&self, agent: &AgentId, calls: &[ToolCall]) {
        let count = calls
            .iter()
            .filter(|call| {
                call.name == "ask"
                    && call
                        .arguments
                        .get("bg")
                        .is_none_or(serde_json::Value::is_boolean)
                    && serde_json::from_value::<Question>(call.arguments.clone()).is_ok()
            })
            .count();
        if count > 0 {
            self.batches
                .lock()
                .await
                .entry(agent.clone())
                .or_default()
                .expected = Some(count);
        }
    }

    async fn present_question_batch(&self, agent: AgentId, batch: Vec<PendingAsk>) {
        if batch.is_empty() {
            return;
        }
        let questions = batch
            .iter()
            .map(|pending| pending.question.clone())
            .collect::<Vec<_>>();
        if let Some(duplicate) = duplicate_question_id(&questions) {
            send_question_error(batch, format!("duplicate question id `{duplicate}`"));
            return;
        }
        // Only explicit background asks expose their own input state. Marking a
        // foreground ask WaitingInput would cause its native call to return early.
        for pending in &batch {
            if self
                .jobs
                .is_background(pending.context.job)
                .await
                .unwrap_or(false)
                && let Err(error) = self
                    .jobs
                    .request_input(
                        pending.context.job,
                        json!(crate::agent::QuestionOutput {
                            questions: vec![pending.question.clone()]
                        }),
                    )
                    .await
            {
                send_question_error(batch, error.to_string());
                return;
            }
        }
        if agent.parent().is_some() {
            self.present_child_questions(batch, questions).await;
        } else {
            self.present_root_questions(agent, batch, questions).await;
        }
    }

    // A foreground ask inside a background script does not block the owning
    // agent either. Stop at the agent boundary: its launch mode belongs to its
    // parent, not to this agent's own question interaction.
    async fn is_effectively_background(&self, mut job: JobId) -> Result<bool, HarnessError> {
        loop {
            let envelope = self.jobs.snapshot(job).await?;
            if envelope.tool == "agent" {
                return Ok(false);
            }
            if self.jobs.is_background(job).await? {
                return Ok(true);
            }
            match envelope.parent {
                Some(parent) => job = parent,
                None => return Ok(false),
            }
        }
    }

    async fn present_root_questions(
        &self,
        agent: AgentId,
        batch: Vec<PendingAsk>,
        questions: Vec<Question>,
    ) {
        let mut background = false;
        for ask in &batch {
            match self.is_effectively_background(ask.context.job).await {
                Ok(true) => {
                    background = true;
                    break;
                }
                Ok(false) => {}
                Err(error) => {
                    send_question_error(batch, error.to_string());
                    return;
                }
            }
        }
        let mut answers = batch
            .iter()
            .enumerate()
            .map(|(index, pending)| {
                let context = pending.context.clone();
                async move {
                    (
                        index,
                        context.receive().await.map_err(|error| error.to_string()),
                    )
                }
            })
            .collect::<FuturesUnordered<_>>();
        let ids = questions
            .iter()
            .map(|question| question.id.clone())
            .collect::<Vec<_>>();
        let mut pending = batch.into_iter().map(Some).collect::<Vec<_>>();
        // Retain the normal host UI, but also allow a background ask to be
        // answered through its durable job's input channel.
        let answer = async {
            match &self.handler {
                Some(handler) => handler
                    .ask_with_background(agent, questions, background)
                    .await
                    .map_err(|error| error.to_string()),
                None => std::future::pending().await,
            }
        };
        tokio::pin!(answer);
        if self.handler.is_none() {
            for item in &mut pending {
                if let Some(ask) = item.as_ref()
                    && !self
                        .jobs
                        .is_background(ask.context.job)
                        .await
                        .unwrap_or(false)
                {
                    let _ = item
                        .take()
                        .unwrap()
                        .result
                        .send(Err(QuestionError::Unavailable.to_string()));
                }
            }
        }
        while pending.iter().any(Option::is_some) {
            tokio::select! {
                answer = &mut answer => {
                    let results = match answer {
                        Ok(answer) => split_answers(&ids, answer),
                        Err(error) => ids.iter().map(|_| Err(error.clone())).collect(),
                    };
                    for (ask, result) in pending.iter_mut().zip(results) {
                        if let Some(ask) = ask.take() {
                            let _ = ask.result.send(result);
                        }
                    }
                }
                Some((index, result)) = answers.next() => {
                    if let Some(ask) = pending[index].take() {
                        let _ = ask.result.send(result);
                    }
                }
            }
        }
    }

    async fn present_child_questions(&self, batch: Vec<PendingAsk>, questions: Vec<Question>) {
        let ask_jobs = batch
            .iter()
            .map(|pending| (pending.question.id.clone(), pending.context.job))
            .collect::<Vec<_>>();
        let output = json!(crate::agent::QuestionOutput { questions });
        let owner_job = match self.open_child_questions(ask_jobs, output).await {
            Ok(owner_job) => owner_job,
            Err(error) => {
                send_question_error(batch, error.to_string());
                return;
            }
        };
        let mut answers = batch
            .into_iter()
            .map(|pending| async move {
                let answer = pending.context.receive().await;
                let resolved = self
                    .resolve_child_question(owner_job, &[pending.context.job])
                    .await;
                let result = resolved
                    .map_err(|error| error.to_string())
                    .and_then(|()| answer.map_err(|error| error.to_string()));
                let _ = pending.result.send(result);
            })
            .collect::<FuturesUnordered<_>>();
        while answers.next().await.is_some() {}
    }

    pub(super) async fn open_child_questions(
        &self,
        ask_jobs: Vec<(String, JobId)>,
        output: serde_json::Value,
    ) -> Result<JobId, HarnessError> {
        let ask_job = ask_jobs
            .first()
            .map(|(_, job)| *job)
            .ok_or_else(|| HarnessError::Agent("question batch is empty".to_owned()))?;
        let owner_job = self.owning_agent_job(ask_job).await?;
        let questions: crate::agent::QuestionOutput = serde_json::from_value(output)
            .map_err(|error| HarnessError::Agent(error.to_string()))?;
        let mut pending = self.pending.lock().await;
        let mut combined = pending.get(&owner_job).cloned().unwrap_or(PendingQuestion {
            ask_jobs: Vec::new(),
            questions: Vec::new(),
            merged: false,
            answering: HashSet::new(),
        });
        for (id, _) in &ask_jobs {
            if combined.ask_jobs.iter().any(|(existing, _)| existing == id) {
                return Err(HarnessError::Agent(format!("duplicate question id `{id}`")));
            }
        }
        combined.ask_jobs.extend(ask_jobs);
        combined.merged |= combined.ask_jobs.len() > 1;
        combined.questions.extend(questions.questions);
        self.jobs
            .request_input(
                owner_job,
                json!(crate::agent::QuestionOutput {
                    questions: combined.questions.clone(),
                }),
            )
            .await?;
        pending.insert(owner_job, combined);
        Ok(owner_job)
    }

    pub(super) async fn resolve_child_question(
        &self,
        owner_job: JobId,
        resolved_jobs: &[JobId],
    ) -> Result<(), HarnessError> {
        let mut pending = self.pending.lock().await;
        let Some(batch) = pending.get_mut(&owner_job) else {
            return Ok(());
        };
        batch
            .ask_jobs
            .retain(|(_, job)| !resolved_jobs.contains(job));
        batch.answering.retain(|job| !resolved_jobs.contains(job));
        batch
            .questions
            .retain(|question| batch.ask_jobs.iter().any(|(id, _)| id == &question.id));
        if batch.ask_jobs.is_empty() {
            pending.remove(&owner_job);
            self.jobs.resume_input(owner_job).await?;
        } else {
            self.jobs
                .request_input(
                    owner_job,
                    json!(crate::agent::QuestionOutput {
                        questions: batch.questions.clone(),
                    }),
                )
                .await?;
        }
        Ok(())
    }

    pub(super) async fn answer_child_question(
        &self,
        owner_job: JobId,
        value: serde_json::Value,
    ) -> Result<bool, HarnessError> {
        let mut batches = self.pending.lock().await;
        let Some(pending) = batches.get_mut(&owner_job) else {
            return Ok(false);
        };
        // Once questions have been merged, keep recognizing keyed replies as
        // the set shrinks. Raw single answers (including answer/comment objects)
        // remain valid when only one question is left.
        let keyed = pending.merged
            && value.as_object().is_some_and(|values| {
                pending
                    .ask_jobs
                    .iter()
                    .any(|(id, _)| values.contains_key(id))
            });
        let answers = if pending.ask_jobs.len() == 1 && !keyed {
            vec![(pending.ask_jobs[0].1, value)]
        } else {
            let values = value.as_object().ok_or_else(|| {
                HarnessError::Agent(
                    "answers to multiple questions must be keyed by question id".to_owned(),
                )
            })?;
            let answers = pending
                .ask_jobs
                .iter()
                .filter_map(|(id, job)| values.get(id).cloned().map(|value| (*job, value)))
                .collect::<Vec<_>>();
            if answers.is_empty() {
                return Err(HarnessError::Agent(
                    "answer contains no pending question ids".to_owned(),
                ));
            }
            answers
        };
        // Claim each route once, then release the coordinator lock before
        // sending to bounded input channels. Duplicate replies must never fill
        // an ask's queue or prevent its receiver from resolving/cancelling.
        let answers = answers
            .into_iter()
            .filter(|(job, _)| pending.answering.insert(*job))
            .collect::<Vec<_>>();
        drop(batches);
        let mut failure = None;
        for (ask_job, answer) in answers {
            if let Err(error) = self.jobs.send(ask_job, answer).await {
                if let Some(pending) = self.pending.lock().await.get_mut(&owner_job) {
                    pending.answering.remove(&ask_job);
                }
                failure.get_or_insert(error);
            }
        }
        match failure {
            Some(error) => Err(error.into()),
            None => Ok(true),
        }
    }

    pub(super) async fn cancel_child_question(&self, owner_job: JobId) {
        let pending = self.pending.lock().await.remove(&owner_job);
        if let Some(pending) = pending {
            for (_, ask_job) in pending.ask_jobs {
                let _ = self.jobs.cancel(ask_job).await;
            }
        }
    }
}

fn send_question_error(batch: Vec<PendingAsk>, error: String) {
    for pending in batch {
        let _ = pending.result.send(Err(error.clone()));
    }
}

fn duplicate_question_id(questions: &[Question]) -> Option<&str> {
    let mut seen = std::collections::HashSet::new();
    questions
        .iter()
        .map(|question| question.id.as_str())
        .find(|id| !seen.insert(*id))
}

fn split_answers(
    ids: &[String],
    answer: serde_json::Value,
) -> Vec<Result<serde_json::Value, String>> {
    if ids.len() == 1 {
        return vec![Ok(answer)];
    }
    let Some(answers) = answer.as_object() else {
        return ids
            .iter()
            .map(|_| Err("answers to multiple questions must be keyed by question id".to_owned()))
            .collect();
    };
    ids.iter()
        .map(|id| {
            answers
                .get(id)
                .cloned()
                .ok_or_else(|| format!("answer is missing question id `{id}`"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::split_answers;
    use serde_json::json;

    #[test]
    fn split_answers_preserves_single_suggestion_with_comment() {
        let answer =
            json!({"answer": "Use the default", "comment": "  Keep the existing settings.\n"});

        assert_eq!(
            split_answers(&["answer".to_owned()], answer.clone()),
            vec![Ok(answer)]
        );
    }

    #[test]
    fn split_answers_preserves_mixed_batch_answers() {
        let suggestion =
            json!({"answer": "Use the default", "comment": "  Keep the existing settings.\n"});
        let ids = ["choice", "commented_choice", "free_form"].map(str::to_owned);

        assert_eq!(
            split_answers(
                &ids,
                json!({
                    "free_form": "My own answer",
                    "commented_choice": suggestion.clone(),
                    "choice": "Continue"
                })
            ),
            vec![
                Ok(json!("Continue")),
                Ok(suggestion),
                Ok(json!("My own answer"))
            ]
        );
    }
    use crate::agent::runtime::tests::*;
    use crate::{
        job::{JobEnvelope, JobSpec, JobState},
        tool::policy::Capability,
    };

    async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), future)
            .await
            .expect("question test stalled")
    }

    async fn until(
        session: &SessionHandle,
        job: JobId,
        predicate: impl Fn(&JobEnvelope) -> bool,
    ) -> JobEnvelope {
        bounded(async {
            loop {
                let snapshot = session.runtime.jobs.snapshot(job).await.unwrap();
                if predicate(&snapshot) {
                    return snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
    }

    async fn terminal(session: &SessionHandle, job: JobId) -> JobEnvelope {
        until(session, job, |job| job.state.is_terminal()).await;
        session.runtime.jobs.wait(job, None, true).await.unwrap()
    }

    async fn owner(session: &SessionHandle) -> JobId {
        let job = session
            .runtime
            .jobs
            .create(JobSpec {
                accepts_input: true,
                ..JobSpec::test(session.root.clone(), "agent")
            })
            .await
            .unwrap();
        session
            .runtime
            .jobs
            .transition(job.id, JobState::Running)
            .await
            .unwrap();
        job.id
    }

    fn ask(id: &str, index: usize) -> AssistantContent {
        AssistantContent::tool_call(
            format!("tool-{index}"),
            index,
            ToolCall {
                id: id.into(),
                name: "ask".into(),
                arguments: json!({"id":id, "prompt":"Question?"}),
            },
        )
    }

    #[tokio::test]
    async fn concurrent_root_questions_are_merged_and_answers_are_split() {
        struct QuestionsWithMode {
            inner: RecordingQuestions,
            backgrounds: Arc<StdMutex<Vec<bool>>>,
            cancel: bool,
        }

        impl QuestionHandler for QuestionsWithMode {
            fn ask(
                &self,
                agent: AgentId,
                questions: Vec<Question>,
            ) -> crate::agent::QuestionFuture {
                self.inner.ask(agent, questions)
            }

            fn ask_with_background(
                &self,
                agent: AgentId,
                questions: Vec<Question>,
                background: bool,
            ) -> crate::agent::QuestionFuture {
                self.backgrounds.lock().unwrap().push(background);
                if self.cancel {
                    Box::pin(async {
                        Err(crate::agent::QuestionError::Failed(
                            "question cancelled".into(),
                        ))
                    })
                } else {
                    self.ask(agent, questions)
                }
            }
        }

        let backgrounds: Arc<StdMutex<Vec<bool>>> = Arc::default();
        let root = tempfile::tempdir().unwrap();
        let requests: Arc<StdMutex<Vec<ModelRequest>>> = Arc::default();
        let batches: Arc<StdMutex<Vec<Vec<Question>>>> = Arc::default();
        let harness = question_harness(
            root.path(),
            &root.path().join("sessions"),
            scripted_provider(
                &requests,
                [
                    response(vec![ask("first", 0), ask("second", 1)]),
                    answer("done"),
                ],
            ),
            Arc::new(QuestionsWithMode {
                inner: RecordingQuestions {
                    batches: batches.clone(),
                    answer: json!({"first":"yes", "second":{"value":2}, "extra":true}),
                },
                backgrounds: backgrounds.clone(),
                cancel: false,
            }),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        assert_eq!(bounded(session.prompt("ask twice")).await.unwrap(), "done");
        {
            let batches = batches.lock().unwrap();
            assert_eq!(batches.len(), 1);
            assert_eq!(
                batches[0].iter().map(|q| q.id.as_str()).collect::<Vec<_>>(),
                ["first", "second"]
            );
            let requests = requests.lock().unwrap();
            let Message::Tool(results) = request_history(&requests[1]).last().unwrap() else {
                panic!("missing tool results")
            };
            assert_eq!(results[0].result["result"], "yes");
            assert_eq!(results[1].result["result"], json!({"value":2}));
        }
        let events =
            std::fs::read_to_string(session.runtime.store.directory().join("events.jsonl"))
                .unwrap();
        for kind in ["question_opened", "question_resolved"] {
            assert_eq!(events.lines().filter(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["event"]["type"] == kind).count(), 2);
        }
        assert_eq!(*backgrounds.lock().unwrap(), [false]);
        shutdown_session(session).await;

        // A mixed batch, or a foreground batch inside a background script, is
        // irrevocable on dismissal. Neither nesting alone nor an agent's own
        // launch mode makes its foreground tool questions background questions.
        for (script_background, ask_background) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let root = tempfile::tempdir().unwrap();
            let backgrounds: Arc<StdMutex<Vec<bool>>> = Arc::default();
            let background = script_background || ask_background;
            let harness = question_harness(
                root.path(),
                &root.path().join("sessions"),
                Arc::new(HangingProvider),
                Arc::new(QuestionsWithMode {
                    inner: RecordingQuestions {
                        batches: Arc::default(),
                        answer: json!({"first":"yes", "second":"yes"}),
                    },
                    backgrounds: backgrounds.clone(),
                    cancel: background,
                }),
            )
            .await;
            let session = harness.new_session().await.unwrap();
            session
                .runtime
                .questions
                .prepare_question_batch(
                    &session.root,
                    &["first", "second"].map(|id| ToolCall {
                        id: id.into(),
                        name: "ask".into(),
                        arguments: json!({"id":id, "prompt":"Question?"}),
                    }),
                )
                .await;
            let script = bounded(session.runtime.executor.execute_model(
                session.root.clone(),
                "script",
                json!({
                    "source": format!(
                        "async function ask(id, bg) {{ try {{ const answer = await tool.ask({{id,prompt:'Question?',bg}}); return bg ? {{job:answer.id}} : {{answer}}; }} catch (error) {{ return {{error:String(error)}}; }} }} return await Promise.all([ask('first',false), ask('second',{ask_background})]);"
                    ),
                    "bg": script_background,
                }),
                None,
            ))
            .await
            .unwrap();
            let output = terminal(&session, script.job).await.output.unwrap();
            for ask in output["value"].as_array().unwrap() {
                if let Some(job) = ask.get("job") {
                    let result =
                        terminal(&session, serde_json::from_value(job.clone()).unwrap()).await;
                    assert_eq!(result.state, JobState::Failed);
                    assert!(result.error.unwrap().contains("question cancelled"));
                } else if background {
                    assert!(
                        ask["error"]
                            .as_str()
                            .unwrap()
                            .contains("question cancelled")
                    );
                } else {
                    assert_eq!(ask["answer"], json!("yes"));
                }
            }
            assert_eq!(*backgrounds.lock().unwrap(), [background]);
            shutdown_session(session).await;
        }
    }

    #[tokio::test]
    async fn background_child_asks_merge_across_turns_and_resolve_independently() {
        let root = tempfile::tempdir().unwrap();
        let harness = test_harness(
            root.path(),
            &root.path().join("sessions"),
            Arc::new(HangingProvider),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        let owner = owner(&session).await;
        let mut asks = Vec::new();
        for (index, id) in ["first", "second", "second"].into_iter().enumerate() {
            let result = session
                .runtime
                .executor
                .execute_model(
                    session.root.child(1),
                    "ask",
                    json!({"id":id, "prompt":"Question?", "bg":true}),
                    Some(owner),
                )
                .await
                .unwrap();
            assert!(result.background);
            if index == 2 {
                assert_eq!(terminal(&session, result.job).await.state, JobState::Failed);
            } else {
                asks.push(result.job);
                until(&session, owner, |job| {
                    job.state == JobState::WaitingInput
                        && job.output.as_ref().unwrap()["questions"]
                            .as_array()
                            .unwrap()
                            .len()
                            == index + 1
                })
                .await;
            }
        }
        for (index, (id, value)) in [("first", "one"), ("second", "two")]
            .into_iter()
            .enumerate()
        {
            assert!(
                session
                    .runtime
                    .questions
                    .answer_child_question(owner, json!({(id):value}))
                    .await
                    .unwrap()
            );
            assert_eq!(
                terminal(&session, asks[index]).await.output,
                Some(json!(value))
            );
            if index == 0 {
                let remaining = session.runtime.jobs.snapshot(owner).await.unwrap();
                assert_eq!(remaining.state, JobState::WaitingInput);
                assert_eq!(remaining.output.unwrap()["questions"][0]["id"], "second");
            }
        }
        assert_eq!(
            session.runtime.jobs.snapshot(owner).await.unwrap().state,
            JobState::Running
        );
        session.runtime.jobs.cancel(owner).await.unwrap();
        shutdown_session(session).await;
    }

    #[tokio::test]
    async fn stable_agent_job_routes_answers_and_duplicate_bursts_do_not_block_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let harness = test_harness(
            root.path(),
            &root.path().join("sessions"),
            Arc::new(HangingProvider),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        let owner = owner(&session).await;
        let mut asks = Vec::new();
        for _ in 0..2 {
            let ask = session
                .runtime
                .jobs
                .create(JobSpec {
                    parent: Some(owner),
                    accepts_input: true,
                    ..JobSpec::test(session.root.child(1), "ask")
                })
                .await
                .unwrap();
            session
                .runtime
                .jobs
                .transition(ask.id, JobState::Running)
                .await
                .unwrap();
            asks.push(ask);
        }
        let coordinator = &session.runtime.questions;
        assert_eq!(
            coordinator
                .open_child_questions(
                    vec![("first".into(), asks[0].id), ("second".into(), asks[1].id)],
                    json!({"questions":[]})
                )
                .await
                .unwrap(),
            owner
        );
        assert_eq!(
            session.runtime.jobs.snapshot(owner).await.unwrap().state,
            JobState::WaitingInput
        );
        assert!(
            coordinator
                .answer_child_question(owner, json!({"first":"yes", "second":2}))
                .await
                .unwrap()
        );
        bounded(async {
            for _ in 0..100 {
                assert!(
                    coordinator
                        .answer_child_question(
                            owner,
                            json!({"first":"duplicate", "second":"duplicate"})
                        )
                        .await
                        .unwrap()
                );
            }
        })
        .await;
        assert_eq!(asks[0].input.recv().await.unwrap(), json!("yes"));
        assert_eq!(asks[1].input.recv().await.unwrap(), json!(2));
        coordinator
            .resolve_child_question(owner, &[asks[0].id, asks[1].id])
            .await
            .unwrap();
        assert_eq!(
            session.runtime.jobs.snapshot(owner).await.unwrap().state,
            JobState::Running
        );
        coordinator
            .open_child_questions(vec![("cancel".into(), asks[0].id)], json!({"questions":[]}))
            .await
            .unwrap();
        bounded(async {
            for _ in 0..100 {
                coordinator
                    .answer_child_question(owner, json!("cancel me"))
                    .await
                    .unwrap();
            }
            coordinator.cancel_child_question(owner).await;
        })
        .await;
        assert!(asks[0].cancellation.is_cancelled());
        for ask in asks {
            session.runtime.jobs.cancel(ask.id).await.unwrap();
        }
        session.runtime.jobs.cancel(owner).await.unwrap();
        shutdown_session(session).await;
    }

    #[tokio::test]
    async fn noninteractive_children_receive_parent_answers_directly_and_through_scripts() {
        for scripted in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let requests: Arc<StdMutex<Vec<ModelRequest>>> = Arc::default();
            let batches: Arc<StdMutex<Vec<Vec<Question>>>> = Arc::default();
            let call = if scripted {
                AssistantContent::tool_call(
                    "script",
                    0,
                    ToolCall {
                        id: "question".into(),
                        name: "script".into(),
                        arguments: json!({"source":"return await tool.ask({id:'child', prompt:'parent question'});"}),
                    },
                )
            } else {
                ask("child", 0)
            };
            let mut capabilities = CapabilitySet::default();
            capabilities.remove(Capability::Interactive);
            let harness = test_builder(
                root.path(),
                &root.path().join("sessions"),
                scripted_provider(&requests, [response(vec![call]), answer("done")]),
            )
            .max_child_depth(1)
            .capabilities(capabilities)
            .question_handler(Arc::new(RecordingQuestions {
                batches: batches.clone(),
                answer: json!("host-answer"),
            }))
            .build()
            .await
            .unwrap();
            let session = harness.new_session().await.unwrap();
            let launched =
                bounded(session.run_script(
                    "return await tool.agent({prompt:'ask parent', depth:0, bg:true});",
                ))
                .await
                .unwrap();
            let owner = serde_json::from_value(launched.value["value"]["id"].clone()).unwrap();
            let waiting = until(&session, owner, |job| job.state == JobState::WaitingInput).await;
            assert_eq!(waiting.output.unwrap()["questions"][0]["id"], "child");
            bounded(session.run_script(format!(
                "return await tool.job({owner}).send({{value:'parent-answer'}});"
            )))
            .await
            .unwrap();
            assert_eq!(terminal(&session, owner).await.output, Some(json!("done")));
            {
                let requests = requests.lock().unwrap();
                assert!(
                    requests
                        .iter()
                        .all(|r| r.tools.iter().any(|tool| tool.name == "ask"))
                );
                assert!(
                    serde_json::to_string(&requests[1].messages)
                        .unwrap()
                        .contains("parent-answer")
                );
            }
            assert!(
                batches.lock().unwrap().is_empty(),
                "child asks must not reach host handler"
            );
            shutdown_session(session).await;
        }
    }
    #[tokio::test]
    async fn question_coordinator_rechecks_root_gate_before_waiting_for_input() {
        for with_handler in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let batches: Arc<StdMutex<Vec<Vec<Question>>>> = Arc::default();
            let mut capabilities = CapabilitySet::default();
            capabilities.remove(Capability::Interactive);
            let mut builder = test_builder(
                root.path(),
                &root.path().join("sessions"),
                Arc::new(HangingProvider),
            )
            .capabilities(capabilities);
            if with_handler {
                builder = builder.question_handler(Arc::new(RecordingQuestions {
                    batches: batches.clone(),
                    answer: json!("unused"),
                }));
            }
            let harness = builder.build().await.unwrap();
            let session = harness.new_session().await.unwrap();
            for background in [false, true] {
                let lease = session
                    .runtime
                    .jobs
                    .create(crate::job::JobSpec {
                        accepts_input: true,
                        background,
                        ..crate::job::JobSpec::test(session.root.clone(), "ask")
                    })
                    .await
                    .unwrap();
                let location = crate::execution::ExecutionLocation::root(root.path().to_owned());
                let context = crate::tool::ToolContext::new(
                    crate::tool::authorization::AuthorizationSubject {
                        agent: session.root.clone(),
                        job: lease.id,
                        parent: None,
                        scope: None,
                        capabilities: session.runtime.harness.capabilities.clone(),
                        cancellation: lease.cancellation,
                    },
                    location.clone(),
                    location,
                    lease.input,
                    session.runtime.jobs.clone(),
                );
                let error = bounded(session.runtime.questions.coordinate_question(
                    context,
                    Question {
                        id: "bypass".into(),
                        prompt: "question".into(),
                        options: vec![],
                    },
                ))
                .await
                .unwrap_err();
                assert!(matches!(error, crate::tool::ToolError::Denied(_)));
                assert_ne!(
                    session.runtime.jobs.snapshot(lease.id).await.unwrap().state,
                    JobState::WaitingInput
                );
                session.runtime.jobs.cancel(lease.id).await.unwrap();
            }
            assert!(batches.lock().unwrap().is_empty());
            shutdown_session(session).await;
        }
    }
}
