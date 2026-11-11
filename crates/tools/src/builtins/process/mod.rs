use super::{TimeoutSecs, duration};
use crate::diagnostic::PartialContext;
use crate::output::ProducedOutput;
use crate::spec::TargetTool;
use crate::{
    error::{AdmissionError, CallError},
    spec::{Call, ended},
};
use skyhook_worker::diagnostic::{Effects, Failure, Fault, Operation, Subject};
use skyhook_worker::process::CommandLine;
use skyhook_worker::{
    op,
    op::{PathKind, Pipe},
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::output::StreamEnd;
use crate::output::{FinishedOutput, TextCaptureField, ends};
use crate::path::{PathAccess, PathArgument};

mod capture;
use capture::Capture;

/// Run a command where the call runs.
pub struct RunCommand;

impl TargetTool for RunCommand {
    const NAME: &'static str = "exec";
    const DESCRIPTION: &'static str =
        "Run a command. Stdin is closed. A nonzero exit code is a normal result";
    type Input = ExecArgs;
    type Admitted = Exec;
    type Op = op::Exec;
    type Rendering = ();

    fn result_schema() -> Option<serde_json::Value> {
        Some(crate::schema::result_schema::<ProcessOutput>())
    }

    fn admit(args: ExecArgs) -> Result<Exec, AdmissionError> {
        Ok(Exec {
            program: program(&args.command)?,
            cwd: args.cwd,
            timeout: args.timeout,
        })
    }

    fn paths(exec: &mut Exec) -> Vec<PathArgument<'_>> {
        let kind = PathKind::WorkingDirectory;
        vec![PathArgument::new(&mut exec.cwd, PathAccess::Read, kind)]
    }

    fn op(exec: &mut Exec, call: &Call) -> Result<op::Exec, CallError> {
        if call.is_cancelled() {
            return Err(
                CallError::cancelled_before_start().operation(Operation::Spawn, Subject::Process)
            );
        }
        let spills = call.spills();
        Ok(op::Exec {
            program: exec.program.clone(),
            cwd: exec.cwd.clone(),
            env: call.process_environment.clone(),
            timeout: exec.timeout.map(duration),
            spill: op::StreamSpill {
                inline: spills.inline,
                tail: spills.tail,
                live: spills.live,
                stdout: spills.name("stdout"),
                stderr: spills.name("stderr"),
            },
        })
    }

    fn rendering(_: Exec) {}

    /// Wait until the process exits, the call is cancelled, or the deadline elapses.
    async fn render<U: Into<CallError> + Send + 'static>(
        (): (),
        mut running: op::Running<op::Exec, U>,
        context: &Call,
    ) -> Result<ProducedOutput, CallError> {
        // Created once the process started, as its output arrives.
        let mut captures = None::<(Capture, Capture)>;
        let unavailable = |pipe| {
            let unavailable = Fault::Pipe { pipe, error: None };
            CallError::from(Failure::from(unavailable).effects(Effects::Unknown))
        };
        let cancelled = loop {
            let event = tokio::select! {
                event = running.event() => event,
                () = context.cancelled() => break true,
            };
            match event {
                None => break false,
                Some(op::ExecEvent::Started) => {
                    let stdout = Capture::create(context, TextCaptureField::Stdout).await?;
                    let stderr = Capture::create(context, TextCaptureField::Stderr).await?;
                    captures = Some((stdout, stderr));
                }
                Some(op::ExecEvent::Output { pipe, text }) => {
                    let (stdout, stderr) = captures.as_mut().ok_or_else(|| unavailable(pipe))?;
                    match pipe {
                        Pipe::Stdout => stdout.write(&text).await?,
                        Pipe::Stderr => stderr.write(&text).await?,
                    }
                }
                Some(op::ExecEvent::Latest { pipe, extent, text }) => {
                    let (stdout, stderr) = captures.as_mut().ok_or_else(|| unavailable(pipe))?;
                    match pipe {
                        Pipe::Stdout => stdout.latest(extent, text).await?,
                        Pipe::Stderr => stderr.latest(extent, text).await?,
                    }
                }
            }
        };
        // The captures that hold output, each as its target ended its stream, and
        // why the first that lost its file did.
        let finish = async move |captures: Option<(Capture, Capture)>,
                                 rests: [op::StreamRest; 2]| {
            let Some((stdout, stderr)) = captures else {
                return Ok::<_, CallError>((Vec::new(), None));
            };
            let [out, err] = rests;
            let (stdout, lost_out) = stdout.finish(out).await?;
            let (stderr, lost_err) = stderr.finish(err).await?;
            let captures = [stdout, stderr].into_iter().flatten().collect();
            Ok((captures, lost_out.or(lost_err)))
        };
        if cancelled {
            // Dropping the run stops the process tree, and a file it was keeping a
            // stream in goes with it.
            drop(running);
            finish(captures, [op::StreamRest::None, op::StreamRest::None]).await?;
            return Err(CallError::cancelled().context(started(Operation::Wait, Subject::Process)));
        }
        let end = ended(running).await??;
        let (captures, lost) = finish(captures, [end.stdout, end.stderr]).await?;
        let (status, timed_out) = match end.status {
            op::ExecStatus::Exited(status) => (status, None),
            op::ExecStatus::TimedOut { status, after } => (status, Some(after)),
        };
        let mut output = produced(status, captures);
        let cut = match (timed_out, lost) {
            (None, None) => return Ok(output),
            (Some(after), _) => {
                let message = format!("timed out after {} seconds", after.as_secs());
                CallError::failed(message).context(started(Operation::Wait, Subject::Process))
            }
            // The command ran to its end; only its output is short.
            (None, Some(lost)) => CallError::from(lost.effects(Effects::Unknown)),
        };
        output.streams = StreamEnd::Cut;
        Err(cut.with_result(output))
    }
}

/// A command admitted to run: its program, working directory and deadline.
pub struct Exec {
    program: op::Program,
    cwd: op::TargetPath,
    timeout: Option<TimeoutSecs>,
}

/// After spawn the command may already have had effects.
fn started(operation: Operation, subject: Subject) -> PartialContext {
    PartialContext::new(operation, subject).effects(Effects::Unknown)
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecArgs {
    /// Text runs with the target's shell; an array such as ["git","status"] runs directly
    command: CommandLine,
    /// Working directory
    #[serde(default = "super::default_dot")]
    cwd: op::TargetPath,
    /// Seconds before the command is terminated; omitted/null means no deadline
    timeout: Option<TimeoutSecs>,
}

/// The program `command` runs, or why it names none.
fn program(command: &CommandLine) -> Result<op::Program, AdmissionError> {
    command.program().map_err(|invalid| {
        AdmissionError::invalid_arguments(invalid.to_string())
            .operation(Operation::Validate, Subject::argument(["command"]))
            .effects(Effects::NotStarted)
    })
}

fn produced(exit: op::ExitStatus, captures: Vec<FinishedOutput>) -> ProducedOutput {
    let output = ProcessOutput {
        exit,
        // Completed captures install these fields when bytes were observed. An
        // absent stream was therefore observed to be empty, not unknown.
        stdout: None,
        stderr: None,
    };
    ProducedOutput::serialized(output)
        .expect("process output serializes")
        .with_captures(captures)
}

#[serde_with::skip_serializing_none]
#[derive(JsonSchema, Serialize)]
struct ProcessOutput {
    exit: op::ExitStatus,
    #[schemars(with = "crate::output::JsonText", transform = ends)]
    stdout: Option<String>,
    #[schemars(transform = ends)]
    stderr: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::{assert_matches, sync::Arc, time::Duration};

    use serde_json::json;
    use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

    use super::*;
    use crate::diagnostic::{Cause, Diagnostic};
    use crate::spec::tests::{CapturedOutput, Workspace};
    use crate::tests::bounded;
    use skyhook_worker::op::IoKind;

    /// A workspace whose `exec` timeouts elapse when `sleep` does.
    fn workspace<F: Future<Output = ()> + Send + 'static>(
        sleep: impl Fn(Duration) -> F + Clone + Send + Sync + 'static,
    ) -> Workspace {
        let sleep: skyhook_worker::local::Sleep =
            Arc::new(move |duration| Box::pin(sleep(duration)));
        Workspace::new().with_clock(sleep)
    }

    /// A sleep that elapses once `deadline` is cancelled, whatever its duration.
    fn elapses(
        deadline: &CancellationToken,
    ) -> impl Fn(Duration) -> WaitForCancellationFutureOwned + Clone + Send + Sync + use<> {
        let deadline = deadline.clone();
        move |_| deadline.clone().cancelled_owned()
    }

    #[tokio::test]
    async fn exit_codes_streams_and_timeout_partial_output() {
        let deadline = CancellationToken::new();
        let workspace = workspace(elapses(&deadline));
        let shell = async |command: &str| workspace.run("exec", json!({"command":command})).await;
        // A signal death reports the signal instead of an exit code; observed
        // empty streams are absent.
        let signal = shell("kill -TERM $$").await.unwrap();
        const SIGTERM: i32 = 15;
        assert_eq!(signal, json!({"exit": {"signal": SIGTERM}}));
        for (command, expected) in [
            ("exit 0", json!({"exit": {"code": 0}})),
            (
                "printf '[1]'; printf warning >&2",
                json!({"exit": {"code": 0}, "stdout": "[1]", "stderr": "warning"}),
            ),
        ] {
            assert_eq!(shell(command).await.unwrap(), expected);
        }
        let output = shell("printf '\\377'; exit 3").await.unwrap();
        assert_eq!(output, json!({"exit": {"code": 3}, "stdout": "\u{fffd}"}));

        // The deadline elapses once the tool has captured the partial output.
        let sink = Arc::<CapturedOutput>::default();
        let command = json!({"command":"printf partial; exec sleep 3600", "timeout":3600});
        let run = workspace.run_with("exec", command, &sink, CancellationToken::new());
        let (result, ()) = tokio::join!(bounded(run), async {
            sink.written(|bytes| !bytes.is_empty()).await;
            deadline.cancel();
        });
        let (diagnostic, output) = result.unwrap_err().into_parts();
        assert_eq!(diagnostic.context.operation, Operation::Wait);
        assert_eq!(diagnostic.context.subject, Subject::Process);
        assert_eq!(diagnostic.context.effects, Effects::Unknown);
        assert_matches!(&diagnostic.cause, Cause::Message(text) if text.contains("3600"));
        const SIGKILL: i32 = 9;
        let partial = json!({"exit": {"signal": SIGKILL}, "stdout": "partial"});
        assert_eq!(output, Some(partial));
    }

    #[tokio::test]
    async fn failures_before_spawn_report_their_stage_and_that_nothing_started() {
        let workspace = workspace(tokio::time::sleep);
        std::fs::write(workspace.path().join("file-cwd"), "not a directory").unwrap();
        let (started, unclaimed) = (Effects::NotStarted, Effects::Unknown);
        for (arguments, operation, effects, argument) in [
            // A missing cwd fails path preflight; a file passes it and fails the handler.
            (
                json!({"command":"touch started", "cwd":"missing-cwd"}),
                Operation::Resolve,
                started,
                "cwd",
            ),
            (
                json!({"command":"touch started", "cwd":"file-cwd"}),
                Operation::Validate,
                started,
                "cwd",
            ),
            // The schema's bounds refuse these before the handler sees them, claiming
            // no effects.
            (
                json!({"command":""}),
                Operation::Validate,
                unclaimed,
                "command",
            ),
            (
                json!({"command":[]}),
                Operation::Validate,
                unclaimed,
                "command",
            ),
            (
                json!({"command":r#" ["touch", "started"] "#}),
                Operation::Validate,
                started,
                "command",
            ),
            (
                json!({"command":"true", "timeout":0}),
                Operation::Validate,
                unclaimed,
                "timeout",
            ),
            (
                json!({"command":"true", "timeout":3601}),
                Operation::Validate,
                unclaimed,
                "timeout",
            ),
        ] {
            let error = workspace.run("exec", arguments.clone()).await.unwrap_err();
            let Diagnostic { cause, context } = error.diagnostic();
            if argument != "cwd" {
                let Cause::InvalidArguments(message) = cause else {
                    panic!("{cause:?}")
                };
                let encoded = arguments["command"]
                    .as_str()
                    .is_some_and(|text| text.contains('['));
                assert_eq!(message.contains("as an array"), encoded, "{message}");
            }
            assert_eq!(context.operation, operation);
            assert_eq!(context.effects, effects, "{arguments}");
            match arguments["cwd"].as_str() {
                Some(cwd) => {
                    assert_matches!(&context.subject, Subject::WorkingDirectory(path) if path.as_str().ends_with(cwd),
                        "{context:?}"
                    )
                }
                None => assert_eq!(context.subject, Subject::argument([argument])),
            }
        }
        // Spawn ENOENT can also mean a missing interpreter, so no path is the subject.
        let arguments = json!({"command":["/skyhook-test-missing-executable"]});
        let diagnostic = workspace
            .run("exec", arguments)
            .await
            .unwrap_err()
            .diagnostic();
        assert_eq!(diagnostic.context.operation, Operation::Spawn);
        assert_eq!(diagnostic.context.subject, Subject::Process);
        assert_eq!(diagnostic.context.effects, Effects::NotStarted);
        assert_matches!(
            diagnostic.cause,
            Cause::Io(skyhook_worker::op::OsError {
                kind: IoKind::NotFound,
                ..
            })
        );
        assert!(!workspace.path().join("started").exists());
    }

    #[tokio::test]
    async fn cancellation_fails_the_call_as_cancelled() {
        let workspace = Workspace::new();
        let arguments = json!({"command":"printf started; exec sleep 3600"});
        let (sink, cancellation) = (Arc::<CapturedOutput>::default(), CancellationToken::new());
        let run = workspace.run_with("exec", arguments, &sink, cancellation.clone());
        let (finished, ()) = tokio::join!(bounded(run), async {
            sink.written(|bytes| !bytes.is_empty()).await;
            cancellation.cancel();
        });
        assert_eq!(finished.unwrap_err().diagnostic().cause, Cause::Cancelled);
    }

    /// Run `command` in `spilling`, with the sink its captures went to.
    async fn produce(
        spilling: &Workspace,
        command: &str,
        cancellation: &CancellationToken,
    ) -> (Result<ProducedOutput, CallError>, Arc<CapturedOutput>) {
        let sink = Arc::<CapturedOutput>::default();
        let arguments = json!({"command":command, "timeout":3600});
        let produced = spilling.produce("exec", arguments, &sink, cancellation.clone());
        (bounded(produced).await, sink)
    }

    /// Once a stream's spill file in `spilling`, staged or whole, holds `bytes`.
    async fn spilled(spilling: &Workspace, bytes: u64) {
        let directory = spilling.path().join("spills/session");
        let holds = || {
            (std::fs::read_dir(&directory).into_iter().flatten())
                .flatten()
                .any(|entry| entry.metadata().is_ok_and(|file| file.len() == bytes))
        };
        bounded(async {
            while !holds() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
    }

    /// A stream within the limit is sent whole. Past it the target keeps the whole
    /// stream in a file of its own: the bytes up to the limit, cut at a character,
    /// are all that was sent, and the file's counts and last bytes come with its end.
    #[tokio::test]
    async fn a_stream_over_the_limit_is_kept_in_a_file_on_its_target() {
        use crate::output::CaptureFile;
        let spilling = workspace(tokio::time::sleep).spilling_past(8);
        let never = CancellationToken::new();
        let (within, sink) = produce(&spilling, "printf 12345678", &never).await;
        assert!(within.unwrap().captures[0].file().is_none());
        assert_eq!(sink.bytes(), b"12345678");
        assert!(spilling.spilled().is_empty());

        // The head's last character would pass the limit, so it is not sent.
        let command = "printf 'one\\ntwo\\303\\251\\nthree\\n'; printf e >&2";
        let (over, sink) = produce(&spilling, command, &never).await;
        let over = over.unwrap();
        let [file] = &spilling.spilled()[..] else {
            panic!("one spill file")
        };
        let text = "one\ntwo\u{e9}\nthree\n";
        assert_eq!(std::fs::read_to_string(file).unwrap(), text);
        assert_eq!(sink.bytes(), b"one\ntwoe");
        let Some(CaptureFile::Written(stream)) = over.captures[0].file() else {
            panic!("stdout is in a file")
        };
        assert_eq!(stream.path, crate::tests::text(file));
        let op::Extent {
            bytes,
            newlines,
            ends_line,
        } = stream.extent;
        let counted = (bytes, newlines, ends_line);
        assert_eq!(counted, (text.len() as u64, 3, true));
        assert_eq!(stream.tail, text.as_bytes());
        assert!(over.captures[1].file().is_none());
    }

    /// A deadline keeps the file with what was written. While a command runs, its
    /// target tells of the latest of a stream it is keeping, at intervals and as the
    /// stream ends; a cancelled run leaves no file. A file that cannot be kept fails
    /// the call for the stream it held, with the output that was sent.
    #[tokio::test]
    async fn a_stream_spill_survives_a_deadline_and_not_a_cancellation_or_a_lost_directory() {
        let deadline = CancellationToken::new();
        let timed = workspace(elapses(&deadline)).spilling_past(8);
        let command = "printf 123456789; exec sleep 3600";
        let never = CancellationToken::new();
        let ((timed_out, _), ()) = tokio::join!(produce(&timed, command, &never), async {
            spilled(&timed, 9).await;
            deadline.cancel();
        });
        let (_, output) = timed_out.unwrap_err().into_parts();
        let [file] = &timed.spilled()[..] else {
            panic!("one spill file")
        };
        assert_eq!(std::fs::read(file).unwrap(), b"123456789");
        assert!(output.unwrap().captures[0].file().is_some());

        // No interval elapses for the stream that ends while its command runs on.
        let ending = "printf 123456789; exec >&-; exec sleep 3600";
        for (cancelling, command) in [
            (workspace(tokio::time::sleep), command),
            (workspace(elapses(&never)), ending),
        ] {
            let cancelling = cancelling.spilling_past(8);
            let (cancellation, sink) = (CancellationToken::new(), Arc::<CapturedOutput>::default());
            let arguments = json!({"command": command, "timeout": 3600});
            let run = cancelling.produce("exec", arguments, &sink, cancellation.clone());
            let (cancelled, ()) = tokio::join!(bounded(run), async {
                let (extent, latest) = sink.latest(|(extent, _)| extent.bytes == 9).await;
                assert_eq!((extent.lines(), latest.as_str()), (1, "123456789"));
                cancellation.cancel();
            });
            assert_eq!(cancelled.unwrap_err().diagnostic().cause, Cause::Cancelled);
            assert_eq!(sink.bytes(), b"12345678");
            let directory = cancelling.path().join("spills/session");
            // The stopped command's target removes what it staged as it drops it.
            bounded(async {
                while std::fs::read_dir(&directory).unwrap().count() > 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await;
        }

        let unwritable = workspace(tokio::time::sleep).spilling_past(8);
        std::fs::write(unwritable.path().join("spills"), b"").unwrap();
        let (lost, sink) = produce(&unwritable, "printf 123456789", &never).await;
        let (diagnostic, output) = lost.unwrap_err().into_parts();
        assert_eq!(
            diagnostic.context.subject,
            Subject::Label("/result/stdout".into())
        );
        assert_eq!(sink.bytes(), b"12345678");
        assert_eq!(output.unwrap().streams, StreamEnd::Cut);
    }

    #[tokio::test]
    async fn capture_and_pipe_io_failures_keep_distinct_phases() {
        use crate::{
            output::{
                OutputContext,
                capture::tests::{CaptureStage, FailingCapture},
            },
            spec::tests::{CapturedOutput, call},
        };
        use std::io;

        fn broken() -> io::Error {
            io::Error::new(io::ErrorKind::BrokenPipe, "private details")
        }
        let opaque = Cause::Io(skyhook_worker::op::OsError {
            kind: IoKind::BrokenPipe,
            code: None,
            detail: None,
        });
        let root = tempfile::tempdir().unwrap();
        for (stage, operation) in [
            (CaptureStage::Open, Operation::CreateCapture),
            (CaptureStage::Write, Operation::WriteCapture),
            (CaptureStage::Finish, Operation::FinishCapture),
        ] {
            let producer = OutputContext::new(Arc::new(FailingCapture::new(
                Arc::new(CapturedOutput::default()),
                stage,
                0,
                broken,
            )));
            let context = call(root.path(), producer.clone(), Default::default());
            let captured = async {
                let mut capture = Capture::create(&context, TextCaptureField::Stdout).await?;
                capture.write("output").await?;
                capture.finish(op::StreamRest::None).await
            };
            let error = bounded(captured).await.err().unwrap();
            let diagnostic = error.diagnostic();
            assert_eq!(diagnostic.context.operation, operation);
            assert_eq!(
                diagnostic.context.subject,
                Subject::Label(TextCaptureField::Stdout.pointer().into())
            );
            assert_eq!(diagnostic.context.effects, Effects::Unknown);
            assert_eq!(diagnostic.cause, opaque);
            bounded(producer.settle()).await.unwrap();
        }

        let error = Some(op::OsError::from(broken()));
        let read = Fault::Pipe {
            pipe: Pipe::Stderr,
            error,
        };
        let diagnostic = CallError::from(Failure::from(read)).diagnostic();
        assert_eq!(diagnostic.context.operation, Operation::Read);
        assert_eq!(
            diagnostic.context.subject,
            Subject::Label("stderr pipe".into())
        );
        assert_eq!(diagnostic.cause, opaque);
    }
}
