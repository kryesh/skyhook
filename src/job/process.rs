//! Which running job a process belongs to.
use super::*;

/// Attributes the processes a job runs to it.
#[derive(Clone)]
pub(crate) struct ProcessOwner {
    jobs: JobManager,
    job: JobId,
}

/// A job's process, attributed to the job until this is dropped.
pub(crate) struct JobProcess {
    owner: ProcessOwner,
    pid: u32,
}

impl ProcessOwner {
    pub(crate) fn attach(&self, pid: u32) -> JobProcess {
        self.jobs
            .handles(self.job, |handles| handles.process = Some(pid));
        JobProcess {
            owner: self.clone(),
            pid,
        }
    }
}

impl JobManager {
    pub(crate) fn process_owner(&self, job: JobId) -> ProcessOwner {
        ProcessOwner {
            jobs: self.clone(),
            job,
        }
    }

    /// The job whose attached process comes first in `pids`.
    pub(crate) fn process_job(&self, pids: &[u32]) -> Option<JobId> {
        let handles = (self.inner.handles)
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        pids.iter().find_map(|&pid| {
            (handles.iter())
                .find_map(|(&job, handles)| (handles.process == Some(pid)).then_some(job))
        })
    }
}

impl Drop for JobProcess {
    fn drop(&mut self) {
        let pid = self.pid;
        self.owner.jobs.handles(self.owner.job, |handles| {
            if handles.process == Some(pid) {
                handles.process = None;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::tests::runtime;

    #[tokio::test]
    async fn the_nearest_attached_ancestor_names_the_job_until_its_process_ends() {
        let (_root, jobs, agent) = runtime().await;
        let mut ids = Vec::new();
        for tool in ["outer", "inner"] {
            let lease = jobs.test_running(JobSpec::test(agent.clone(), tool)).await;
            ids.push(lease.into_test_id());
        }
        let outer = jobs.process_owner(ids[0]).attach(10);
        let inner = jobs.process_owner(ids[1]).attach(20);
        assert_eq!(jobs.process_job(&[30, 20, 10, 1]), Some(ids[1]));
        assert_eq!(jobs.process_job(&[30, 1]), None);
        drop(inner);
        assert_eq!(jobs.process_job(&[30, 20, 10, 1]), Some(ids[0]));
        drop(outer);
        assert_eq!(jobs.process_job(&[30, 20, 10, 1]), None);
    }
}
