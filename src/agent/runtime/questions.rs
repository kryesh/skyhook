//! Batches questions and routes answers through host handlers or stable agent jobs.

use futures_util::{StreamExt, stream::FuturesUnordered};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
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

// True while claimed, from claim until resolution, so duplicate answers cannot
// fill a bounded input channel. Each (re)opened question gets a new generation.
#[derive(Clone)]
struct AnswerRoute(Arc<AtomicBool>);

impl AnswerRoute {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    fn claim(&self) -> Option<AnswerClaim> {
        self.0
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            .then(|| AnswerClaim(Some(self.clone())))
    }
}

// Owns rollback until live mailbox acceptance; a synchronous Drop releases only
// this generation, even when the caller is cancelled mid-reply.
struct AnswerClaim(Option<AnswerRoute>);

impl AnswerClaim {
    fn delivered(mut self) {
        // Leave the route claimed until resolution, suppressing duplicate sends.
        self.0.take();
    }
}

impl Drop for AnswerClaim {
    fn drop(&mut self) {
        if let Some(route) = &self.0 {
            route.0.store(false, Ordering::Release);
        }
    }
}

#[derive(Clone)]
struct PendingQuestionEntry {
    question: Question,
    job: JobId,
    delivery: AnswerRoute,
}

impl PendingQuestionEntry {
    fn new(question: Question, job: JobId) -> Self {
        Self {
            question,
            job,
            delivery: AnswerRoute::new(),
        }
    }
}

#[derive(Clone)]
struct PendingQuestion {
    // Nonempty while installed; presentation and routing derive from these entries.
    entries: Vec<PendingQuestionEntry>,
    merged: bool,
}

impl PendingQuestion {
    fn new(entries: Vec<PendingQuestionEntry>) -> Result<Self, HarnessError> {
        if entries.is_empty() {
            return Err(HarnessError::Agent("question batch is empty".to_owned()));
        }
        let ids = entries.iter().map(|entry| entry.question.id.as_str());
        if let Some(duplicate) = duplicate_question_id(ids) {
            return Err(HarnessError::Agent(format!(
                "duplicate question id `{duplicate}`"
            )));
        }
        Ok(Self {
            merged: entries.len() > 1,
            entries,
        })
    }

    // Atomic: a rejected merge leaves the installed entries and routes untouched.
    fn merge(&mut self, other: Self) -> Result<(), HarnessError> {
        let entries = self.entries.iter().cloned().chain(other.entries).collect();
        self.entries = Self::new(entries)?.entries;
        self.merged = true;
        Ok(())
    }

    fn output(&self) -> crate::agent::QuestionOutput {
        crate::agent::QuestionOutput {
            questions: self
                .entries
                .iter()
                .map(|entry| entry.question.clone())
                .collect(),
        }
    }
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
            if envelope.role == crate::job::JobRole::Agent {
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
        if context.agent().parent().is_none()
            && !context
                .capabilities()
                .contains(crate::tool::policy::Capability::Interactive)
        {
            return Err(crate::tool::ToolError::Denied(
                "root questions require the interactive capability".to_owned(),
            ));
        }
        let (result, received) = oneshot::channel();
        let agent = context.agent().clone();
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

    pub(super) async fn prepare_question_batch(
        &self,
        agent: &AgentId,
        calls: &[ToolCall],
        registry: &crate::tool::ToolRegistry,
    ) {
        let count = calls
            .iter()
            .filter(|call| {
                registry
                    .get(call.name())
                    .is_some_and(|tool| tool.job_role() == crate::job::JobRole::Question)
                    && call
                        .arguments()
                        .get("bg")
                        .is_none_or(serde_json::Value::is_boolean)
                    && serde_json::from_value::<Question>(serde_json::Value::Object(
                        call.arguments().clone(),
                    ))
                    .is_ok()
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
        if let Some(duplicate) = duplicate_question_id(questions.iter().map(|q| q.id.as_str())) {
            send_question_error(batch, format!("duplicate question id `{duplicate}`"));
            return;
        }
        // Only explicit background asks expose their own input state. Marking a
        // foreground ask WaitingInput would cause its native call to return early.
        for pending in &batch {
            if self
                .jobs
                .is_background(pending.context.job())
                .await
                .unwrap_or(false)
                && let Err(error) = self
                    .jobs
                    .request_input(
                        pending.context.job(),
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
            self.present_child_questions(batch).await;
        } else {
            self.present_root_questions(agent, batch, questions).await;
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
            // A foreground ask inside a background script does not block the agent.
            match self.jobs.is_effectively_background(ask.context.job()).await {
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
                    .ask(agent, questions, background)
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
                        .is_background(ask.context.job())
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

    async fn present_child_questions(&self, batch: Vec<PendingAsk>) {
        let entries = batch
            .iter()
            .map(|pending| {
                PendingQuestionEntry::new(pending.question.clone(), pending.context.job())
            })
            .collect();
        let questions = match PendingQuestion::new(entries) {
            Ok(questions) => questions,
            Err(error) => {
                send_question_error(batch, error.to_string());
                return;
            }
        };
        let owner_job = match self.open_child_questions(questions).await {
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
                    .resolve_child_question(owner_job, &[pending.context.job()])
                    .await;
                let result = resolved
                    .map_err(|error| error.to_string())
                    .and_then(|()| answer.map_err(|error| error.to_string()));
                let _ = pending.result.send(result);
            })
            .collect::<FuturesUnordered<_>>();
        while answers.next().await.is_some() {}
    }

    async fn open_child_questions(
        &self,
        questions: PendingQuestion,
    ) -> Result<JobId, HarnessError> {
        let owner_job = self.owning_agent_job(questions.entries[0].job).await?;
        let mut pending = self.pending.lock().await;
        let combined = if let Some(existing) = pending.get(&owner_job) {
            let mut combined = existing.clone();
            combined.merge(questions)?;
            combined
        } else {
            questions
        };
        self.jobs
            .request_input(owner_job, json!(combined.output()))
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
        // Retire resolved questions before the fallible job update, so a failure
        // never routes later answers to finished ask jobs.
        batch
            .entries
            .retain(|entry| !resolved_jobs.contains(&entry.job));
        if batch.entries.is_empty() {
            pending.remove(&owner_job);
            self.jobs.resume_input(owner_job).await?;
        } else {
            let output = json!(batch.output());
            self.jobs.request_input(owner_job, output).await?;
        }
        Ok(())
    }

    pub(super) async fn answer_child_question(
        &self,
        owner_job: JobId,
        value: serde_json::Value,
    ) -> Result<bool, HarnessError> {
        let batches = self.pending.lock().await;
        let Some(pending) = batches.get(&owner_job) else {
            return Ok(false);
        };
        // Once questions have been merged, keep recognizing keyed replies as
        // the set shrinks. Raw single answers (including answer/comment objects)
        // remain valid when only one question is left.
        let keyed = pending.merged
            && value.as_object().is_some_and(|values| {
                pending
                    .entries
                    .iter()
                    .any(|entry| values.contains_key(&entry.question.id))
            });
        let answers = if pending.entries.len() == 1 && !keyed {
            vec![(&pending.entries[0], value)]
        } else {
            let values = value.as_object().ok_or_else(|| {
                HarnessError::Agent(
                    "answers to multiple questions must be keyed by question id".to_owned(),
                )
            })?;
            let answers = pending
                .entries
                .iter()
                .filter_map(|entry| {
                    values
                        .get(&entry.question.id)
                        .cloned()
                        .map(|value| (entry, value))
                })
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
            .filter_map(|(entry, answer)| {
                entry
                    .delivery
                    .claim()
                    .map(|claim| (entry.job, answer, claim))
            })
            .collect::<Vec<_>>();
        drop(batches);
        let mut failure = None;
        for (ask_job, answer, claim) in answers {
            // Live ask mailboxes accept via try_send and return Ok in the same
            // poll; there must be no await between acceptance and disarming
            // rollback. These routes are not retained-agent resumption receipts.
            match self.jobs.send(ask_job, answer).await {
                Ok(()) => claim.delivered(),
                Err(error) => {
                    failure.get_or_insert(error);
                }
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
            for entry in pending.entries {
                let _ = self.jobs.cancel(entry.job).await;
            }
        }
    }
}

fn send_question_error(batch: Vec<PendingAsk>, error: String) {
    for pending in batch {
        let _ = pending.result.send(Err(error.clone()));
    }
}

fn duplicate_question_id<'a>(mut ids: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let mut seen = HashSet::new();
    ids.find(|id| !seen.insert(*id))
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
    use crate::agent::runtime::tests::*;
    use crate::{
        job::{JobEnvelope, JobSpec, JobState},
        tool::{ToolContext, ToolError, policy::Capability},
    };

    type Batches = Arc<StdMutex<Vec<Vec<Question>>>>;
    type Backgrounds = Arc<StdMutex<Vec<bool>>>;

    #[test]
    fn split_answers_preserves_single_and_mixed_batch_answers() {
        let suggestion =
            json!({"answer": "Use the default", "comment": "  Keep the existing settings.\n"});
        let single = split_answers(&["answer".to_owned()], suggestion.clone());
        assert_eq!(single, vec![Ok(suggestion.clone())]);
        let ids = ["choice", "commented_choice", "free_form"].map(str::to_owned);
        let answers = json!({"free_form": "My own answer", "commented_choice": suggestion, "choice": "Continue"});
        let (first, last) = (json!("Continue"), json!("My own answer"));
        let found = split_answers(&ids, answers);
        assert_eq!(found, vec![Ok(first), Ok(suggestion), Ok(last)]);
    }

    fn pending_questions(entries: &[(&str, JobId)]) -> super::PendingQuestion {
        let question = |id: &str| crate::agent::Question {
            id: id.to_owned(),
            prompt: "Question?".to_owned(),
            options: Vec::new(),
        };
        let entries = entries.iter();
        let entries = entries.map(|(id, job)| super::PendingQuestionEntry::new(question(id), *job));
        super::PendingQuestion::new(entries.collect()).unwrap()
    }

    fn ask(id: &str, index: usize) -> AssistantContent {
        tool_call(index, id, "ask", json!({"id":id, "prompt":"Question?"}))
    }

    async fn hanging_session(root: &tempfile::TempDir) -> SessionHandle {
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, Arc::new(HangingProvider)).await;
        harness.new_session().await.unwrap()
    }

    async fn mode_session(
        root: &Path,
        provider: Arc<dyn Provider>,
        answer: serde_json::Value,
        cancel: bool,
    ) -> (SessionHandle, Batches, Backgrounds) {
        let (batches, backgrounds) = (Batches::default(), Backgrounds::default());
        let handler = RecordingQuestions {
            batches: batches.clone(),
            backgrounds: backgrounds.clone(),
            answer,
            cancel,
        };
        let harness = test_builder(root, &root.join("sessions"), provider, false)
            .question_handler(Arc::new(handler))
            .build()
            .await
            .unwrap();
        (harness.new_session().await.unwrap(), batches, backgrounds)
    }

    #[tokio::test]
    async fn concurrent_root_questions_are_merged_and_answers_are_split() {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let asks = response(vec![ask("first", 0), ask("second", 1)]);
        let provider = scripted_provider(&requests, [asks, answer("done")]);
        let answers = json!({"first":"yes", "second":{"value":2}, "extra":true});
        let (session, batches, backgrounds) =
            mode_session(root.path(), provider, answers, false).await;
        assert_eq!(bounded(session.prompt("ask twice")).await.unwrap(), "done");
        {
            let batches = batches.lock().unwrap();
            assert_eq!(batches.len(), 1);
            // Concurrent asks join the batch in arrival order.
            let mut ids = batches[0].iter().map(|q| q.id.as_str()).collect::<Vec<_>>();
            ids.sort_unstable();
            assert_eq!(ids, ["first", "second"]);
            let requests = requests.lock().unwrap();
            let Message::Tool(results) = requests[1].history.last().unwrap() else {
                panic!("missing tool results")
            };
            assert_eq!(results[0].result["result"], "yes");
            assert_eq!(results[1].result["result"], json!({"value":2}));
        }
        assert_eq!(*backgrounds.lock().unwrap(), [false]);
        shutdown_session(session).await;

        // A mixed batch, or a foreground batch inside a background script, is
        // irrevocable on dismissal; neither nesting nor launch mode alone is.
        for (script_background, ask_background) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let root = tempfile::tempdir().unwrap();
            let background = script_background || ask_background;
            let answers = json!({"first":"yes", "second":"yes"});
            let provider = Arc::new(HangingProvider);
            let (session, _, backgrounds) =
                mode_session(root.path(), provider, answers, background).await;
            let call = |id| ToolCall::new(id, "ask", json!({"id":id, "prompt":"Question?"}));
            let calls = ["first", "second"].map(|id| call(id).unwrap());
            let (runtime, agent) = (&session.runtime, session.root.clone());
            let registry = runtime.executor.registry();
            let prepared = runtime
                .questions
                .prepare_question_batch(&agent, &calls, registry);
            prepared.await;
            let source = format!(
                "async function ask(id, bg) {{ try {{ const answer = await tool.ask({{id,prompt:'Question?',bg}}); return bg ? {{job:answer.id}} : {{answer}}; }} catch (error) {{ return {{error:String(error)}}; }} }} return await Promise.all([ask('first',false), ask('second',{ask_background})]);"
            );
            let arguments = json!({"source": source, "bg": script_background});
            let script = runtime
                .executor
                .execute_model(agent, "script", arguments, None);
            let script = bounded(script).await.unwrap();
            let output = terminal(&session, script.job).await.output.unwrap();
            for ask in output["value"].as_array().unwrap() {
                if let Some(job) = ask.get("job") {
                    let job = serde_json::from_value(job.clone()).unwrap();
                    let result = terminal(&session, job).await;
                    assert_eq!(result.state, JobState::Failed);
                    assert!(result.error.unwrap().contains("question cancelled"));
                } else if background {
                    let error = ask["error"].as_str().unwrap();
                    assert!(error.contains("question cancelled"));
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
        let session = hanging_session(&root).await;
        let owner = owner(&session).await;
        start_child(&session, 1, Some(owner)).await;
        let (executor, jobs) = (&session.runtime.executor, &session.runtime.jobs);
        let waiting_with = |count: usize| {
            move |job: &JobEnvelope| {
                job.state == JobState::WaitingInput
                    && job.output.as_ref().unwrap()["questions"]
                        .as_array()
                        .unwrap()
                        .len()
                        == count
            }
        };
        let mut asks = Vec::new();
        for (index, id) in ["first", "second", "second"].into_iter().enumerate() {
            let arguments = json!({"id":id, "prompt":"Question?", "bg":true});
            let child = session.root.child(1);
            let result = executor.execute_model(child, "ask", arguments, Some(owner));
            let result = result.await.unwrap();
            assert!(result.background);
            if index == 2 {
                assert_eq!(terminal(&session, result.job).await.state, JobState::Failed);
                continue;
            }
            asks.push(result.job);
            until(&session, owner, waiting_with(index + 1)).await;
        }
        let questions = &session.runtime.questions;
        let answers = [("first", "one"), ("second", "two")];
        for (index, (id, value)) in answers.into_iter().enumerate() {
            let answered = questions.answer_child_question(owner, json!({(id):value}));
            assert!(answered.await.unwrap());
            let output = terminal(&session, asks[index]).await.output;
            assert_eq!(output, Some(json!(value)));
            if index == 0 {
                let remaining = jobs.snapshot(owner).await.unwrap();
                assert_eq!(remaining.state, JobState::WaitingInput);
                assert_eq!(remaining.output.unwrap()["questions"][0]["id"], "second");
            }
        }
        assert_eq!(jobs.snapshot(owner).await.unwrap().state, JobState::Running);
        jobs.cancel(owner).await.unwrap();
        shutdown_session(session).await;
    }

    #[tokio::test]
    async fn stable_agent_job_routes_answers_and_duplicate_bursts_do_not_block_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let session = hanging_session(&root).await;
        let owner = owner(&session).await;
        start_child(&session, 1, Some(owner)).await;
        let jobs = &session.runtime.jobs;
        let mut asks = Vec::new();
        for _ in 0..2 {
            let spec = JobSpec {
                parent: Some(owner),
                accepts_input: true,
                ..JobSpec::test(session.root.child(1), "ask")
            };
            let ask = jobs.create(spec).await.unwrap().into_test_fixture();
            jobs.transition(ask.id(), JobState::Running).await.unwrap();
            asks.push(ask);
        }
        let coordinator = &session.runtime.questions;
        let pending = pending_questions(&[("first", asks[0].id()), ("second", asks[1].id())]);
        let opened = coordinator.open_child_questions(pending).await.unwrap();
        assert_eq!(opened, owner);
        let state = jobs.snapshot(owner).await.unwrap().state;
        assert_eq!(state, JobState::WaitingInput);
        let answered = coordinator.answer_child_question(owner, json!({"first":"yes", "second":2}));
        assert!(answered.await.unwrap());
        // More than a job's input mailbox (32) holds.
        bounded(async {
            for _ in 0..40 {
                let duplicate = json!({"first":"duplicate", "second":"duplicate"});
                let duplicate = coordinator.answer_child_question(owner, duplicate);
                assert!(duplicate.await.unwrap());
            }
        })
        .await;
        let (mut first_input, mut second_input) = (asks[0].take_input(), asks[1].take_input());
        assert_eq!(first_input.recv().await.unwrap(), json!("yes"));
        assert_eq!(second_input.recv().await.unwrap(), json!(2));
        let resolved = [asks[0].id(), asks[1].id()];
        let resolve = coordinator.resolve_child_question(owner, &resolved);
        resolve.await.unwrap();
        assert_eq!(jobs.snapshot(owner).await.unwrap().state, JobState::Running);
        let pending = pending_questions(&[("cancel", asks[0].id())]);
        coordinator.open_child_questions(pending).await.unwrap();
        bounded(async {
            for _ in 0..40 {
                let answer = coordinator.answer_child_question(owner, json!("cancel me"));
                answer.await.unwrap();
            }
            coordinator.cancel_child_question(owner).await;
        })
        .await;
        assert!(asks[0].cancellation_token().is_cancelled());
        for ask in asks {
            jobs.cancel(ask.id()).await.unwrap();
        }
        jobs.cancel(owner).await.unwrap();
        shutdown_session(session).await;
    }

    async fn park_answer(
        answer: &mut std::pin::Pin<
            Box<impl std::future::Future<Output = Result<bool, HarnessError>>>,
        >,
        route: &super::AnswerRoute,
    ) {
        bounded(async {
            loop {
                assert!(futures_util::poll!(answer.as_mut()).is_pending());
                if route.0.load(Ordering::Acquire) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
    }

    #[tokio::test]
    async fn cancelled_answer_under_backpressure_releases_claim_without_cleanup_task() {
        let root = tempfile::tempdir().unwrap();
        let session = hanging_session(&root).await;
        let owner = owner(&session).await;
        start_child(&session, 1, Some(owner)).await;
        let jobs = &session.runtime.jobs;
        // A live ask job with a full input mailbox and no retained-agent resume handler.
        let spec = JobSpec {
            parent: Some(owner),
            accepts_input: true,
            role: crate::job::JobRole::Question,
            ..JobSpec::test(session.root.child(1), "ask")
        };
        let mut ask = jobs.create(spec).await.unwrap().into_test_fixture();
        let job = ask.id();
        jobs.transition(job, JobState::Running).await.unwrap();
        let mut input = ask.take_input();
        for _ in 0..input.max_capacity() {
            jobs.send(job, json!("backlog")).await.unwrap();
        }
        let coordinator = &session.runtime.questions;
        let pending = || pending_questions(&[("one", job)]);
        let route = async || {
            let pending = coordinator.pending.lock().await;
            pending[&owner].entries[0].delivery.clone()
        };
        let answer = |value| Box::pin(coordinator.answer_child_question(owner, json!(value)));
        coordinator.open_child_questions(pending()).await.unwrap();
        let old_route = route().await;
        let mut abandoned = answer("abandoned");
        park_answer(&mut abandoned, &old_route).await;
        // Reused IDs test route generation: a stale claim must not release the new route.
        let job_list = [job];
        bounded(coordinator.resolve_child_question(owner, &job_list))
            .await
            .unwrap();
        coordinator.open_child_questions(pending()).await.unwrap();
        let new_route = route().await;
        assert!(!Arc::ptr_eq(&old_route.0, &new_route.0));
        let mut new_answer = answer("new");
        park_answer(&mut new_answer, &new_route).await;
        drop(abandoned);
        assert!(!old_route.0.load(Ordering::Acquire));
        assert!(new_route.0.load(Ordering::Acquire));
        assert!(bounded(answer("suppressed")).await.unwrap());
        // Dropping a pending caller must not strand Sending without further polling.
        drop(new_answer);
        assert!(!new_route.0.load(Ordering::Acquire));
        assert_eq!(input.len(), input.max_capacity());
        for _ in 0..input.max_capacity() {
            assert_eq!(input.try_recv().unwrap(), json!("backlog"));
        }
        assert!(bounded(answer("retry")).await.unwrap());
        assert_eq!(input.try_recv().unwrap(), json!("retry"));
        assert!(bounded(answer("duplicate")).await.unwrap());
        assert!(input.try_recv().is_err());
        coordinator
            .resolve_child_question(owner, &job_list)
            .await
            .unwrap();
        jobs.cancel(job).await.unwrap();
        jobs.cancel(owner).await.unwrap();
        shutdown_session(session).await;
    }

    #[tokio::test]
    async fn noninteractive_children_receive_parent_answers_directly_and_through_scripts() {
        for scripted in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let requests = Requests::default();
            let batches = Batches::default();
            let call = if scripted {
                let source = "return await tool.ask({id:'child', prompt:'parent question'});";
                tool_call(0, "question", "script", json!({ "source": source }))
            } else {
                ask("child", 0)
            };
            let mut capabilities = CapabilitySet::default();
            capabilities.remove(Capability::Interactive);
            let provider = scripted_provider(&requests, [response(vec![call]), answer("done")]);
            let handler = RecordingQuestions {
                batches: batches.clone(),
                answer: json!("host-answer"),
                ..Default::default()
            };
            let harness = test_builder(root.path(), &root.path().join("sessions"), provider, false)
                .max_child_depth(1)
                .capabilities(capabilities)
                .question_handler(Arc::new(handler))
                .build()
                .await
                .unwrap();
            let session = harness.new_session().await.unwrap();
            // The idle root is told about its background child's question and would
            // spend a scripted response on it; this test answers through scripts.
            let root_inbox = quiet_root(&session);
            let launch = "return await tool.agent({prompt:'ask parent', depth:0, bg:true});";
            let launched = bounded(session.run_script(launch)).await.unwrap();
            let owner = serde_json::from_value(launched.value["value"]["id"].clone()).unwrap();
            let waiting = until(&session, owner, |job| job.state == JobState::WaitingInput).await;
            assert_eq!(waiting.output.unwrap()["questions"][0]["id"], "child");
            let send = format!("return await tool.job({owner}).send({{value:'parent-answer'}});");
            bounded(session.run_script(send)).await.unwrap();
            assert_eq!(terminal(&session, owner).await.output, Some(json!("done")));
            {
                let requests = requests.lock().unwrap();
                let offers_ask = |r: &ModelRequest| r.tools.iter().any(|tool| tool.name == "ask");
                assert!(requests.iter().all(offers_ask));
                let messages = rendered(&requests[1]);
                assert!(messages.contains("parent-answer"));
            }
            // Child asks must not reach the host handler.
            assert!(batches.lock().unwrap().is_empty());
            drop(root_inbox);
            shutdown_session(session).await;
        }
    }

    #[tokio::test]
    async fn question_coordinator_rechecks_root_gate_before_waiting_for_input() {
        for with_handler in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let batches = Batches::default();
            let mut capabilities = CapabilitySet::default();
            capabilities.remove(Capability::Interactive);
            let (provider, sessions) = (Arc::new(HangingProvider), root.path().join("sessions"));
            let mut builder =
                test_builder(root.path(), &sessions, provider, false).capabilities(capabilities);
            if with_handler {
                let batches = batches.clone();
                let handler = RecordingQuestions {
                    batches,
                    ..Default::default()
                };
                builder = builder.question_handler(Arc::new(handler));
            }
            let session = builder.build().await.unwrap().new_session().await.unwrap();
            let (jobs, questions) = (&session.runtime.jobs, &session.runtime.questions);
            for background in [false, true] {
                let spec = JobSpec {
                    accepts_input: true,
                    background,
                    ..JobSpec::test(session.root.clone(), "ask")
                };
                let mut lease = jobs.create(spec).await.unwrap().into_test_fixture();
                let here = crate::execution::ExecutionLocation::root(root.path().to_owned());
                let subject = crate::tool::authorization::AuthorizationSubject {
                    agent: session.root.clone(),
                    job: lease.id(),
                    parent: None,
                    scope: None,
                    capabilities: session.runtime.capabilities.clone(),
                    cancellation: lease.cancellation_token(),
                };
                let input = lease.take_input();
                let context = ToolContext::new(subject, here.clone(), here, input, jobs.clone());
                let question = Question {
                    id: "bypass".into(),
                    prompt: "question".into(),
                    options: vec![],
                };
                let asked = questions.coordinate_question(context, question);
                let error = bounded(asked).await.unwrap_err();
                assert!(matches!(error, ToolError::Denied(_)));
                let state = jobs.snapshot(lease.id()).await.unwrap().state;
                assert_ne!(state, JobState::WaitingInput);
                jobs.cancel(lease.id()).await.unwrap();
            }
            assert!(batches.lock().unwrap().is_empty());
            shutdown_session(session).await;
        }
    }
}
