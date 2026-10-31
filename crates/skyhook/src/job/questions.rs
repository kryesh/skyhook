//! What a waiting job asks, each question by the ask job its answer goes to, and
//! the answers routed to those asks.
use super::*;
use crate::agent::Question;

/// A question and the ask job that waits for its answer.
#[derive(Clone, Debug)]
pub(crate) struct Asked {
    pub(crate) ask: JobId,
    pub(crate) question: Question,
}

/// What a waiting job asks: one question, whose answer is the whole reply, or
/// questions asked together, each answered under its id, a lone one left either
/// way. A job read back from the journal is known only to ask, so it asks none.
#[derive(Clone, Debug)]
pub(super) enum Asking {
    One(Asked),
    Merged(Vec<Asked>),
}

impl Default for Asking {
    fn default() -> Self {
        Self::Merged(Vec::new())
    }
}

impl Asking {
    fn asked(&self) -> &[Asked] {
        match self {
            Self::One(asked) => std::slice::from_ref(asked),
            Self::Merged(asked) => asked,
        }
    }

    /// The questions as a waiting job presents them.
    pub(super) fn output(&self) -> QuestionOutput {
        let questions = self.asked().iter().map(|asked| asked.question.clone());
        QuestionOutput {
            questions: questions.collect(),
        }
    }

    /// This, asking `more` after what it asks; refused when a question id repeats.
    fn with(self, more: Vec<Asked>) -> Result<Self, JobError> {
        let asked: Vec<_> = self.asked().iter().cloned().chain(more).collect();
        let mut ids = std::collections::HashSet::new();
        if let Some(repeated) = asked.iter().find(|asked| !ids.insert(&asked.question.id)) {
            return Err(JobError::DuplicateQuestion(repeated.question.id.clone()));
        }
        let merged = matches!(&self, Self::Merged(left) if !left.is_empty());
        Ok(match <[Asked; 1]>::try_from(asked) {
            Ok([one]) if !merged => Self::One(one),
            Ok(one) => Self::Merged(one.into()),
            Err(asked) => Self::Merged(asked),
        })
    }

    /// What is left to ask once `ask` ended, if anything.
    fn without(&self, ask: JobId) -> Option<Self> {
        let left: Vec<_> = (self.asked().iter())
            .filter(|asked| asked.ask != ask)
            .cloned()
            .collect();
        match self {
            _ if left.is_empty() => None,
            Self::One(_) => Some(self.clone()),
            Self::Merged(_) => Some(Self::Merged(left)),
        }
    }

    fn asks(&self, ask: JobId) -> bool {
        self.asked().iter().any(|asked| asked.ask == ask)
    }

    /// The answer `reply` gives each question it answers, by its ask job.
    fn answers(&self, reply: Value) -> Result<Vec<(JobId, Value)>, JobError> {
        let asked = match self {
            Self::One(asked) => return Ok(vec![(asked.ask, reply)]),
            Self::Merged(asked) => asked,
        };
        let keyed = reply
            .as_object()
            .filter(|values| (asked.iter()).any(|asked| values.contains_key(&asked.question.id)));
        let values = match (keyed, asked.as_slice()) {
            (None, [lone]) => return Ok(vec![(lone.ask, reply)]),
            (Some(values), _) => values,
            (None, _) if reply.is_object() => return Err(JobError::UnmatchedAnswers),
            (None, _) => return Err(JobError::UnkeyedAnswers),
        };
        let answered = asked.iter().filter_map(|asked| {
            let answer = values.get(&asked.question.id)?;
            Some((asked.ask, answer.clone()))
        });
        Ok(answered.collect())
    }
}

/// What routing a reply to the questions a job asks did.
enum Routed {
    /// It asks none.
    Unasked,
    Delivered,
    /// These asks' mailboxes had no room for their answers.
    Full(Vec<mpsc::Sender<Value>>),
}

impl JobManager {
    /// Ask `asked` of job `id`, after what it already asks: an ask job its own
    /// question, or the agent job its child's questions are answered through.
    /// An ask that ended before this registers it was retired by its end, so it
    /// is left out, and nothing is asked when none is left.
    pub(crate) async fn request_input(&self, id: JobId, asked: Vec<Asked>) -> Result<(), JobError> {
        let declared: Vec<_> = std::iter::once(id)
            .chain(asked.iter().map(|asked| asked.ask))
            .collect();
        let ask = move |state: &CoreState| {
            let unended = |asked: &Asked| state.job(asked.ask).is_ok_and(|ask| ask.end().is_none());
            let asked: Vec<_> = asked.into_iter().filter(unended).collect();
            if asked.is_empty() {
                return Ok(Plan::new(Vec::new(), |_, _| ()));
            }
            let asking = state.job(id)?.asking().cloned().unwrap_or_default();
            let change = JobChange::Ask(asking.with(asked)?);
            lifecycle::journaled(state, id, change, Rejected::input)
        };
        self.inner
            .store
            .commit_on(declared, Command::Journal, ask)
            .await
    }

    /// The agent job whose owner answers ask job `ask`: the nearest one it runs
    /// under.
    pub(crate) fn answered_through(&self, ask: JobId) -> Result<JobId, JobError> {
        (self.facts(|jobs| jobs.answered_through(ask))).ok_or(JobError::UnownedQuestion)
    }

    /// Route `reply` to the asks of the questions job `id` asks: each answer is
    /// accepted into its ask's mailbox once, so a repeated reply answers nothing
    /// again. Routing waits for room in a full mailbox without answering it, so
    /// a reply abandoned meanwhile leaves that question unanswered. False when
    /// `id` asks nothing.
    pub(crate) async fn answer(&self, id: JobId, reply: Value) -> Result<bool, JobError> {
        loop {
            let reply = reply.clone();
            let route = move |state: &mut CoreState| -> Result<Routed, JobError> {
                let Some(asking) = (state.jobs.get(id)).and_then(JobFacts::asking) else {
                    return Ok(Routed::Unasked);
                };
                if asking.asked().is_empty() {
                    return Ok(Routed::Unasked);
                }
                let mut full = Vec::new();
                for (ask, answer) in asking.answers(reply)? {
                    let input = (state.jobs.get(ask))
                        .filter(|entry| !entry.answered())
                        .and_then(|entry| Some(entry.handles.as_ref()?.input.clone()));
                    let Some(input) = input else {
                        continue;
                    };
                    match input.try_send(answer) {
                        Ok(()) => drop(state.jobs.modify(ask, |entry, _| entry.answer())),
                        Err(mpsc::error::TrySendError::Full(_)) => full.push(input),
                        // Its handler stopped listening: it ends unanswered.
                        Err(mpsc::error::TrySendError::Closed(_)) => {}
                    }
                }
                Ok(if full.is_empty() {
                    Routed::Delivered
                } else {
                    Routed::Full(full)
                })
            };
            match self.inner.store.live(route).await? {
                Routed::Unasked => return Ok(false),
                Routed::Delivered => return Ok(true),
                Routed::Full(inputs) => {
                    for input in inputs {
                        drop(input.reserve().await);
                    }
                }
            }
        }
    }
}

impl Jobs {
    fn answered_through(&self, ask: JobId) -> Option<JobId> {
        let mut job = self.get(ask)?.parent?;
        while self.get(job)?.role() != JobRole::Agent {
            job = self.get(job)?.parent?;
        }
        Some(job)
    }

    /// The job whose question ask job `ask` waits on, and the change ending the ask
    /// journals of it: it asks again what it still asks, so its owner is told, or
    /// runs on.
    pub(super) fn unasked(&self, ask: JobId) -> Option<(JobId, AgentId, JobChange)> {
        let asking_job = self.answered_through(ask)?;
        let entry = self.get(asking_job)?;
        let asking = entry.asking().filter(|asking| asking.asks(ask))?;
        let change = match asking.without(ask) {
            Some(left) => JobChange::Ask(left),
            None => JobChange::Advance(JobTransition::Running),
        };
        Some((asking_job, entry.agent.clone(), change))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::tests::{asked, asked_by, runtime};
    use crate::tests::bounded;
    use serde_json::json;

    /// A running agent job and `count` running asks of its child under it, each
    /// with its mailbox and worker.
    async fn asking(
        count: usize,
    ) -> (
        tempfile::TempDir,
        JobManager,
        JobId,
        Vec<(JobId, mpsc::Receiver<Value>, super::super::JobWorker)>,
    ) {
        let (root, jobs, agent) = runtime().await;
        let owner = JobSpec {
            accepts_input: true,
            role: JobRole::Agent,
            ..JobSpec::test(agent.clone(), "agent")
        };
        let owner = jobs.test_running(owner).await.into_test_id();
        let child = jobs.test_install(owner).await;
        let mut asks = Vec::new();
        for _ in 0..count {
            let spec = JobSpec {
                parent: Some(owner),
                role: JobRole::Question,
                accepts_input: true,
                ..JobSpec::test(child.clone(), "ask")
            };
            let lease = jobs.test_running(spec).await;
            let id = lease.id();
            let (input, worker) = lease.split();
            asks.push((id, input, worker));
        }
        (root, jobs, owner, asks)
    }

    fn questions(jobs: &JobManager, id: JobId) -> Option<Vec<String>> {
        jobs.store().read(move |state| asked(state, id))
    }

    fn answered() -> JobOutcome {
        JobOutcome::Completed(ToolOutput::new(json!("answered")).into())
    }

    /// A reply reaches each ask it answers once, however often it repeats and
    /// however far the repeats outnumber a mailbox's room; replies that answer
    /// nothing are refused. Each ask's end retires its question, so the job asks
    /// what is left, refusing an id it still asks, and runs on once none is.
    #[tokio::test]
    async fn replies_answer_each_ask_once_and_ending_asks_retire_their_questions() {
        let (_root, jobs, owner, asks) = asking(2).await;
        let mut asks = asks.into_iter();
        let (first, mut first_input, first_worker) = asks.next().unwrap();
        let (second, mut second_input, second_worker) = asks.next().unwrap();
        let mut both = asked_by(first, "first");
        both.extend(asked_by(second, "second"));
        jobs.request_input(owner, both).await.unwrap();
        let reply = json!({"first": "yes", "second": 2});
        bounded(async {
            for _ in 0..=JOB_INPUT_CAPACITY {
                assert!(jobs.answer(owner, reply.clone()).await.unwrap());
            }
        })
        .await;
        assert_eq!(first_input.try_recv().unwrap(), json!("yes"));
        assert_eq!(second_input.try_recv().unwrap(), json!(2));
        assert!(first_input.try_recv().is_err() && second_input.try_recv().is_err());
        for (unanswerable, refusal) in [
            (json!("unkeyed"), "keyed by question id"),
            (json!({"other": 1}), "no pending question ids"),
        ] {
            let error = jobs.answer(owner, unanswerable).await.unwrap_err();
            assert!(error.to_string().contains(refusal), "{error}");
        }
        first_worker.fail(answered()).await;
        assert_eq!(questions(&jobs, owner), Some(vec!["second".to_owned()]));
        let repeated = jobs.request_input(owner, asked_by(owner, "second")).await;
        assert!(matches!(repeated, Err(JobError::DuplicateQuestion(id)) if id == "second"));
        // The lone question left was answered already.
        assert!(jobs.answer(owner, json!("again")).await.unwrap());
        assert!(second_input.try_recv().is_err());
        second_worker.fail(answered()).await;
        assert_eq!(jobs.snapshot(owner).await.unwrap().state, JobState::Running);
        assert!(!jobs.answer(owner, json!("update")).await.unwrap());
    }

    /// An ask that ends between finding the job it is answered through and asking
    /// it there is not asked: the job runs on, asking nothing.
    #[tokio::test]
    async fn an_ask_ended_before_it_is_asked_is_left_out() {
        let (_root, jobs, owner, mut asks) = asking(1).await;
        let (ask, _input, worker) = asks.pop().unwrap();
        assert_eq!(jobs.answered_through(ask).unwrap(), owner);
        worker.fail(answered()).await;
        jobs.request_input(owner, asked_by(ask, "late"))
            .await
            .unwrap();
        assert_eq!(questions(&jobs, owner), None);
        assert_eq!(jobs.snapshot(owner).await.unwrap().state, JobState::Running);
    }

    /// One question's answer is the whole reply, even one keyed by its id.
    #[tokio::test]
    async fn a_lone_question_takes_the_whole_reply() {
        let (_root, jobs, owner, mut asks) = asking(1).await;
        let (ask, input, _worker) = &mut asks[0];
        jobs.request_input(owner, asked_by(*ask, "solo"))
            .await
            .unwrap();
        let reply = json!({"solo": "kept whole"});
        assert!(jobs.answer(owner, reply.clone()).await.unwrap());
        assert_eq!(input.try_recv().unwrap(), reply);
    }

    /// An answer to a full mailbox waits for room without answering, so one
    /// abandoned meanwhile leaves its question to the next reply, which arrives
    /// once, behind what the mailbox held.
    #[tokio::test]
    async fn a_full_mailbox_defers_the_answer_and_an_abandoned_one_answers_nothing() {
        let (_root, jobs, owner, mut asks) = asking(1).await;
        let (ask, input, _worker) = &mut asks[0];
        let ask = *ask;
        for _ in 0..JOB_INPUT_CAPACITY {
            jobs.send(ask, json!("backlog")).await.unwrap();
        }
        jobs.request_input(owner, asked_by(ask, "one"))
            .await
            .unwrap();
        let mut abandoned = Box::pin(jobs.answer(owner, json!("abandoned")));
        assert!(futures_util::poll!(&mut abandoned).is_pending());
        drop(abandoned);
        // Its routing, queued on the core, has run once a later change has.
        jobs.store().live(|_| ()).await;
        let waiting = tokio::spawn({
            let jobs = jobs.clone();
            async move { jobs.answer(owner, json!("waited")).await }
        });
        let mut received = Vec::new();
        while received.len() <= JOB_INPUT_CAPACITY {
            received.push(bounded(input.recv()).await.unwrap());
        }
        assert!(bounded(waiting).await.unwrap().unwrap());
        assert!(jobs.answer(owner, json!("repeated")).await.unwrap());
        assert!(input.try_recv().is_err());
        let backlog = vec![json!("backlog"); JOB_INPUT_CAPACITY];
        assert_eq!(received[..JOB_INPUT_CAPACITY], backlog);
        assert_eq!(received[JOB_INPUT_CAPACITY], json!("waited"));
    }
}
