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

    async fn present_root_questions(
        &self,
        agent: AgentId,
        batch: Vec<PendingAsk>,
        questions: Vec<Question>,
    ) {
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
                    .ask(agent, questions)
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
}
