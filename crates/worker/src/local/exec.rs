//! Running a process on this machine until it exits or its deadline elapses.
use std::process::Stdio;

use tokio::io::{AsyncRead, AsyncReadExt as _};

use crate::diagnostic::{Effects, Failure, Fault, Operation, Subject};
use crate::fs::spill::SpillName;
use crate::fs::{CommitMode, PermissionPolicy, StagedWriter};
use crate::op::{
    Events, Exec, ExecEnd, ExecEvent, ExecStatus, Extent, OsError, Pipe, SpilledStream, StreamRest,
};
use crate::process::{ProcessGroup, Shell};

const PROCESS_CHUNK: usize = 8 * 1024;

impl crate::op::Operation for Exec {
    type Event = ExecEvent;
    type End = ExecEnd;
    type Error = Failure;

    fn paths(&mut self) -> Vec<&mut crate::op::TargetPath> {
        vec![&mut self.cwd]
    }

    async fn perform(
        self,
        local: &super::Local,
        events: Events<ExecEvent>,
    ) -> Result<ExecEnd, Failure> {
        let exec = self;
        let not_started = |operation, subject, fault: Fault| {
            Failure::at(operation, subject, fault).effects(Effects::NotStarted)
        };
        let directory = || Subject::working_directory(&exec.cwd);
        let cwd = exec.cwd.native();
        let inspected = cwd.to_owned();
        let metadata = skyhook_base::blocking(move || std::fs::metadata(inspected)).await;
        let metadata =
            metadata.map_err(|error| not_started(Operation::Inspect, directory(), error.into()))?;
        if !metadata.is_dir() {
            let fault = Fault::NotDirectory;
            return Err(not_started(Operation::Validate, directory(), fault));
        }
        let mut command = exec.program.command(Shell::Login);
        command
            .current_dir(cwd)
            .envs(&exec.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Redirected stdio still permits /dev/tty access, which stops a child outside
        // the terminal's foreground group (SIGTTIN/SIGTTOU) even when interactive.
        // A new session detaches the controlling terminal and also makes the child
        // its process-group leader, preserving group-wide cleanup below.
        // SAFETY: setsid is async-signal-safe and the hook does not allocate or
        // access shared state between fork and exec.
        #[cfg(unix)]
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // Spawn can fail while setting up the child or loading its interpreter. In
        // particular, ENOENT does not establish that the executable itself is absent.
        let (mut child, mut group) = ProcessGroup::spawn(&mut command)
            .map_err(|error| not_started(Operation::Spawn, Subject::Process, error.into()))?;
        let stdout = child.stdout.take().expect("stdout was requested as a pipe");
        let stderr = child.stderr.take().expect("stderr was requested as a pipe");
        let _ = events.send(ExecEvent::Started).await;
        let deadline = async {
            match exec.timeout {
                Some(timeout) => {
                    local.sleep(timeout).await;
                    timeout
                }
                None => std::future::pending().await,
            }
        };
        let (inline, tail, live) = (exec.spill.inline, exec.spill.tail, exec.spill.live);
        let stream = |pipe, name| Stream {
            pipe,
            inline,
            tail,
            name,
            local,
            events: &events,
            decoder: LossyText::default(),
            output: Output::Sending(Vec::new()),
            live,
            due: local.sleep(live),
            reported: Extent::default(),
        };
        let mut out = stream(Pipe::Stdout, exec.spill.stdout);
        let mut err = stream(Pipe::Stderr, exec.spill.stderr);
        // Keep the deadline active while descendants hold output pipes open.
        let execution = async {
            let (status, (), ()) = tokio::try_join!(
                async { child.wait().await.map_err(started(Operation::Wait)) },
                out.forward(stdout),
                err.forward(stderr),
            )?;
            Ok::<_, Failure>(status)
        };
        let exited = {
            tokio::pin!(execution);
            tokio::select! {
                status = &mut execution => Ok(status?),
                after = deadline => Err(after),
            }
        };
        let status = match exited {
            Ok(status) => ExecStatus::Exited(status.into()),
            Err(after) => {
                group.kill();
                child.kill().await.map_err(started(Operation::Terminate))?;
                let status = child.wait().await.map_err(started(Operation::Wait))?;
                let status = status.into();
                ExecStatus::TimedOut { status, after }
            }
        };
        Ok(ExecEnd {
            status,
            stdout: out.finish().await,
            stderr: err.finish().await,
        })
    }
}

/// After spawn the command may already have had effects.
fn started(operation: Operation) -> impl Fn(std::io::Error) -> Failure {
    move |error| Failure::at(operation, Subject::Process, error).effects(Effects::Unknown)
}

/// Reading `pipe` failed with `error`; only the tool names a pipe.
fn pipe(pipe: Pipe, error: std::io::Error) -> Failure {
    let error = Some(error.into());
    Failure::from(Fault::Pipe { pipe, error }).effects(Effects::Unknown)
}

/// One pipe's output as text: sent as events while it is within `inline` bytes, then
/// kept whole in the spill file `name`.
struct Stream<'a> {
    pipe: Pipe,
    inline: u64,
    tail: u64,
    name: SpillName,
    local: &'a super::Local,
    events: &'a Events<ExecEvent>,
    decoder: LossyText,
    output: Output,
    /// How often at most the latest of a stream being kept is reported, when the
    /// next report is due, and how much of the stream the last one told of.
    live: std::time::Duration,
    due: futures_util::future::BoxFuture<'static, ()>,
    reported: Extent,
}

/// Where a stream's text goes.
enum Output {
    /// To its events, with what was sent kept for the file a longer stream needs.
    Sending(Vec<u8>),
    Spilling(Spill),
    /// Nowhere: its file could not be kept.
    Lost(StreamRest),
}

impl Stream<'_> {
    /// Take `stream`'s output until it ends.
    async fn forward(&mut self, mut stream: impl AsyncRead + Unpin) -> Result<(), Failure> {
        let mut buffer = vec![0_u8; PROCESS_CHUNK];
        loop {
            let unreported = self.unreported().is_some();
            let read = tokio::select! {
                read = stream.read(&mut buffer) => read,
                () = &mut self.due, if unreported => {
                    self.report();
                    continue;
                }
            };
            let read = read.map_err(|error| pipe(self.pipe, error))?;
            if read == 0 {
                // Nothing follows an ended stream for its reader to hold up, so its
                // latest waits to be taken; the command may run on long after.
                if let Some(spill) = self.unreported() {
                    let _ = self.events.send(self.latest(spill)).await;
                }
                return Ok(());
            }
            let text = self.decoder.push(&buffer[..read]);
            self.take(&text).await;
        }
    }

    async fn take(&mut self, text: &str) {
        let sent = match &mut self.output {
            Output::Sending(sent) => sent,
            Output::Spilling(spill) => {
                if let Err(lost) = spill.write(text.as_bytes(), self.tail).await {
                    self.output = Output::Lost(lost);
                }
                return;
            }
            Output::Lost(_) => return,
        };
        let room = usize::try_from(self.inline.saturating_sub(sent.len() as u64));
        let (head, rest) = text.split_at(text.floor_char_boundary(room.unwrap_or(usize::MAX)));
        if !head.is_empty() {
            sent.extend_from_slice(head.as_bytes());
            let (pipe, text) = (self.pipe, head.to_owned());
            // Nobody reads the output any more: the call is being dropped.
            if (self.events.send(ExecEvent::Output { pipe, text }).await).is_err() {
                return std::future::pending().await;
            }
        }
        if rest.is_empty() {
            return;
        }
        let sent = std::mem::take(sent);
        let spilling = async {
            let mut spill = Spill::start(self.local, self.name.clone()).await?;
            spill.write(&sent, self.tail).await?;
            spill.write(rest.as_bytes(), self.tail).await?;
            Ok(spill)
        };
        self.output = match spilling.await {
            Ok(spill) => Output::Spilling(spill),
            Err(lost) => Output::Lost(lost),
        };
    }

    /// The stream being kept, when it holds more than the last report told of.
    fn unreported(&self) -> Option<&Spill> {
        match &self.output {
            Output::Spilling(spill) if spill.extent != self.reported => Some(spill),
            _ => None,
        }
    }

    /// The latest of the stream `spill` keeps.
    fn latest(&self, spill: &Spill) -> ExecEvent {
        let (pipe, extent) = (self.pipe, spill.extent);
        let text = String::from_utf8_lossy(spill.end(self.tail)).into_owned();
        ExecEvent::Latest { pipe, extent, text }
    }

    /// Tell of the latest of the stream being kept, unless the last report still
    /// waits to be taken: a stream that goes on never waits for its reader.
    fn report(&mut self) {
        if let Some(spill) = self.unreported() {
            let extent = spill.extent;
            if self.events.offer(self.latest(spill)) {
                self.reported = extent;
            }
        }
        self.due = self.local.sleep(self.live);
    }

    /// What became of the stream past its events, its file committed whole.
    async fn finish(mut self) -> StreamRest {
        let rest = std::mem::take(&mut self.decoder).finish();
        self.take(&rest).await;
        match self.output {
            Output::Sending(_) => StreamRest::None,
            Output::Spilling(spill) => spill.commit(self.tail).await,
            Output::Lost(lost) => lost,
        }
    }
}

/// A stream being kept in a staged file, which dropping it removes.
struct Spill {
    staged: StagedWriter,
    path: crate::op::TargetPath,
    extent: Extent,
    /// Its last bytes, trimmed to those reported once twice as many are held.
    tail: Vec<u8>,
}

impl Spill {
    async fn start(local: &super::Local, name: SpillName) -> Result<Self, StreamRest> {
        let local = local.clone();
        let placed = skyhook_base::blocking(move || local.spill(&name)).await;
        let path = (placed.map_err(|failed| StreamRest::Lost(failed.fault()))?).path;
        let staged = StagedWriter::create(path.native(), PermissionPolicy::Private).await;
        Ok(Self {
            staged: staged.map_err(lost)?,
            path,
            extent: Extent::default(),
            tail: Vec::new(),
        })
    }

    /// Count `text` as its file takes it, so the two agree whenever this is dropped.
    async fn write(&mut self, text: &[u8], tail: u64) -> Result<(), StreamRest> {
        self.extent = self.extent.after(text);
        let tail = usize::try_from(tail).unwrap_or(usize::MAX);
        self.tail.extend_from_slice(text);
        if self.tail.len() > tail.saturating_mul(2) {
            self.tail.drain(..self.tail.len() - tail);
        }
        let written = self.staged.write(text).await;
        written.map_err(|error| lost(error.into()))
    }

    /// The stream's last bytes, at most `tail` of them, from where a character
    /// starts: text.
    fn end(&self, tail: u64) -> &[u8] {
        let end = &self.tail;
        let start = end
            .len()
            .saturating_sub(usize::try_from(tail).unwrap_or(usize::MAX));
        let start = (start..end.len())
            .find(|&index| end[index] & 0xc0 != 0x80)
            .unwrap_or(end.len());
        &end[start..]
    }

    /// Publish the file, never over another, and report the stream.
    async fn commit(self, tail: u64) -> StreamRest {
        let end = self.end(tail).to_vec();
        match self.staged.commit(CommitMode::NoClobber).await {
            Err(failed) => lost(failed.error),
            Ok(()) => StreamRest::Spilled(SpilledStream {
                path: self.path,
                extent: self.extent,
                tail: end,
            }),
        }
    }
}

/// A stream's file could not be kept, as `error` classifies it.
fn lost(error: OsError) -> StreamRest {
    StreamRest::Lost(Fault::Os(OsError {
        detail: None,
        ..error
    }))
}

/// A byte stream decoded to text as it arrives: an invalid sequence becomes U+FFFD,
/// and one a read cut short waits for its rest.
#[derive(Default)]
struct LossyText(Vec<u8>);

impl LossyText {
    fn push(&mut self, bytes: &[u8]) -> String {
        let pending = &mut self.0;
        pending.extend_from_slice(bytes);
        let mut text = String::new();
        let mut consumed = 0;
        while consumed < pending.len() {
            match std::str::from_utf8(&pending[consumed..]) {
                Ok(valid) => {
                    text.push_str(valid);
                    consumed = pending.len();
                }
                Err(error) => {
                    let end = consumed + error.valid_up_to();
                    text.push_str(&String::from_utf8_lossy(&pending[consumed..end]));
                    consumed = end;
                    match error.error_len() {
                        Some(length) => {
                            text.push('\u{fffd}');
                            consumed += length;
                        }
                        None => break,
                    }
                }
            }
        }
        pending.drain(..consumed);
        text
    }

    /// The text of a sequence the stream's end cut short.
    fn finish(self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_matches_lossy_utf8_at_every_chunk_boundary() {
        let corpus: &[&[u8]] = &[
            b"",
            b"plain\ntext",
            "aé€🦀z".as_bytes(),
            b"\xff\xc0\xaf\xed\xa0\x80\xf4\x90\x80\x80",
            b"a\xe2x\x82b\xf0\x9f",
            b"\xc2",
            b"\xe2\x82",
            b"\xf0\x9f\xa6",
        ];
        let decoded = |chunks: &[&[u8]]| {
            let mut decoder = LossyText::default();
            let text: String = chunks.iter().map(|chunk| decoder.push(chunk)).collect();
            text + &decoder.finish()
        };
        for bytes in corpus {
            let expected = String::from_utf8_lossy(bytes);
            for split in 0..=bytes.len() {
                let (head, tail) = bytes.split_at(split);
                assert_eq!(
                    decoded(&[head, tail]),
                    expected,
                    "split {split} of {bytes:?}"
                );
            }
            let single: Vec<&[u8]> = bytes.chunks(1).collect();
            assert_eq!(decoded(&single), expected);
        }
    }

    /// A deadline drops a stream's reader while its file is taking text. The stream
    /// ends once that write has: kept with the text counted in it, or lost as the
    /// write failed.
    #[tokio::test]
    async fn a_stream_dropped_while_its_file_takes_text_ends_as_that_write_does() {
        use crate::tests::bounded;
        // More than a pipe holds: its write waits for the pipe's reader.
        let text = "line\n".repeat(1 << 18);
        for kept in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let local = crate::local::Local::new(root.path().to_owned());
            let (events, _events) = tokio::sync::mpsc::channel(1);
            let (reader, writer) = std::io::pipe().unwrap();
            let file = std::fs::File::from(std::os::fd::OwnedFd::from(writer));
            let path = root.path().join("stdout");
            let mut stream = Stream {
                pipe: Pipe::Stdout,
                inline: 0,
                tail: 10,
                name: SpillName::unique(&"session", "stdout"),
                local: &local,
                events: &Events::new(events),
                decoder: LossyText::default(),
                output: Output::Spilling(Spill {
                    staged: StagedWriter::writing_to(file, &path),
                    path: crate::op::TargetPath::new(path.to_str().unwrap()),
                    extent: Extent::default(),
                    tail: Vec::new(),
                }),
                live: std::time::Duration::MAX,
                due: Box::pin(std::future::pending()),
                reported: Extent::default(),
            };
            {
                let taking = stream.take(&text);
                tokio::pin!(taking);
                assert!(futures_util::poll!(taking).is_pending());
            }
            if kept {
                let read = std::thread::spawn(move || std::io::read_to_string(reader));
                let StreamRest::Spilled(spilled) = bounded(stream.finish()).await else {
                    panic!("the stream is kept")
                };
                assert_eq!(read.join().unwrap().unwrap(), text);
                assert_eq!(spilled.extent, Extent::default().after(text.as_bytes()));
                assert_eq!(spilled.tail, b"line\nline\n");
            } else {
                drop(reader);
                let StreamRest::Lost(Fault::Os(error)) = bounded(stream.finish()).await else {
                    panic!("the stream is lost")
                };
                assert_eq!(error.kind, crate::op::IoKind::BrokenPipe);
            }
        }
    }
}
