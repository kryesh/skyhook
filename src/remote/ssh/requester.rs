//! Who ran an askpass helper, read from `/proc` on the machine serving its prompt.
use crate::{
    job::JobManager,
    remote::prompt::{Requester, RequesterExecutable},
};

/// The requester of a prompt from askpass helper `helper`: the helper's parent,
/// and the job whose running process is the helper or its nearest ancestor.
pub(super) fn requester(helper: u32, jobs: Option<&JobManager>) -> Requester {
    let mut lineage = vec![helper];
    // A reused PID read mid-walk could repeat; the walk stops rather than loop.
    while let Some(parent) = lineage.last().copied().and_then(parent)
        && !lineage.contains(&parent)
    {
        lineage.push(parent);
    }
    Requester {
        executable: lineage
            .get(1)
            .map_or(RequesterExecutable::Unknown, |&parent| executable(parent)),
        job: jobs.and_then(|jobs| jobs.process_job(&lineage)),
    }
}

/// The parent of `pid`, until the chain leaves the visible process tree.
fn parent(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_parent(&stat).filter(|&parent| parent != 0)
}

/// The fourth field of `/proc/<pid>/stat`. The second, the command name in
/// parentheses, may itself hold spaces and parentheses, so fields follow its last `)`.
fn parse_parent(stat: &str) -> Option<u32> {
    let (_, fields) = stat.rsplit_once(')')?;
    fields.split_ascii_whitespace().nth(1)?.parse().ok()
}

/// Reading another user's executable link needs ptrace access, which a
/// set-user-ID program such as sudo denies; its user still identifies it.
fn executable(pid: u32) -> RequesterExecutable {
    if let Ok(path) = std::fs::read_link(format!("/proc/{pid}/exe")) {
        return RequesterExecutable::Path(path);
    }
    // SAFETY: geteuid has no preconditions and does not mutate process state.
    let server = unsafe { libc::geteuid() };
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|status| privileged(&status, server))
        .unwrap_or_default()
}

/// A process whose effective user, in its `/proc/<pid>/status`, is not `server`.
fn privileged(status: &str, server: u32) -> Option<RequesterExecutable> {
    let field = |key: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix(":\t"))
    };
    let name = field("Name")?.to_owned();
    let uid = field("Uid")?
        .split_ascii_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    (uid != server).then_some(RequesterExecutable::Privileged { name, uid })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::JobSpec;

    #[test]
    fn parent_follows_the_command_name_however_it_is_spelled() {
        for stat in [
            "42 (sudo) S 41 42 42 0 -1",
            "42 (a b) c) (d) S 41 42 42 0 -1",
            "42 ()) R 41 1",
        ] {
            assert_eq!(parse_parent(stat), Some(41), "{stat}");
        }
        assert_eq!(parse_parent("42 (sudo"), None);
        assert_eq!(parse_parent("42 (sudo) S"), None);
    }

    #[test]
    fn only_another_effective_user_identifies_an_unreadable_executable() {
        let status =
            |name: &str, uids: &str| format!("Name:\t{name}\nUmask:\t0022\nUid:\t{uids}\n");
        assert_eq!(
            privileged(&status("sudo", "1000\t0\t0\t0"), 1000),
            Some(RequesterExecutable::Privileged {
                name: "sudo".into(),
                uid: 0
            })
        );
        assert_eq!(privileged(&status("sudo", "0\t1000\t0\t0"), 1000), None);
        assert_eq!(privileged("Name:\tsudo\n", 1000), None);
    }

    /// A child killed and reaped however the test ends.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    struct Helper(std::process::Child);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    impl Drop for Helper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn helper() -> Helper {
        Helper(
            std::process::Command::new("sleep")
                .arg("60")
                .spawn()
                .unwrap(),
        )
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn names_the_helpers_parent_and_the_job_of_its_nearest_attached_ancestor() {
        let runtime = crate::tests::TestRuntime::new().await;
        let jobs = &runtime.jobs;
        let job = (jobs
            .test_running(JobSpec::test(runtime.agent.clone(), "exec"))
            .await)
            .into_test_id();
        let helper = helper();
        let pid = helper.0.id();
        let ours = RequesterExecutable::Path(std::env::current_exe().unwrap());
        assert_eq!(
            requester(pid, Some(jobs)),
            Requester {
                executable: ours,
                job: None
            }
        );
        let attached = jobs.process_owner(job).attach(std::process::id());
        assert_eq!(requester(pid, Some(jobs)).job, Some(job));
        assert_eq!(requester(pid, None).job, None);
        drop(attached);
        assert_eq!(requester(pid, Some(jobs)).job, None);
        drop(helper);
        assert_eq!(requester(pid, Some(jobs)), Requester::default());
    }

    /// Without `/proc`, the parent and its executable are unknown, and only the
    /// helper itself can name a job.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn without_proc_only_the_helper_itself_names_a_job() {
        let runtime = crate::tests::TestRuntime::new().await;
        let jobs = &runtime.jobs;
        let job = (jobs
            .test_running(JobSpec::test(runtime.agent.clone(), "exec"))
            .await)
            .into_test_id();
        let helper = helper();
        let pid = helper.0.id();
        let _parent = jobs.process_owner(job).attach(std::process::id());
        assert_eq!(requester(pid, Some(jobs)), Requester::default());
        let _helper = jobs.process_owner(job).attach(pid);
        assert_eq!(
            requester(pid, Some(jobs)),
            Requester {
                executable: RequesterExecutable::Unknown,
                job: Some(job)
            }
        );
    }
}
