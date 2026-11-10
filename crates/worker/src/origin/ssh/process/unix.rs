//! An SSH process on unix, leading a process group, and with a terminal its session.
use std::{ffi::OsString, io, os::fd::AsFd as _, process::Stdio};

use super::{Diagnostics, Spawned};
use crate::{
    origin::ProcessEnvironment,
    process::{Console, ProcessGroup},
};

pub(in crate::origin::ssh) type Child = tokio::process::Child;
pub(in crate::origin::ssh) type Errors = tokio::process::ChildStderr;

/// Keep what a finished process wrote to `errors`: all of it is already in the pipe,
/// read without waiting for other writers.
pub(super) async fn written(errors: &mut Errors, diagnostics: &mut Diagnostics) {
    // A duplicate shares the pipe's non-blocking mode, which the runtime sets.
    let Ok(pipe) = errors.as_fd().try_clone_to_owned() else {
        return;
    };
    let mut pipe = std::fs::File::from(pipe);
    let mut buffer = [0; 1024];
    loop {
        match io::Read::read(&mut pipe, &mut buffer) {
            Ok(0) => return,
            Ok(read) => diagnostics.keep(&buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            // WouldBlock: the pipe holds nothing more.
            Err(_) => return,
        }
    }
}

/// Start `program` with `arguments` and `environment`'s variables over this process's,
/// leading a process group of its own and, with a `console`, its session there.
pub(in crate::origin::ssh) fn spawn(
    program: &str,
    arguments: &[OsString],
    environment: &ProcessEnvironment,
    console: Option<&Console>,
) -> io::Result<Spawned> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(arguments)
        .envs(environment)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match console {
        Some(console) => console.control(&mut command),
        None => {
            command.process_group(0);
        }
    }
    let (mut child, group) = ProcessGroup::spawn(&mut command)?;
    let input = child.stdin.take().expect("stdin was requested as a pipe");
    let output = child.stdout.take().expect("stdout was requested as a pipe");
    let errors = child.stderr.take().expect("stderr was requested as a pipe");
    Ok(Spawned {
        child,
        group,
        input: Box::new(input),
        output: Box::new(output),
        errors,
    })
}
