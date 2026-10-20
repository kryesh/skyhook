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
        self.jobs.processes().insert(pid, self.job);
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
        let processes = self.processes();
        pids.iter().find_map(|pid| processes.get(pid).copied())
    }

    fn processes(&self) -> std::sync::MutexGuard<'_, HashMap<u32, JobId>> {
        (self.inner.processes)
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for JobProcess {
    fn drop(&mut self) {
        let mut processes = self.owner.jobs.processes();
        if processes.get(&self.pid) == Some(&self.owner.job) {
            processes.remove(&self.pid);
        }
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
