//! Batches questions and routes answers through host handlers or stable agent jobs.

use futures_util::{StreamExt, stream::FuturesUnordered};
use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    sync::{
        Arc, Mutex as StdMutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Mutex, oneshot};

use crate::{
    agent::{HarnessError, Question, QuestionError, QuestionHandler, QuestionReply},
    identity::{AgentId, JobId},
    job::JobManager,
    tool::{
        ToolError, ToolOutput,
        diagnostic::{Effects, Operation, Subject},
    },
};

pub(super) struct QuestionCoordinator {
    jobs: JobManager,
    handler: Option<Arc<dyn QuestionHandler>>,
    pending: Mutex<HashMap<JobId, PendingQuestion>>,
    batches: Mutex<HashMap<BatchKey, QuestionBatch>>,
    joined: StdMutex<Joined>,
}

/// The asks one issuer made together: a model response's calls, or a script's.
type BatchKey = (AgentId, Option<JobId>);

/// Each issuer's newest ask to join a batch, and how many of its joined asks are
/// still waiting. Job ids ascend, so no later batch waits for an ask at or below
/// it; the entry goes once no joined ask waits.
type Joined = HashMap<BatchKey, (JobId, usize)>;

/// An ask's place in its issuer's `Joined` entry, released however the ask ends,
/// cancellation included.
struct JoinedAsk<'a> {
    joined: &'a StdMutex<Joined>,
    key: BatchKey,
}

impl<'a> JoinedAsk<'a> {
    /// Join `job` to its issuer's entry; also returns the issuer's previous newest ask.
    fn join(joined: &'a StdMutex<Joined>, key: BatchKey, job: JobId) -> (Self, Option<JobId>) {
        let mut entries = joined.lock().unwrap_or_else(PoisonError::into_inner);
        let newest = entries.get(&key).map(|(newest, _)| *newest);
        let entry = entries.entry(key.clone()).or_insert((job, 0));
        *entry = (entry.0.max(job), entry.1 + 1);
        (Self { joined, key }, newest)
    }
}

impl Drop for JoinedAsk<'_> {
    fn drop(&mut self) {
        let mut entries = self.joined.lock().unwrap_or_else(PoisonError::into_inner);
        if let Entry::Occupied(mut joined) = entries.entry(self.key.clone()) {
            joined.get_mut().1 -= 1;
            if joined.get().1 == 0 {
                joined.remove();
            }
        }
    }
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
    /// `entries` is never empty: every batch holds the ask that opened it.
    fn new(entries: Vec<PendingQuestionEntry>) -> Result<Self, HarnessError> {
        let ids = entries.iter().map(|entry| entry.question.id.as_str());
        if let Some(duplicate) = duplicate_question_id(ids) {
            return Err(HarnessError::DuplicateQuestion(duplicate.to_owned()));
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

    /// The batch less the questions `jobs` asked; None once every one is answered.
    fn without(mut self, jobs: &[JobId]) -> Option<Self> {
        self.entries.retain(|entry| !jobs.contains(&entry.job));
        (!self.entries.is_empty()).then_some(self)
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
    result: oneshot::Sender<Result<serde_json::Value, ToolError>>,
}

impl PendingAsk {
    fn finish(self, result: Result<serde_json::Value, ToolError>, operation: Operation) {
        let subject = question_subject(&self.question);
        let result = result.map_err(|error| error.operation(operation, subject));
        let _ = self.result.send(result);
    }
}

#[derive(Default)]
struct QuestionBatch {
    /// The issuer's other asks that have neither joined nor ended; the batch is
    /// presented once none remain.
    awaited: HashSet<JobId>,
    pending: Vec<PendingAsk>,
}

impl QuestionCoordinator {
    pub(super) fn new(jobs: JobManager, handler: Option<Arc<dyn QuestionHandler>>) -> Self {
        Self {
            jobs,
            handler,
            pending: Mutex::new(HashMap::new()),
            batches: Mutex::default(),
            joined: StdMutex::default(),
        }
    }

    async fn owning_agent_job(&self, mut job: JobId) -> Result<JobId, HarnessError> {
        loop {
            let envelope = self.jobs.snapshot(job).await?;
            if envelope.role == crate::job::JobRole::Agent {
                return Ok(job);
            }
            job = envelope.parent.ok_or(HarnessError::UnownedQuestion)?;
        }
    }

    pub(super) async fn coordinate_question(
        self: &Arc<Self>,
        context: crate::tool::ToolContext,
        question: Question,
    ) -> Result<serde_json::Value, crate::tool::ToolError> {
        let subject = question_subject(&question);
        // Enforce before batching or waiting on a job input channel: omitting the
        // host handler alone would leave background root questions waiting forever.
        if context.agent().parent().is_none()
            && !context
                .capabilities()
                .contains(crate::tool::policy::Capability::Interactive)
        {
            return Err(crate::tool::ToolError::denied(
                "root questions require the interactive capability",
            )
            .operation(Operation::Authorize, subject)
            .effects(Effects::NotStarted));
        }
        let (result, received) = oneshot::channel();
        let (agent, job) = (context.agent().clone(), context.job());
        let key = (agent.clone(), context.job_subject().parent);
        if let Some(parent) = key.1 {
            self.jobs.issued(parent).await;
        }
        let mut open = self.batches.lock().await;
        let (_joined, newest) = JoinedAsk::join(&self.joined, key.clone(), job);
        let awaited: Vec<JobId> = if open.contains_key(&key) {
            Vec::new()
        } else {
            // A response's calls are all created before any runs, and a script's
            // issued calls are published above: a new batch waits for the issuer's
            // other live asks.
            let jobs = self.jobs.list(&agent).await.into_iter();
            jobs.filter(|sibling| {
                sibling.parent == key.1
                    && sibling.role == crate::job::JobRole::Question
                    && !sibling.state.is_terminal()
                    && sibling.id != job
                    && newest < Some(sibling.id)
            })
            .map(|sibling| sibling.id)
            .collect()
        };
        let batch = open.entry(key.clone()).or_default();
        batch.awaited.extend(&awaited);
        batch.awaited.remove(&job);
        batch.pending.push(PendingAsk {
            context,
            question,
            result,
        });
        self.present_ready(&mut open, &key);
        drop(open);
        for sibling in awaited {
            let (runtime, key) = (self.clone(), key.clone());
            tokio::spawn(async move {
                let _ = runtime.jobs.wait_settled(sibling).await;
                let mut open = runtime.batches.lock().await;
                if open
                    .get_mut(&key)
                    .is_some_and(|batch| batch.awaited.remove(&sibling))
                {
                    runtime.present_ready(&mut open, &key);
                }
            });
        }
        received.await.unwrap_or_else(|_| {
            Err(ToolError::failed("question batch stopped").operation(Operation::Receive, subject))
        })
    }

    /// Present the batch at `key` once no ask it waits for remains.
    fn present_ready(
        self: &Arc<Self>,
        open: &mut HashMap<BatchKey, QuestionBatch>,
        key: &BatchKey,
    ) {
        if open.get(key).is_some_and(|batch| batch.awaited.is_empty()) {
            let mut batch = open.remove(key).expect("checked above").pending;
            // An ask dropped after joining has no one left to answer.
            batch.retain(|ask| !ask.result.is_closed());
            if batch.is_empty() {
                return;
            }
            let (runtime, agent) = (self.clone(), key.0.clone());
            tokio::spawn(async move { runtime.present_question_batch(agent, batch).await });
        }
    }

    async fn present_question_batch(&self, agent: AgentId, batch: Vec<PendingAsk>) {
        let entries = batch
            .iter()
            .map(|pending| {
                PendingQuestionEntry::new(pending.question.clone(), pending.context.job())
            })
            .collect();
        let questions = match PendingQuestion::new(entries) {
            Ok(questions) => questions,
            Err(error) => {
                let error = super::tools::harness_error(error).effects(Effects::NotStarted);
                send_question_error(batch, error);
                return;
            }
        };
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
                        crate::agent::QuestionOutput {
                            questions: vec![pending.question.clone()],
                        },
                    )
                    .await
            {
                send_question_error(batch, super::tools::harness_error(error.into()));
                return;
            }
        }
        if agent.parent().is_some() {
            self.present_child_questions(batch, questions).await;
        } else {
            let questions = questions.output().questions;
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
                    send_question_error(batch, super::tools::harness_error(error.into()));
                    return;
                }
            }
        }
        let mut answers = batch
            .iter()
            .enumerate()
            .map(|(index, pending)| {
                let context = pending.context.clone();
                async move { (index, context.receive().await) }
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
                    item.take().unwrap().finish(
                        Err(ToolError::failed(QuestionError::Unavailable.to_string())
                            .effects(Effects::NotStarted)),
                        Operation::Prepare,
                    );
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
                            ask.finish(result.map_err(ToolError::failed), Operation::Receive);
                        }
                    }
                }
                Some((index, result)) = answers.next() => {
                    if let Some(ask) = pending[index].take() {
                        ask.finish(result, Operation::Receive);
                    }
                }
            }
        }
    }

    async fn present_child_questions(&self, batch: Vec<PendingAsk>, questions: PendingQuestion) {
        let owner_job = match self.open_child_questions(questions).await {
            Ok(owner_job) => owner_job,
            Err(error) => {
                send_question_error(batch, super::tools::harness_error(error));
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
                let result = finish_child_answer(&pending.question, answer, resolved);
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
            .request_input(owner_job, combined.output())
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
        let Some(batch) = pending.remove(&owner_job) else {
            return Ok(());
        };
        // Retire resolved questions before the fallible job update, so a failure
        // never routes later answers to finished ask jobs.
        match batch.without(resolved_jobs) {
            Some(remaining) => {
                let output = remaining.output();
                pending.insert(owner_job, remaining);
                self.jobs.request_input(owner_job, output).await?;
            }
            None => self.jobs.resume_input(owner_job).await?,
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
            let values = value.as_object().ok_or(HarnessError::UnkeyedAnswers)?;
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
                return Err(HarnessError::UnmatchedAnswers);
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

// A received answer survives a later presentation/persistence failure. Conversely,
// cleanup must not turn a cancellation or interruption into an ordinary failure.
fn finish_child_answer(
    question: &Question,
    answer: Result<serde_json::Value, ToolError>,
    resolved: Result<(), HarnessError>,
) -> Result<serde_json::Value, ToolError> {
    let subject = question_subject(question);
    let answer = answer.map_err(|error| error.operation(Operation::Receive, subject.clone()))?;
    resolved.map_err(|error| {
        super::tools::harness_error(error)
            .operation(Operation::Save, subject)
            .effects(Effects::Unknown)
            .with_result(ToolOutput::new(answer.clone()))
    })?;
    Ok(answer)
}

fn question_subject(question: &Question) -> Subject {
    Subject::Label(format!("question {:?}", question.id))
}

fn send_question_error(batch: Vec<PendingAsk>, error: ToolError) {
    let (diagnostic, _) = error.into_facts();
    for pending in batch {
        pending.finish(
            Err(ToolError::from_facts(diagnostic.clone(), None)),
            Operation::Prepare,
        );
    }
}

fn duplicate_question_id<'a>(mut ids: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let mut seen = HashSet::new();
    ids.find(|id| !seen.insert(*id))
}

/// Each question's answer from a host reply, as the value its ask returns.
fn split_answers(
    ids: &[String],
    mut reply: QuestionReply,
) -> Vec<Result<serde_json::Value, String>> {
    ids.iter()
        .map(|id| {
            let answer = reply
                .remove(id)
                .ok_or_else(|| format!("answer is missing question id `{id}`"))?;
            Ok(serde_json::to_value(answer).expect("answers serialize as JSON strings"))
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
    use serde_json::json;

    type Batches = Arc<StdMutex<Vec<Vec<Question>>>>;
    type Backgrounds = Arc<StdMutex<Vec<bool>>>;

    #[test]
    fn split_answers_keeps_each_answers_wire_shape_and_reports_missing_ids() {
        use crate::agent::QuestionAnswer;
        let commented = QuestionAnswer::Commented {
            answer: "Use the default".into(),
            comment: "  Keep the existing settings.\n".into(),
        };
        let reply = [
            ("choice", QuestionAnswer::Text("Continue".into())),
            ("commented", commented),
        ];
        let reply = reply.map(|(id, answer)| (id.to_owned(), answer)).into();
        let ids = ["choice", "commented", "missing"].map(str::to_owned);
        let found = split_answers(&ids, reply);
        let suggestion =
            json!({"answer": "Use the default", "comment": "  Keep the existing settings.\n"});
        assert_eq!(found[..2], [Ok(json!("Continue")), Ok(suggestion)]);
        assert!(found[2].as_ref().unwrap_err().contains("missing"));
    }

    #[test]
    fn question_resolution_preserves_received_answers_and_control_failures() {
        use crate::tool::diagnostic::{Operation, Subject};

        let question = question("stable-id");
        let failure = || HarnessError::AgentStopped;
        let answer = json!({"answer":"received"});
        let error =
            super::finish_child_answer(&question, Ok(answer.clone()), Err(failure())).unwrap_err();
        let (diagnostic, output) = error.into_parts();
        assert_eq!(output.unwrap().value, answer);
        assert_eq!(diagnostic.context.operation, Operation::Save);
        assert_eq!(
            diagnostic.context.subject,
            Subject::Label("question \"stable-id\"".into())
        );

        for cause in [ToolError::cancelled(), ToolError::interrupted()] {
            let diagnostic = cause.diagnostic();
            let error =
                super::finish_child_answer(&question, Err(cause), Err(failure())).unwrap_err();
            assert_eq!(error.diagnostic().cause, diagnostic.cause);
            assert_eq!(error.diagnostic().context.operation, Operation::Receive);
        }
    }

    fn question(id: &str) -> Question {
        Question {
            id: id.to_owned(),
            prompt: "Question?".to_owned(),
            options: Vec::new(),
        }
    }

    fn pending_questions(entries: &[(&str, JobId)]) -> super::PendingQuestion {
        let entries = entries.iter();
        let entries = entries.map(|(id, job)| super::PendingQuestionEntry::new(question(id), *job));
        super::PendingQuestion::new(entries.collect()).unwrap()
    }

    fn ask(id: &str, index: u32) -> AssistantItem {
        tool_call(index, id, "ask", json!({"id":id, "prompt":"Question?"}))
    }

    /// A running ask job of the root agent and the context its handler receives.
    async fn ask_context(
        session: &SessionHandle,
        workspace: &Path,
        spec: JobSpec,
    ) -> (ToolContext, crate::job::JobWorker) {
        let jobs = &session.runtime.jobs;
        let parent = spec.parent;
        let lease = jobs.test_running(spec).await;
        let here = crate::execution::ExecutionLocation::root(workspace.to_owned());
        let subject = crate::tool::authorization::AuthorizationSubject {
            agent: session.root.clone(),
            job: lease.id(),
            parent,
            capabilities: session.runtime.capabilities.clone(),
            cancellation: lease.cancellation_token(),
        };
        let (input, worker) = lease.split();
        let context = ToolContext::new(subject, here.clone(), here, input, jobs.clone());
        (context, worker)
    }

    async fn hanging_session(root: &tempfile::TempDir) -> SessionHandle {
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, Arc::new(HangingProvider)).await;
        harness.new_session().await.unwrap()
    }

    async fn mode_session(
        root: &Path,
        provider: Arc<dyn Provider>,
        answer: &str,
        cancel: bool,
    ) -> (SessionHandle, Batches, Backgrounds) {
        let (batches, backgrounds) = (Batches::default(), Backgrounds::default());
        let handler = RecordingQuestions {
            batches: batches.clone(),
            backgrounds: backgrounds.clone(),
            answer: answer.into(),
            cancel,
        };
        let harness = test_builder(root, &root.join("sessions"), provider, false)
            .question_handler(Arc::new(handler))
            .build()
            .await
            .unwrap();
        (harness.new_session().await.unwrap(), batches, backgrounds)
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_root_questions_are_merged_and_answers_are_split() {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let asks = response(vec![ask("first", 0), ask("second", 1)]);
        let provider = scripted_provider(&requests, [asks, answer("done")]);
        let (session, batches, backgrounds) =
            mode_session(root.path(), provider, "yes", false).await;
        assert_eq!(bounded(session.prompt("ask twice")).await.unwrap(), "done");
        {
            let batches = batches.lock().unwrap();
            assert_eq!(batches.len(), 1);
            // Concurrent asks join the batch in arrival order.
            let mut ids = batches[0].iter().map(|q| q.id.as_str()).collect::<Vec<_>>();
            ids.sort_unstable();
            assert_eq!(ids, ["first", "second"]);
            let requests = requests.lock().unwrap();
            let crate::provider::protocol::Message::Tool(results) =
                requests[1].history.last().unwrap()
            else {
                panic!("missing tool results")
            };
            assert_eq!(results[0].result["result"], "yes");
            assert_eq!(results[1].result["result"], "yes");
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
            let provider = Arc::new(HangingProvider);
            let (session, _, backgrounds) =
                mode_session(root.path(), provider, "yes", background).await;
            let (runtime, agent) = (&session.runtime, session.root.clone());
            let source = format!(
                "async function ask(id, bg) {{ try {{ const answer = await tool.ask({{id,prompt:'Question?',bg}}); return bg ? {{job:answer.id}} : {{answer:answer.unwrap()}}; }} catch (error) {{ return {{error:String(error)}}; }} }} return await Promise.all([ask('first',false), ask('second',{ask_background})]);"
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
                    assert!(
                        result
                            .rendered_error(&CapabilitySet::default())
                            .unwrap()
                            .contains("question cancelled")
                    );
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

    /// A sibling ask holds its batch open until it joins or, like one rejected
    /// before it runs, ends. A later ask from another issuer does not close it, and
    /// one dropped after joining leaves neither its question nor its join behind.
    #[tokio::test(start_paused = true)]
    async fn sibling_asks_hold_the_batch_until_they_join_or_end() {
        let root = tempfile::tempdir().unwrap();
        let provider = Arc::new(HangingProvider);
        let (session, batches, _) = mode_session(root.path(), provider, "yes", false).await;
        let spec = || JobSpec {
            accepts_input: true,
            role: crate::job::JobRole::Question,
            ..JobSpec::test(session.root.clone(), "ask")
        };
        let (context, _worker) = ask_context(&session, root.path(), spec()).await;
        let sibling = session.runtime.jobs.test_running(spec()).await;
        let questions = &session.runtime.questions;
        let script = (session.runtime.jobs)
            .test_running(JobSpec::test(session.root.clone(), "script"))
            .await;
        let scripted = JobSpec {
            parent: Some(script.id()),
            ..spec()
        };
        let (scripted, _scripted_worker) = ask_context(&session, root.path(), scripted).await;
        let scripted = questions.coordinate_question(scripted, question("scripted"));
        assert_eq!(bounded(scripted).await.unwrap(), json!("yes"));
        let mut asked = Box::pin(questions.coordinate_question(context, question("first")));
        let key = (session.root.clone(), None);
        let opened = async || questions.batches.lock().await.contains_key(&key);
        pending_until(&mut asked, opened).await;
        let joined = || {
            questions
                .joined
                .lock()
                .unwrap()
                .get(&key)
                .map(|entry| entry.1)
        };
        let (dropped, _dropped_worker) = ask_context(&session, root.path(), spec()).await;
        let mut dropped = Box::pin(questions.coordinate_question(dropped, question("dropped")));
        pending_until(&mut dropped, async || joined() == Some(2)).await;
        drop(dropped);
        assert_eq!(joined(), Some(1));
        session.runtime.jobs.cancel(sibling.id()).await.unwrap();
        assert_eq!(bounded(asked).await.unwrap(), json!("yes"));
        assert_eq!(joined(), None);
        let presented = batches.lock().unwrap().clone();
        let ids: Vec<Vec<&str>> = (presented.iter())
            .map(|batch| batch.iter().map(|question| question.id.as_str()).collect())
            .collect();
        assert_eq!(ids, [["scripted"], ["first"]]);
        shutdown_session(session).await;
    }

    #[tokio::test]
    async fn background_child_asks_merge_across_turns_and_resolve_independently() {
        let root = tempfile::tempdir().unwrap();
        let session = hanging_session(&root).await;
        let owner = owner(&session, false).await;
        start_child(&session, 1, Some(owner)).await;
        let (executor, jobs) = (&session.runtime.executor, &session.runtime.jobs);
        let waiting_with = |count: usize| {
            move |job: &JobEnvelope| {
                job.question
                    .as_ref()
                    .is_some_and(|question| question.questions.len() == count)
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
                assert_eq!(remaining.question.unwrap().questions[0].id, "second");
            }
        }
        assert_eq!(jobs.snapshot(owner).await.unwrap().state, JobState::Running);
        jobs.cancel(owner).await.unwrap();
        shutdown_session(session).await;
    }

    #[tokio::test(start_paused = true)]
    async fn stable_agent_job_routes_answers_and_duplicate_bursts_do_not_block_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let session = hanging_session(&root).await;
        let owner = owner(&session, false).await;
        start_child(&session, 1, Some(owner)).await;
        let jobs = &session.runtime.jobs;
        let mut asks = Vec::new();
        for _ in 0..2 {
            let spec = JobSpec {
                parent: Some(owner),
                accepts_input: true,
                ..JobSpec::test(session.root.child(1), "ask")
            };
            asks.push(jobs.test_running(spec).await);
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
        let mut asks = asks.into_iter();
        let (first, second) = (asks.next().unwrap(), asks.next().unwrap());
        let resolved = [first.id(), second.id()];
        let cancellation = first.cancellation_token();
        let (mut first_input, _first) = first.split();
        let (mut second_input, _second) = second.split();
        assert_eq!(first_input.recv().await.unwrap(), json!("yes"));
        assert_eq!(second_input.recv().await.unwrap(), json!(2));
        let resolve = coordinator.resolve_child_question(owner, &resolved);
        resolve.await.unwrap();
        assert_eq!(jobs.snapshot(owner).await.unwrap().state, JobState::Running);
        let pending = pending_questions(&[("cancel", resolved[0])]);
        coordinator.open_child_questions(pending).await.unwrap();
        bounded(async {
            for _ in 0..40 {
                let answer = coordinator.answer_child_question(owner, json!("cancel me"));
                answer.await.unwrap();
            }
            coordinator.cancel_child_question(owner).await;
        })
        .await;
        assert!(cancellation.is_cancelled());
        for ask in resolved {
            jobs.cancel(ask).await.unwrap();
        }
        jobs.cancel(owner).await.unwrap();
        shutdown_session(session).await;
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_answer_under_backpressure_releases_claim_without_cleanup_task() {
        let root = tempfile::tempdir().unwrap();
        let session = hanging_session(&root).await;
        let owner = owner(&session, false).await;
        start_child(&session, 1, Some(owner)).await;
        let jobs = &session.runtime.jobs;
        // A live ask job with a full input mailbox and no retained-agent resume handler.
        let spec = JobSpec {
            parent: Some(owner),
            accepts_input: true,
            role: crate::job::JobRole::Question,
            ..JobSpec::test(session.root.child(1), "ask")
        };
        let ask = jobs.test_running(spec).await;
        let job = ask.id();
        let (mut input, _worker) = ask.split();
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
        pending_until(&mut abandoned, async || old_route.0.load(Ordering::Acquire)).await;
        // Reused IDs test route generation: a stale claim must not release the new route.
        let job_list = [job];
        bounded(coordinator.resolve_child_question(owner, &job_list))
            .await
            .unwrap();
        coordinator.open_child_questions(pending()).await.unwrap();
        let new_route = route().await;
        assert!(!Arc::ptr_eq(&old_route.0, &new_route.0));
        let mut new_answer = answer("new");
        pending_until(&mut new_answer, async || {
            new_route.0.load(Ordering::Acquire)
        })
        .await;
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

    #[tokio::test(start_paused = true)]
    async fn noninteractive_children_receive_parent_answers_directly_and_through_scripts() {
        for scripted in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let requests = Requests::default();
            let batches = Batches::default();
            let call = if scripted {
                let source =
                    "return (await tool.ask({id:'child', prompt:'parent question'})).result;";
                tool_call(0, "question", "script", json!({ "source": source }))
            } else {
                ask("child", 0)
            };
            let mut capabilities = CapabilitySet::default();
            capabilities.remove(Capability::Interactive);
            let provider = scripted_provider(&requests, [response(vec![call]), answer("done")]);
            let handler = RecordingQuestions {
                batches: batches.clone(),
                answer: "host-answer".into(),
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
            assert_eq!(waiting.question.unwrap().questions[0].id, "child");
            let send = format!("return await tool.job({owner}).send({{value:'parent-answer'}});");
            bounded(session.run_script(send)).await.unwrap();
            assert_eq!(terminal(&session, owner).await.output, Some(json!("done")));
            {
                let requests = requests.lock().unwrap();
                let offers_ask = |r: &ModelRequest| r.tools.iter().any(|tool| tool.name == "ask");
                assert!(requests.iter().all(|r| offers_ask(r)));
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
                let (context, _worker) = ask_context(&session, root.path(), spec).await;
                let job = context.job();
                let asked = questions.coordinate_question(context, question("bypass"));
                let error = bounded(asked).await.unwrap_err();
                assert!(matches!(
                    error.diagnostic().cause,
                    crate::tool::diagnostic::Cause::Denied(_)
                ));
                let state = jobs.snapshot(job).await.unwrap().state;
                assert_ne!(state, JobState::WaitingInput);
                jobs.cancel(job).await.unwrap();
            }
            assert!(batches.lock().unwrap().is_empty());
            shutdown_session(session).await;
        }
    }
}
