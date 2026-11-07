//! Batches questions and presents them to the host handler or to the agent job a
//! child's parent answers through.

use futures_util::{StreamExt, stream::FuturesUnordered};
use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    sync::{Arc, Mutex as StdMutex, PoisonError},
};
use tokio::sync::{Mutex, oneshot};

use crate::{
    agent::{Question, QuestionError, QuestionHandler, QuestionHeader, QuestionReply},
    identity::{AgentId, JobId},
    job::{Asked, JobManager},
    tool::{
        ToolError,
        diagnostic::{Effects, Operation, Subject},
    },
};

pub(super) struct QuestionCoordinator {
    jobs: JobManager,
    handler: Option<Arc<dyn QuestionHandler>>,
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

struct PendingAsk {
    context: crate::tool::ToolContext,
    question: Question,
    result: oneshot::Sender<Result<serde_json::Value, ToolError>>,
}

impl PendingAsk {
    fn asked(&self) -> Asked {
        Asked {
            ask: self.context.job(),
            question: self.question.clone(),
        }
    }

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
            batches: Mutex::default(),
            joined: StdMutex::default(),
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
                .contains(&crate::tool::policy::Capability::Interactive)
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
            let jobs = self.jobs.active(&agent).into_iter();
            jobs.filter(|sibling| {
                sibling.parent == key.1
                    && sibling.role == crate::job::JobRole::Question
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
            self.jobs.supervisor().spawn(async move {
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
            // An ask dropped after joining has no one left to answer. The rest are
            // presented in the order they were called, whichever ran first.
            batch.retain(|ask| !ask.result.is_closed());
            batch.sort_by_key(|ask| ask.context.job());
            if batch.is_empty() {
                return;
            }
            let (runtime, agent) = (self.clone(), key.0.clone());
            let presented = async move { runtime.present_question_batch(agent, batch).await };
            self.jobs.supervisor().spawn(presented);
        }
    }

    async fn present_question_batch(&self, agent: AgentId, batch: Vec<PendingAsk>) {
        // Only explicit background asks expose their own input state. Marking a
        // foreground ask WaitingInput would cause its native call to return early.
        for pending in &batch {
            let job = pending.context.job();
            if self.jobs.is_background(job).await.unwrap_or(false)
                && let Err(error) = self.jobs.request_input(job, vec![pending.asked()]).await
            {
                send_question_error(batch, error.into());
                return;
            }
        }
        if agent.parent().is_some() {
            self.present_child_questions(batch).await;
        } else {
            self.present_root_questions(agent, batch).await;
        }
    }

    async fn present_root_questions(&self, agent: AgentId, batch: Vec<PendingAsk>) {
        let questions: Vec<_> = batch.iter().map(|ask| ask.question.clone()).collect();
        let headers = questions.iter().map(|question| &question.header);
        if let Some(duplicate) = duplicate_header(headers) {
            let error = crate::job::JobError::DuplicateQuestion(duplicate.clone());
            send_question_error(batch, ToolError::from(error).effects(Effects::NotStarted));
            return;
        }
        let mut background = false;
        for ask in &batch {
            // A foreground ask inside a background script does not block the agent.
            match self.jobs.is_effectively_background(ask.context.job()) {
                Ok(true) => {
                    background = true;
                    break;
                }
                Ok(false) => {}
                Err(error) => {
                    send_question_error(batch, error.into());
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
        let headers = questions
            .iter()
            .map(|question| question.header.clone())
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
                        Ok(answer) => split_answers(&headers, answer),
                        Err(error) => headers.iter().map(|_| Err(error.clone())).collect(),
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

    /// Ask the batch of the agent job a child answers through, after what it
    /// already asks, and pass each ask the answer its mailbox receives.
    async fn present_child_questions(&self, batch: Vec<PendingAsk>) {
        let asked = batch.iter().map(PendingAsk::asked).collect();
        let owner = self.jobs.answered_through(batch[0].context.job());
        let opened = match owner {
            Ok(owner) => self.jobs.request_input(owner, asked).await,
            Err(error) => Err(error),
        };
        if let Err(error) = opened {
            send_question_error(batch, error.into());
            return;
        }
        let mut answers = batch
            .into_iter()
            .map(|pending| async move {
                let answer = pending.context.receive().await;
                pending.finish(answer, Operation::Receive);
            })
            .collect::<FuturesUnordered<_>>();
        while answers.next().await.is_some() {}
    }
}

fn question_subject(question: &Question) -> Subject {
    Subject::Label(format!("question {:?}", question.header.as_str()))
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

fn duplicate_header<'a>(
    mut headers: impl Iterator<Item = &'a QuestionHeader>,
) -> Option<&'a QuestionHeader> {
    let mut seen = HashSet::new();
    headers.find(|header| !seen.insert(*header))
}

/// Each question's answer from a host reply, as the value its ask returns.
fn split_answers(
    headers: &[QuestionHeader],
    mut reply: QuestionReply,
) -> Vec<Result<serde_json::Value, String>> {
    headers
        .iter()
        .map(|header| {
            let answer = reply
                .remove(header)
                .ok_or_else(|| format!("answer is missing question `{header}`"))?;
            Ok(serde_json::to_value(answer).expect("answers serialize as JSON strings"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::split_answers;
    use crate::agent::runtime::tests::*;
    use crate::tool::executor::InvocationKind;
    use crate::{
        job::{JobSpec, JobState},
        tool::{ToolContext, policy::Capability},
    };
    use serde_json::json;
    use std::assert_matches;

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
        let reply = reply
            .map(|(header, answer)| (header.parse().unwrap(), answer))
            .into();
        let headers = ["choice", "commented", "missing"].map(|header| header.parse().unwrap());
        let found = split_answers(&headers, reply);
        let suggestion =
            json!({"answer": "Use the default", "comment": "  Keep the existing settings.\n"});
        assert_eq!(found[..2], [Ok(json!("Continue")), Ok(suggestion)]);
        assert!(found[2].as_ref().unwrap_err().contains("missing"));
    }

    fn question(header: &str) -> Question {
        Question::test(header)
    }

    fn ask(header: &str, index: u32) -> AssistantItem {
        tool_call(
            index,
            header,
            "ask",
            json!({"header": header, "prompt": "Question?"}),
        )
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
        ephemeral_session(&harness).await
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
        (ephemeral_session(&harness).await, batches, backgrounds)
    }

    #[tokio::test]
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
            // Concurrent asks are presented in the order they were called.
            let ids = batches[0]
                .iter()
                .map(|q| q.header.as_str())
                .collect::<Vec<_>>();
            assert_eq!(ids, ["first", "second"]);
            let requests = requests.lock().unwrap();
            let crate::provider::protocol::Message::Tool(results) =
                requests[1].messages().last().unwrap()
            else {
                panic!("missing tool results")
            };
            assert_eq!(results[0].result["result"], "yes");
            assert_eq!(results[1].result["result"], "yes");
        }
        assert_eq!(*backgrounds.lock().unwrap(), [false]);

        // A mixed batch, or a foreground batch inside a background script, is
        // irrevocable on dismissal; neither nesting nor launch mode alone is.
        let asks = async |session: &SessionHandle, script_background, ask_background| {
            let source = format!(
                "async function ask(id, bg) {{ try {{ const answer = await tool.ask({{header:id,prompt:'Question?',bg}}); return bg ? {{job:answer.id}} : {{answer:answer.unwrap()}}; }} catch (error) {{ return {{error:String(error)}}; }} }} return await Promise.all([ask('first',false), ask('second',{ask_background})]);"
            );
            let arguments = json!({"source": source, "bg": script_background});
            let script = (session.runtime.executor).run_model(&session.root, "script", arguments);
            let script = bounded(script).await.unwrap();
            let output = terminal(session, script.job).await.output.unwrap();
            for ask in output["value"].as_array().unwrap() {
                if let Some(job) = ask.get("job") {
                    let job = serde_json::from_value(job.clone()).unwrap();
                    let result = terminal(session, job).await;
                    assert_eq!(result.state, JobState::Failed);
                    assert!(
                        result
                            .rendered_error(&CapabilitySet::default())
                            .unwrap()
                            .contains("question cancelled")
                    );
                } else if script_background || ask_background {
                    let error = ask["error"].as_str().unwrap();
                    assert!(error.contains("question cancelled"));
                } else {
                    assert_eq!(ask["answer"], json!("yes"));
                }
            }
        };
        asks(&session, false, false).await;
        assert_eq!(*backgrounds.lock().unwrap(), [false, false]);
        shutdown_session(session).await;
        let provider = Arc::new(HangingProvider);
        let (session, _, backgrounds) = mode_session(root.path(), provider, "yes", true).await;
        for (script_background, ask_background) in [(false, true), (true, false), (true, true)] {
            asks(&session, script_background, ask_background).await;
        }
        assert_eq!(*backgrounds.lock().unwrap(), [true; 3]);
        shutdown_session(session).await;
    }

    /// A sibling ask holds its batch open until it joins or, like one rejected
    /// before it runs, ends. A later ask from another issuer does not close it, and
    /// one dropped after joining leaves neither its question nor its join behind.
    #[tokio::test]
    async fn sibling_asks_hold_the_batch_until_they_join_or_end() {
        let root = tempfile::tempdir().unwrap();
        let provider = Arc::new(HangingProvider);
        let (session, batches, _) = mode_session(root.path(), provider, "yes", false).await;
        let spec = || JobSpec {
            accepts_input: true,
            role: crate::job::JobRole::Question,
            ..JobSpec::test(session.root.clone(), "ask")
        };
        let (context, worker) = ask_context(&session, root.path(), spec()).await;
        let sibling = session.runtime.jobs.test_running(spec()).await;
        let questions = &session.runtime.questions;
        let script = (session.runtime.jobs)
            .test_running(JobSpec::test(session.root.clone(), "script"))
            .await;
        let scripted = JobSpec {
            parent: Some(script.id()),
            ..spec()
        };
        let (scripted, scripted_worker) = ask_context(&session, root.path(), scripted).await;
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
        let (dropped, dropped_worker) = ask_context(&session, root.path(), spec()).await;
        let mut dropped = Box::pin(questions.coordinate_question(dropped, question("dropped")));
        pending_until(&mut dropped, async || joined() == Some(2)).await;
        drop(dropped);
        assert_eq!(joined(), Some(1));
        // Its worker stops on the cancellation, as a cooperative one does.
        session.runtime.jobs.cancel(sibling.id()).await.unwrap();
        drop(sibling);
        assert_eq!(bounded(asked).await.unwrap(), json!("yes"));
        assert_eq!(joined(), None);
        let presented = batches.lock().unwrap().clone();
        let ids: Vec<Vec<&str>> = (presented.iter())
            .map(|batch| {
                batch
                    .iter()
                    .map(|question| question.header.as_str())
                    .collect()
            })
            .collect();
        assert_eq!(ids, [["scripted"], ["first"]]);
        drop((worker, scripted_worker, dropped_worker, script));
        shutdown_session(session).await;
    }

    /// A background child's asks from separate calls merge into its owner's
    /// questions; a repeated id fails. Answering them is
    /// `a_childs_batched_questions_resolve_one_answer_at_a_time`'s.
    #[tokio::test]
    async fn background_child_asks_merge_across_turns() {
        let root = tempfile::tempdir().unwrap();
        let session = hanging_session(&root).await;
        let owner = owner(&session, false).await;
        start_child(&session, 1, Some(owner)).await;
        let (executor, jobs) = (&session.runtime.executor, &session.runtime.jobs);
        let asked = move |state: &CoreState| crate::job::tests::asked(state, owner);
        let waiting_with = async |count: usize| {
            let asked = move |state: &CoreState| asked(state).filter(|ids| ids.len() == count);
            found(&session, [Key::Job(owner)], asked).await
        };
        for (index, id) in ["first", "second", "second"].into_iter().enumerate() {
            let arguments = json!({"header":id, "prompt":"Question?", "bg":true});
            let child = session.root.child(1);
            let result =
                executor.execute_as(InvocationKind::Model, child, "ask", arguments, Some(owner));
            let result = result.await.unwrap();
            assert!(result.background);
            if index == 2 {
                assert_eq!(terminal(&session, result.job).await.state, JobState::Failed);
                continue;
            }
            waiting_with(index + 1).await;
        }
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
                let source =
                    "return (await tool.ask({header:'child', prompt:'parent question'})).result;";
                tool_call(0, "question", "script", json!({ "source": source }))
            } else {
                ask("child", 0)
            };
            let mut capabilities = CapabilitySet::default();
            capabilities.remove(&Capability::Interactive);
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
            let session = ephemeral_session(&harness).await;
            // The idle root is told about its background child's question and would
            // spend a scripted response on it; this test answers through scripts.
            let root_inbox = quiet_root(&session);
            let launch = "return await tool.agent({prompt:'ask parent', depth:0, bg:true});";
            let launched = bounded(session.run_script(launch)).await.unwrap();
            let owner = serde_json::from_value(launched.value["value"]["id"].clone()).unwrap();
            let asked = move |state: &CoreState| crate::job::tests::asked(state, owner);
            let asked = found(&session, [Key::Job(owner)], asked).await;
            assert_eq!(asked, ["child"]);
            let send = format!("return await tool.control({{job:{owner}, send:'parent-answer'}});");
            bounded(session.run_script(send)).await.unwrap();
            let output = terminal(&session, owner).await.output;
            assert_eq!(output, Some(json!({"reply": "done"})));
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
        let root = tempfile::tempdir().unwrap();
        let batches = Batches::default();
        let mut capabilities = CapabilitySet::default();
        capabilities.remove(&Capability::Interactive);
        let (provider, sessions) = (Arc::new(HangingProvider), root.path().join("sessions"));
        let handler = RecordingQuestions {
            batches: batches.clone(),
            ..Default::default()
        };
        let harness = test_builder(root.path(), &sessions, provider, false)
            .capabilities(capabilities)
            .question_handler(Arc::new(handler))
            .build()
            .await
            .unwrap();
        let session = ephemeral_session(&harness).await;
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
            assert_matches!(
                error.diagnostic().cause,
                crate::tool::diagnostic::Cause::Denied(_)
            );
            let state = jobs.snapshot(job).await.unwrap().state;
            assert_ne!(state, JobState::WaitingInput);
            jobs.cancel(job).await.unwrap();
        }
        assert!(batches.lock().unwrap().is_empty());
        shutdown_session(session).await;
    }
}
