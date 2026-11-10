//! An SSH process on Windows, created here so it can be given a pseudo console, which
//! the standard library cannot attach. It runs in a process tree of its own, and its
//! standard streams are pipes whose ends here overlap.
use std::{
    ffi::OsString,
    io,
    os::windows::{
        io::{AsRawHandle as _, OwnedHandle},
        process::ExitStatusExt as _,
    },
};

use tokio::{io::AsyncReadExt as _, net::windows::named_pipe::NamedPipeServer};
use windows_sys::Win32::{
    Foundation::WAIT_OBJECT_0,
    System::Threading::{GetExitCodeProcess, INFINITE, TerminateProcess, WaitForSingleObject},
};

use super::{Diagnostics, Spawned};
use crate::{
    origin::ProcessEnvironment,
    process::{
        Console, ProcessGroup,
        windows::{
            check, create,
            pipe::{self, Theirs, from_child, to_child},
        },
    },
};

/// A process's error output, and this process's own writer into it. The writer stays
/// out of the runtime: a pipe end the runtime registers takes no completion routines,
/// which the child writes its end with.
pub(in crate::origin::ssh) struct Errors {
    pipe: NamedPipeServer,
    mark: Option<OwnedHandle>,
}

impl tokio::io::AsyncRead for Errors {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.pipe).poll_read(context, buffer)
    }
}

/// Keep what a finished process wrote to `errors` before it exited. Output the
/// runtime has read can wait for its next turn, so a mark this process writes after
/// the exit, which no other writer can know, ends it. The mark is written while the
/// pipe is read, which a full pipe needs.
pub(super) async fn written(errors: &mut Errors, diagnostics: &mut Diagnostics) {
    let Some(writer) = errors.mark.take() else {
        return;
    };
    let mut mark = [0; 16];
    if getrandom::fill(&mut mark).is_err() {
        return;
    }
    let marking = skyhook_base::blocking(move || pipe::write(&writer, &mark));
    let reading = async {
        let mut unread = Vec::new();
        let mut buffer = [0; 1024];
        while let Ok(read @ 1..) = errors.pipe.read(&mut buffer).await {
            unread.extend_from_slice(&buffer[..read]);
            if let Some(at) = (unread.windows(mark.len())).position(|window| window == mark) {
                diagnostics.keep(&unread[..at]);
                return;
            }
            // The end may hold the start of the mark.
            let settled = unread.len().saturating_sub(mark.len() - 1);
            diagnostics.keep(&unread[..settled]);
            unread.drain(..settled);
        }
    };
    tokio::pin!(marking);
    tokio::select! {
        // A mark that cannot be written ends nothing; what was drained stands.
        Err(_) = &mut marking => {}
        () = reading => {}
    }
}

/// Start `program`, found on the search path as `<program>.exe`, with `arguments`,
/// this process's environment and `environment`'s variables over it, in a process tree
/// of its own and, with a `console`, attached to it.
pub(in crate::origin::ssh) fn spawn(
    program: &str,
    arguments: &[OsString],
    environment: &ProcessEnvironment,
    console: Option<&Console>,
) -> io::Result<Spawned> {
    let (input, stdin) = to_child(Theirs::Overlapped)?;
    let (output, stdout) = from_child(Theirs::Overlapped)?;
    let (errors, stderr) = from_child(Theirs::Overlapped)?;
    let mark = stderr.try_clone()?;
    let created = create::Created::suspended(
        program,
        arguments,
        environment,
        [&stdin, &stdout, &stderr],
        console.map(Console::handle),
    )?;
    drop((stdin, stdout, stderr));
    let group = match ProcessGroup::start(&created.process, &created.thread) {
        Ok(group) => group,
        Err(error) => {
            // SAFETY: the process handle is open; the call takes no pointers.
            unsafe { TerminateProcess(created.process.as_raw_handle(), 1) };
            return Err(error);
        }
    };
    Ok(Spawned {
        child: Child(created.process),
        group,
        input: Box::new(input),
        output: Box::new(output),
        errors: Errors {
            pipe: errors,
            mark: Some(mark),
        },
    })
}

/// A process this module created.
pub(in crate::origin::ssh) struct Child(OwnedHandle);

impl Child {
    pub(in crate::origin::ssh) async fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        let process = self.0.try_clone()?;
        skyhook_base::blocking(move || {
            // SAFETY: the handle is open and owned by this closure.
            if unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) } != WAIT_OBJECT_0 {
                return Err(io::Error::last_os_error());
            }
            let mut code = 0;
            // SAFETY: the handle is open and `code` is a valid out pointer.
            check(unsafe { GetExitCodeProcess(process.as_raw_handle(), &raw mut code) })?;
            Ok(std::process::ExitStatus::from_raw(code))
        })
        .await
    }

    pub(in crate::origin::ssh) async fn kill(&mut self) -> io::Result<()> {
        // SAFETY: the handle is open; the call takes no pointers.
        check(unsafe { TerminateProcess(self.0.as_raw_handle(), 1) })
    }
}
