//! Running a process on a target with its shell, streaming its output back.
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::TargetPath;
use crate::fs::spill::SpillName;

/// What a process runs.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Program {
    /// Shell text, run with the target's shell: `/bin/sh -lc` on unix, Windows
    /// PowerShell 5.1 on Windows.
    Shell(String),
    /// A program and its arguments, run directly.
    Argv { program: String, args: Vec<String> },
}

/// Run `program` in `cwd` with stdin closed until it exits or `timeout` elapses,
/// then stop its whole process tree.
#[derive(Debug, Deserialize, Serialize)]
pub struct Exec {
    pub program: Program,
    pub cwd: TargetPath,
    /// Variables set over the target's own environment.
    pub env: std::collections::BTreeMap<String, String>,
    pub timeout: Option<Duration>,
    pub spill: StreamSpill,
}

/// What becomes of a stream longer than is sent back: the target keeps all of it in
/// a file in its spill directory, and reports the bytes it ends with.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StreamSpill {
    /// The bytes of each stream sent as events.
    pub inline: u64,
    /// The most bytes of a kept stream's end reported with it.
    pub tail: u64,
    /// How often at most a stream being kept reports its latest bytes while it runs.
    pub live: Duration,
    pub stdout: SpillName,
    pub stderr: SpillName,
}

#[cfg(test)]
impl StreamSpill {
    /// Streams sent whole, however long.
    pub(crate) fn never() -> Self {
        Self {
            inline: u64::MAX,
            tail: 0,
            live: Duration::ZERO,
            stdout: SpillName::unique(&"test", "stdout"),
            stderr: SpillName::unique(&"test", "stderr"),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub enum ExecEvent {
    Started,
    /// A stream's next text: its bytes as UTF-8, invalid ones replaced. Only a
    /// stream's first [`StreamSpill::inline`] bytes are sent.
    Output {
        pipe: Pipe,
        text: String,
    },
    /// The latest of a stream being kept in a file: what was counted of it so far, and
    /// its last bytes, at most [`StreamSpill::tail`] of them. Each replaces the one
    /// before, and one is sent only when the one before was taken.
    Latest {
        pipe: Pipe,
        extent: Extent,
        text: String,
    },
}

/// How much text was counted: its length, its newlines, and whether it ends a line.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Extent {
    pub bytes: u64,
    pub newlines: u64,
    pub ends_line: bool,
}

impl Extent {
    /// The lines the bytes hold, one they do not end included.
    pub fn lines(self) -> usize {
        usize::try_from(self.newlines).unwrap_or(usize::MAX)
            + usize::from(self.bytes > 0 && !self.ends_line)
    }

    /// The extent once `data` follows.
    #[must_use]
    pub fn after(self, data: &[u8]) -> Self {
        let newlines = data.iter().filter(|&&byte| byte == b'\n').count() as u64;
        Self {
            bytes: self.bytes + data.len() as u64,
            newlines: self.newlines + newlines,
            ends_line: data.last().map_or(self.ends_line, |&byte| byte == b'\n'),
        }
    }
}

/// A stream its target kept whole as text in a file of its spill directory: what it
/// counted of it, and its last bytes.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SpilledStream {
    pub path: TargetPath,
    pub extent: Extent,
    #[serde(with = "serde_bytes")]
    pub tail: Vec<u8>,
}

/// What became of a stream past the bytes sent as events.
#[derive(Debug, Deserialize, Serialize)]
pub enum StreamRest {
    /// It had none.
    None,
    /// Kept in a file.
    Spilled(SpilledStream),
    /// Its file could not be kept, and the rest was discarded.
    Lost(crate::diagnostic::Fault),
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Pipe {
    Stdout,
    Stderr,
}

/// How a terminated process ended: with an exit code (Windows' 32-bit codes as
/// `i32`), or by a signal, which only unix has; spelled as a tool's result shows it.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, schemars::JsonSchema, PartialEq, Eq)]
pub enum ExitStatus {
    #[serde(rename = "code")]
    Exited(i32),
    #[serde(rename = "signal")]
    Terminated(i32),
}

impl From<std::process::ExitStatus> for ExitStatus {
    fn from(status: std::process::ExitStatus) -> Self {
        #[cfg(unix)]
        if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
            return Self::Terminated(signal);
        }
        Self::Exited(status.code().unwrap_or_default())
    }
}

impl std::fmt::Display for ExitStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exited(code) => write!(formatter, "exit status: {code}"),
            Self::Terminated(signal) => write!(formatter, "signal: {signal}"),
        }
    }
}

/// How a process ended, and what became of the output not sent as events.
#[derive(Debug, Deserialize, Serialize)]
pub struct ExecEnd {
    pub status: ExecStatus,
    pub stdout: StreamRest,
    pub stderr: StreamRest,
}

#[derive(Debug, Deserialize, Serialize)]
pub enum ExecStatus {
    Exited(ExitStatus),
    /// The timeout `after` elapsed and the process tree was stopped.
    TimedOut {
        status: ExitStatus,
        after: Duration,
    },
}
