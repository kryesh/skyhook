//! Jobs read a page at a time, for hosts: an owner's root jobs, a parent's children,
//! and jobs by id.

use super::{JobEnvelope, JobError, JobManager, Settled};
use crate::{
    identity::{AgentId, JobId},
    session::{Page, SessionError},
};

/// Which jobs a page lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobScope {
    /// The jobs `owner` created that do not nest under a parent
    /// ([`JobEnvelope::nests`]): the roots of its view.
    Roots(AgentId),
    /// A parent's children, whoever created them.
    Children(JobId),
}

impl JobManager {
    /// Up to `limit` jobs of `scope` at `page`, in id order: as the session holds them,
    /// or as the journal leaves them.
    pub async fn page(
        &self,
        scope: JobScope,
        page: Page<JobId>,
        limit: usize,
    ) -> Result<Vec<JobEnvelope>, JobError> {
        self.listed(move |journal| match &scope {
            JobScope::Roots(owner) => journal.root_jobs(owner, page, limit),
            JobScope::Children(parent) => journal.child_jobs(*parent, page, limit),
        })
        .await
    }

    /// The jobs `ids` names that exist, in id order.
    pub async fn views(&self, mut ids: Vec<JobId>) -> Result<Vec<JobEnvelope>, JobError> {
        ids.sort_unstable();
        ids.dedup();
        self.listed(move |_| Ok(ids)).await
    }

    /// The jobs `list` lists, as the session holds them or the journal leaves them.
    pub(super) async fn listed(
        &self,
        list: impl FnOnce(&crate::session::Journal) -> Result<Vec<JobId>, SessionError> + Send + 'static,
    ) -> Result<Vec<JobEnvelope>, JobError> {
        let manager = self.clone();
        let page = move || {
            let journal = manager.store().journal();
            let listed = list(&journal)?;
            // Only held jobs move: one a commit not yet applied moves is held until then.
            let (mut jobs, settled) = manager.inner.store.read(|state| {
                let held = |id: &JobId| state.jobs.get(*id).map(|entry| entry.metadata(*id));
                let jobs: Vec<_> = listed.iter().filter_map(held).collect();
                let settled = listed.iter().filter(|id| !state.jobs.contains(**id));
                (jobs, settled.copied().collect::<Vec<_>>())
            });
            let settled = Settled::read(&journal, &settled)?;
            jobs.extend(settled.into_iter().map(|(id, entry)| entry.metadata(id)));
            jobs.sort_by_key(|job| job.id);
            Ok::<_, SessionError>(jobs)
        };
        Ok(self.store().reads().hold(page).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::{ObservationSnapshot, RuntimeEvent},
        job::{JobEnd, JobRole, JobState, JobTransition},
        session::{SessionEvent, tests::job_created},
        tests::TestRuntime,
    };

    /// Observing holds only the jobs the session still holds, however many it let
    /// go, which pages read instead, and so does an observer that saw them finish;
    /// a child created under an older parent nests under it, never as a root of the
    /// same owner.
    #[tokio::test]
    async fn observing_holds_live_jobs_and_pages_read_the_rest() {
        let TestRuntime {
            root: _root,
            store,
            agent,
            jobs,
        } = TestRuntime::on_disk().await;
        let append = async |event| store.append(agent.clone(), event).await.unwrap();
        let id = |id: u64| JobId::new(id).unwrap();
        let started = async |job| {
            append(job_created(job, JobRole::Tool)).await;
            for state in [JobTransition::AwaitingApproval, JobTransition::Running] {
                append(SessionEvent::JobStateChanged { job, state }).await;
            }
        };
        let mut early = store.observe();
        started(id(1)).await;
        for job in (2..=6).map(id) {
            started(job).await;
            let state = JobEnd::Completed;
            let images = Vec::new();
            let diagnostic = None;
            append(SessionEvent::JobFinished {
                job,
                state,
                diagnostic,
                images,
            })
            .await;
            append(SessionEvent::JobClaimed { job }).await;
        }
        let observation = store.observe();
        assert_eq!(
            observation.snapshot.jobs.keys().collect::<Vec<_>>(),
            [&id(1)]
        );
        let mut updates = observation.updates;
        let mut child = job_created(id(7), JobRole::Tool);
        if let SessionEvent::JobCreated { parent, .. } = &mut child {
            *parent = Some(id(1));
        }
        append(child).await;
        let listed =
            |page: Vec<JobEnvelope>| page.into_iter().map(|job| job.id).collect::<Vec<_>>();
        let roots = jobs
            .page(JobScope::Roots(agent.clone()), Page::First, 64)
            .await
            .unwrap();
        assert_eq!(listed(roots), (1..=6).map(id).collect::<Vec<_>>());
        let next = jobs.page(JobScope::Roots(agent.clone()), Page::After(id(2)), 2);
        assert_eq!(listed(next.await.unwrap()), [id(3), id(4)]);
        let before = jobs.page(JobScope::Roots(agent.clone()), Page::Before(id(4)), 2);
        assert_eq!(listed(before.await.unwrap()), [id(2), id(3)]);
        let last = jobs.page(JobScope::Roots(agent.clone()), Page::Last, 2);
        assert_eq!(listed(last.await.unwrap()), [id(5), id(6)]);
        let children = jobs
            .page(JobScope::Children(id(1)), Page::First, 64)
            .await
            .unwrap();
        assert_eq!(listed(children), [id(7)]);
        let told = std::iter::from_fn(|| updates.try_recv().ok())
            .any(|update| matches!(update.event, RuntimeEvent::Job(job) if job.id == id(7)));
        assert!(told);
        let mut unheld = Vec::new();
        while let Ok(update) = early.updates.try_recv() {
            unheld.extend(early.snapshot.apply(update).unheld);
        }
        let held =
            |snapshot: &ObservationSnapshot| snapshot.jobs.keys().copied().collect::<Vec<_>>();
        assert_eq!(held(&early.snapshot), [id(1), id(7)]);
        assert_eq!(held(&store.observe().snapshot), [id(1), id(7)]);
        let finished = unheld.iter().map(|job| (job.id, job.state));
        let completed = (2..=6).map(|job| (id(job), JobState::Completed));
        assert_eq!(finished.collect::<Vec<_>>(), completed.collect::<Vec<_>>());
    }
}
