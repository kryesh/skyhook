//! A spawned child's process tree, so stopping it also reaches descendants.

use std::io;

/// Every process a spawned child starts: on Unix the process group it leads, on
/// Windows a job object it joined before it ran, which its descendants cannot leave.
/// The tree is killed at most once: by [`kill`](Self::kill) or on drop, unless
/// [`release`](Self::release)d. On Windows the job also kills the tree when this
/// process exits without dropping it.
pub struct ProcessGroup(Option<Tree>);

#[cfg(unix)]
type Tree = libc::pid_t;
#[cfg(windows)]
type Tree = std::os::windows::io::OwnedHandle;

impl ProcessGroup {
    /// Spawn `command` with its process tree. On Unix the command must already be set
    /// to lead its own group (`process_group(0)` or `setsid`); on Windows this sets its
    /// creation flags.
    pub fn spawn(
        command: &mut tokio::process::Command,
    ) -> io::Result<(tokio::process::Child, Self)> {
        #[cfg(unix)]
        {
            let child = super::spawn(command)?;
            let leader = child.id().and_then(|id| libc::pid_t::try_from(id).ok());
            Ok((child, Self(leader)))
        }
        #[cfg(windows)]
        {
            let job = windows::job()?;
            command.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
            let mut child = super::spawn(command)?;
            // Suspended until it is in the job, so it starts nothing outside it.
            let started = match (child.raw_handle(), child.id()) {
                (Some(process), Some(id)) => {
                    windows::start_in(&job, process, || windows::resume(id))
                }
                _ => Err(io::ErrorKind::NotFound.into()),
            };
            if let Err(error) = started {
                let _ = child.start_kill();
                return Err(error);
            }
            Ok((child, Self(Some(job))))
        }
    }

    /// The tree of `process`, created suspended with its one thread `thread`, which
    /// runs once the process is in the tree.
    #[cfg(windows)]
    pub(crate) fn start(
        process: &std::os::windows::io::OwnedHandle,
        thread: &std::os::windows::io::OwnedHandle,
    ) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle as _;
        let job = windows::job()?;
        windows::start_in(&job, process.as_raw_handle(), || {
            windows::resume_thread(thread)
        })?;
        Ok(Self(Some(job)))
    }

    /// Kill every process in the tree.
    pub fn kill(&mut self) {
        let Some(tree) = self.0.take() else { return };
        #[cfg(unix)]
        // SAFETY: kill takes integer IDs and accesses no memory. The negative
        // ID targets only the group our child was spawned to lead.
        unsafe {
            libc::kill(-tree, libc::SIGKILL);
        }
        #[cfg(windows)]
        windows::terminate(&tree);
    }

    /// Stop owning the tree without killing it.
    pub fn release(mut self) {
        #[cfg(windows)]
        if let Some(job) = &self.0 {
            windows::keep_on_close(job);
        }
        self.0 = None;
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(windows)]
mod windows {
    use std::{
        io,
        os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle, RawHandle},
    };
    use windows_sys::Win32::{
        Foundation::{FALSE, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
            },
            Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
        },
    };

    fn check(succeeded: bool) -> io::Result<()> {
        succeeded.then_some(()).ok_or_else(io::Error::last_os_error)
    }

    /// A new anonymous job whose processes die when its last handle closes.
    pub(super) fn job() -> io::Result<OwnedHandle> {
        // SAFETY: both arguments may be null; the result is a new handle or null.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        check(!job.is_null())?;
        // SAFETY: the handle was just created and is owned here alone.
        let job = unsafe { OwnedHandle::from_raw_handle(job) };
        limit(&job, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)?;
        Ok(job)
    }

    fn limit(job: &OwnedHandle, flags: JOB_OBJECT_LIMIT) -> io::Result<()> {
        // SAFETY: the structure is plain data, valid when zeroed.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = flags;
        // SAFETY: the job handle is open, and the pointer and length describe `limits`.
        check(
            unsafe {
                SetInformationJobObject(
                    job.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    size_of_val(&limits) as u32,
                )
            } != FALSE,
        )
    }

    /// Put the suspended `process` in `job`, then let it run.
    pub(super) fn start_in(
        job: &OwnedHandle,
        process: RawHandle,
        resume: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        // SAFETY: both handles are open: the job is owned here and the process not yet
        // reaped.
        check(unsafe { AssignProcessToJobObject(job.as_raw_handle(), process) } != FALSE)?;
        resume()
    }

    pub(super) fn resume_thread(thread: &OwnedHandle) -> io::Result<()> {
        // SAFETY: the thread handle is open with suspend-resume access.
        check(unsafe { ResumeThread(thread.as_raw_handle()) } != u32::MAX)
    }

    /// Resume the one thread of the suspended process `id`. The standard library keeps
    /// the handle of the thread it created to itself, so the thread is found by its
    /// process.
    pub(super) fn resume(id: u32) -> io::Result<()> {
        // SAFETY: the snapshot takes no pointers; the result is a new handle or invalid.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        check(snapshot != INVALID_HANDLE_VALUE)?;
        // SAFETY: the handle was just created and is owned here alone.
        let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
        // SAFETY: the structure is plain data, valid when zeroed.
        let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        // SAFETY: the snapshot is open and `entry` is a sized THREADENTRY32.
        let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &raw mut entry) };
        while found != FALSE {
            if entry.th32OwnerProcessID == id {
                // SAFETY: OpenThread takes integers; the result is a new handle or null.
                let thread =
                    unsafe { OpenThread(THREAD_SUSPEND_RESUME, FALSE, entry.th32ThreadID) };
                check(!thread.is_null())?;
                // SAFETY: the handle was just opened and is owned here alone.
                return resume_thread(&unsafe { OwnedHandle::from_raw_handle(thread) });
            }
            // SAFETY: as for Thread32First.
            found = unsafe { Thread32Next(snapshot.as_raw_handle(), &raw mut entry) };
        }
        Err(io::ErrorKind::NotFound.into())
    }

    pub(super) fn terminate(job: &OwnedHandle) {
        // SAFETY: the job handle is open; the call takes no pointers.
        unsafe {
            TerminateJobObject(job.as_raw_handle(), 1);
        }
    }

    /// Let the job's processes outlive its handle.
    pub(super) fn keep_on_close(job: &OwnedHandle) {
        let _ = limit(job, 0);
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use tokio_util::sync::CancellationToken;

    use crate::local::Local;
    use crate::op::{Exec, ExecEnd, ExecEvent, Program, TargetPath};
    use crate::target::Target as _;
    use crate::tests::bounded;

    /// `program` run in a fresh directory whose deadline elapses once `deadline` is
    /// cancelled, and the directory.
    fn exec(program: Program, deadline: &CancellationToken) -> (Local, Exec, tempfile::TempDir) {
        let root = tempfile::tempdir().unwrap();
        let elapses = deadline.clone();
        let sleep: crate::local::Sleep =
            Arc::new(move |_| Box::pin(elapses.clone().cancelled_owned()));
        let local = Local::with_clock(root.path().to_owned(), sleep);
        let exec = Exec {
            program,
            cwd: TargetPath::new(root.path().to_str().unwrap()),
            env: Default::default(),
            timeout: Some(Duration::from_secs(3600)),
            spill: crate::op::StreamSpill::never(),
        };
        (local, exec, root)
    }

    /// Dropping a run and its deadline both stop the whole tree, even a descendant the
    /// shell left behind.
    #[tokio::test]
    async fn cancellation_and_timeout_kill_descendants_even_after_the_shell_exits() {
        // The descendant holds `held`, a FIFO, open for writing. Its reader sees EOF
        // once every holder has died, whether or not anything reaps them.
        let command = "exec 3>held; sleep 3600 & printf started; exit 0";
        for timeout in [false, true] {
            let deadline = CancellationToken::new();
            let (local, exec, root) = exec(Program::Shell(command.into()), &deadline);
            let held = root.path().join("held");
            crate::tests::fifo(&held);
            let mut held = tokio::net::unix::pipe::OpenOptions::new()
                .open_receiver(&held)
                .unwrap();
            let Ok(mut running) = local.run(exec).await;
            loop {
                match bounded(running.event()).await {
                    Some(ExecEvent::Output { .. }) => break,
                    Some(_) => {}
                    None => panic!("the shell ended before its output"),
                }
            }
            if timeout {
                deadline.cancel();
                let ended = bounded(running.end()).await;
                assert!(
                    matches!(
                        ended,
                        Ok(Ok(ExecEnd {
                            status: crate::op::ExecStatus::TimedOut { .. },
                            ..
                        }))
                    ),
                    "{ended:?}"
                );
            } else {
                drop(running);
            }
            let mut rest = Vec::new();
            bounded(tokio::io::AsyncReadExt::read_to_end(&mut held, &mut rest))
                .await
                .unwrap();
        }
    }

    /// Every process leads its own session.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn each_process_leads_its_own_session() {
        let stat = "read pid comm state ppid pgrp sid rest < /proc/self/stat; printf '%s %s' \"$pid\" \"$sid\"";
        let program = Program::Argv {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), stat.into()],
        };
        let (local, exec, _root) = exec(program, &CancellationToken::new());
        let Ok(mut running) = local.run(exec).await;
        let mut stdout = String::new();
        while let Some(event) = bounded(running.event()).await {
            if let ExecEvent::Output { text, .. } = event {
                stdout += &text;
            }
        }
        let (pid, sid) = stdout.split_once(' ').unwrap();
        assert_eq!(pid, sid);
    }
}
