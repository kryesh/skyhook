//! Batches questions and routes answers through host handlers or stable agent jobs.

use futures_util::future::join_all;
use serde_json::json;
use std::{collections::HashMap, sync::Arc};
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
        let Some(handler) = self.handler.clone() else {
            send_question_error(batch, QuestionError::Unavailable.to_string());
            return;
        };
        let answer = handler.ask(agent, questions);
        tokio::pin!(answer);
        let answer = tokio::select! {
            answer = &mut answer => answer.map_err(|error| error.to_string()),
            () = batch[0].context.cancelled() => Err("tool was cancelled".to_owned()),
        };
        match answer {
            Ok(answer) => distribute_question_answer(batch, answer),
            Err(error) => send_question_error(batch, error),
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
        let answers = join_all(batch.iter().map(|pending| pending.context.receive())).await;
        let resolved = self.resolve_child_question(owner_job).await;
        if let Err(error) = resolved {
            send_question_error(batch, error.to_string());
            return;
        }
        for (pending, answer) in batch.into_iter().zip(answers) {
            let _ = pending
                .result
                .send(answer.map_err(|error| error.to_string()));
        }
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
        {
            let mut pending = self.pending.lock().await;
            if pending.contains_key(&owner_job) {
                return Err(HarnessError::Agent(
                    "an agent job may only have one outstanding question batch".to_owned(),
                ));
            }
            pending.insert(owner_job, PendingQuestion { ask_jobs });
        }
        if let Err(error) = self.jobs.request_input(owner_job, output).await {
            self.pending.lock().await.remove(&owner_job);
            return Err(error.into());
        }
        Ok(owner_job)
    }

    pub(super) async fn resolve_child_question(
        &self,
        owner_job: JobId,
    ) -> Result<(), HarnessError> {
        self.pending.lock().await.remove(&owner_job);
        self.jobs.resume_input(owner_job).await?;
        Ok(())
    }

    pub(super) async fn answer_child_question(
        &self,
        owner_job: JobId,
        value: serde_json::Value,
    ) -> Result<bool, HarnessError> {
        let pending = self.pending.lock().await.get(&owner_job).cloned();
        let Some(pending) = pending else {
            return Ok(false);
        };
        let ids = pending
            .ask_jobs
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let answers = split_answers(&ids, value)
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(HarnessError::Agent)?;
        for ((_, ask_job), answer) in pending.ask_jobs.iter().zip(answers) {
            self.jobs.send(*ask_job, answer).await?;
        }
        Ok(true)
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

fn distribute_question_answer(batch: Vec<PendingAsk>, answer: serde_json::Value) {
    let ids = batch
        .iter()
        .map(|pending| pending.question.id.clone())
        .collect::<Vec<_>>();
    for (pending, result) in batch.into_iter().zip(split_answers(&ids, answer)) {
        let _ = pending.result.send(result);
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
