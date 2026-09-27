use super::TimeoutSecs;
#[cfg(unix)]
use crate::process_group::ProcessGroup;
use crate::tool::ToolOptions;
use crate::tool::diagnostic::{Effects, Operation, PartialContext, Subject};
use crate::tool::invocation::{AdmissionError, LocalCatalogBuilder, LocalContext, LocalError};
use crate::tool::output::ProducedOutput;
use crate::tool::registry::Invocation;
use std::process::{ExitStatus, Stdio};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _},
    process::Command,
};

use crate::tool::StreamEnd;
use crate::tool::output::{FinishedOutput, TextCaptureField};
use crate::tool::{
    PathArgument, PathKind, RegistryError,
    policy::{Capability, PathAccess},
};

mod capture;
use capture::Capture;

const PROCESS_CHUNK: usize = 8 * 1024;

/// Register `exec`, whose `timeout` elapses when `sleep` does.
pub(super) fn register<F: Future<Output = ()> + Send + 'static>(
    builder: &mut LocalCatalogBuilder,
    sleep: impl Fn(std::time::Duration) -> F + Clone + Send + Sync + 'static,
) -> Result<(), RegistryError> {
    builder.register_checked(
        super::names::EXEC,
        "Run a command. Stdin is closed.",
        ToolOptions::new(vec![Capability::Exec])
            .result::<ProcessOutput>()
            .placement(crate::tool::ToolPlacement::TargetedWorkspace)
            .target_authentication()
            .named()
            .background()
            .argument_paths(|exec: &mut Exec| {
                let kind = PathKind::WorkingDirectory;
                vec![PathArgument::new(&mut exec.cwd, PathAccess::Read, kind)]
            }),
        |args: ExecArgs| {
            Ok(Exec {
                command: args.command.process()?,
                cwd: args.cwd,
                timeout: args.timeout,
            })
        },
        move |exec| {
            let sleep = sleep.clone();
            Invocation::new(|context| exec.run(context, sleep))
        },
    )?;
    Ok(())
}

/// A command admitted to run: its process, working directory and deadline.
struct Exec {
    command: Command,
    cwd: String,
    timeout: Option<TimeoutSecs>,
}

impl Exec {
    async fn run<F: Future<Output = ()>>(
        mut self,
        context: LocalContext,
        sleep: impl FnOnce(std::time::Duration) -> F,
    ) -> Result<ProducedOutput, LocalError> {
        self.command
            .current_dir(working_directory(&self.cwd).await?);
        let timeout = self.timeout;
        let deadline = async move {
            match timeout {
                Some(timeout) => {
                    sleep(timeout.duration()).await;
                    timeout
                }
                None => std::future::pending().await,
            }
        };
        run_process(context, self.command, deadline)
            .await
            .map(ProcessResult::into_output)
    }
}

async fn working_directory(cwd: &str) -> Result<&std::path::Path, LocalError> {
    let path = std::path::Path::new(cwd);
    let metadata = tokio::fs::metadata(path).await.map_err(|error| {
        LocalError::io(error)
            .operation(Operation::Inspect, Subject::working_directory(path))
            .effects(Effects::NotStarted)
    })?;
    if !metadata.is_dir() {
        return Err(LocalError::failed("not a directory")
            .operation(Operation::Validate, Subject::working_directory(path))
            .effects(Effects::NotStarted));
    }
    Ok(path)
}

/// Run `command` until it exits, the call is cancelled, or `deadline` elapses.
async fn run_process(
    context: LocalContext,
    mut command: Command,
    deadline: impl Future<Output = TimeoutSecs>,
) -> Result<ProcessResult, LocalError> {
    // Override inherited/provider askpass settings even without Targets. Keep the
    // rejecting broker alive until the command and its cleanup have completed.
    let rejecting_askpass = if context.capabilities().contains(Capability::Interactive) {
        None
    } else {
        Some(
            crate::remote::AskpassServer::start(
                std::sync::Arc::new(crate::remote::RejectSensitivePrompts),
                None,
            )
            .map_err(|error| {
                LocalError::io(error)
                    .operation(
                        Operation::Prepare,
                        Subject::Label("noninteractive askpass".to_owned()),
                    )
                    .effects(Effects::NotStarted)
            })?,
        )
    };
    command.envs(&context.process_environment);
    if let Some(askpass) = &rejecting_askpass {
        command.envs(askpass.environment());
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    if context.capabilities().contains(Capability::Interactive) {
        command.process_group(0);
    } else {
        // Redirected stdio still permits /dev/tty access (and SIGTTIN stops).
        // A new session detaches the controlling terminal and also makes the
        // child its process-group leader, preserving group-wide cleanup below.
        // Do not combine this with process_group(0): a group leader cannot setsid.
        // SAFETY: setsid is async-signal-safe and the hook does not allocate or
        // access shared state between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    if context.is_cancelled() {
        return Err(LocalError::cancelled()
            .operation(Operation::Spawn, Subject::Process)
            .effects(Effects::NotStarted));
    }
    // Spawn can fail while setting up the child or loading its interpreter. In
    // particular, ENOENT does not establish that the executable itself is absent.
    let mut child = command.spawn().map_err(|error| {
        LocalError::io(error)
            .operation(Operation::Spawn, Subject::Process)
            .effects(Effects::NotStarted)
    })?;
    #[cfg(unix)]
    let mut group = ProcessGroup::led_by(&child);
    let pipe_unavailable = |pipe: &str| {
        LocalError::failed("pipe unavailable")
            .context(started(Operation::Prepare, Subject::Label(pipe.to_owned())))
    };
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| pipe_unavailable(STDOUT_PIPE))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| pipe_unavailable(STDERR_PIPE))?;

    // Keep cancellation and the deadline active while descendants hold output pipes open.
    let mut stdout_capture = Capture::create(&context, TextCaptureField::Stdout).await?;
    let mut stderr_capture = Capture::create(&context, TextCaptureField::Stderr).await?;
    let completion = {
        let execution = async {
            let (status, (), ()) = tokio::try_join!(
                async {
                    child.wait().await.map_err(LocalError::annotated(started(
                        Operation::Wait,
                        Subject::Process,
                    )))
                },
                capture_stream(stdout, &mut stdout_capture, STDOUT_PIPE),
                capture_stream(stderr, &mut stderr_capture, STDERR_PIPE),
            )?;
            Ok::<_, LocalError>(status)
        };
        tokio::pin!(execution);
        tokio::select! {
            status = &mut execution => ProcessCompletion::Exited(status?),
            timeout = deadline => ProcessCompletion::TimedOut(timeout),
            () = context.cancelled() => ProcessCompletion::Cancelled,
        }
    };
    let mut stop = async || {
        #[cfg(unix)]
        group.kill();
        child.kill().await.map_err(LocalError::annotated(started(
            Operation::Terminate,
            Subject::Process,
        )))?;
        child.wait().await.map_err(LocalError::annotated(started(
            Operation::Wait,
            Subject::Process,
        )))
    };
    let finish = async move |status: ExitStatus| {
        Ok::<_, LocalError>(ProcessResult {
            status,
            captures: [
                stdout_capture.finish().await?,
                stderr_capture.finish().await?,
            ]
            .into_iter()
            .flatten()
            .collect(),
        })
    };
    // Keep select! unbiased above. The selected variant alone controls both
    // cleanup and presentation; only expiry can carry a finite deadline.
    match completion {
        ProcessCompletion::Exited(status) => finish(status).await,
        ProcessCompletion::TimedOut(seconds) => {
            let mut output = finish(stop().await?).await?.into_output();
            output.streams = StreamEnd::Cut;
            Err(LocalError::with_output(
                format!("timed out after {} seconds", u64::from(seconds)),
                output,
            )
            .context(started(Operation::Wait, Subject::Process)))
        }
        ProcessCompletion::Cancelled => {
            finish(stop().await?).await?;
            Err(LocalError::cancelled().context(started(Operation::Wait, Subject::Process)))
        }
    }
}

const STDOUT_PIPE: &str = "stdout pipe";
const STDERR_PIPE: &str = "stderr pipe";

/// After spawn the command may already have had effects.
fn started(operation: Operation, subject: Subject) -> PartialContext {
    PartialContext::new(operation, subject).effects(Effects::Started)
}

enum ProcessCompletion {
    Exited(ExitStatus),
    TimedOut(TimeoutSecs),
    Cancelled,
}

async fn capture_stream<R>(
    mut stream: R,
    capture: &mut Capture,
    pipe: &'static str,
) -> Result<(), LocalError>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; PROCESS_CHUNK];
    loop {
        let read = stream.read(&mut buffer).await.map_err(|error| {
            LocalError::io(error)
                .context(started(Operation::Read, Subject::Label(pipe.to_owned())))
                .opaque_io()
        })?;
        if read == 0 {
            break;
        }
        capture.write_bytes(&buffer[..read]).await?;
    }
    Ok(())
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ExecArgs {
    command: CommandLine,
    /// Working directory.
    #[serde(default = "super::default_dot")]
    cwd: String,
    /// Timeout seconds; omitted/null means no deadline.
    timeout: Option<TimeoutSecs>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(untagged)]
#[schemars(inline)]
enum CommandLine {
    /// Run with /bin/sh -lc.
    Shell(String),
    /// Program and arguments without shell parsing, for example `["cargo","test"]`.
    #[schemars(extend("minItems" = 1))]
    Argv(Vec<String>),
}

impl CommandLine {
    /// The process this command line runs, or why it names none.
    fn process(&self) -> Result<Command, AdmissionError> {
        let invalid = match self {
            // A model sometimes encodes argv as a string; running it through the
            // shell would execute a bracketed word rather than the intended program.
            Self::Shell(text) if serde_json::from_str::<Vec<String>>(text).is_ok() => {
                "`command` is a JSON array encoded as a string; send `command` as an array"
            }
            Self::Shell(text) if !text.is_empty() => {
                let mut command = Command::new("/bin/sh");
                command.arg("-lc").arg(text);
                return Ok(command);
            }
            Self::Argv(argv) if !argv.is_empty() => {
                let mut command = Command::new(&argv[0]);
                command.args(&argv[1..]);
                return Ok(command);
            }
            Self::Shell(_) | Self::Argv(_) => "command cannot be empty",
        };
        Err(AdmissionError::invalid_arguments(invalid)
            .operation(Operation::Validate, Subject::argument(["command"]))
            .effects(Effects::NotStarted))
    }
}

/// Internal output retains completed field bindings until the canonical product handoff.
struct ProcessResult {
    status: ExitStatus,
    captures: Vec<FinishedOutput>,
}

impl ProcessResult {
    fn into_output(self) -> ProducedOutput {
        let output = ProcessOutput {
            exit_code: self.status.code(),
            #[cfg(unix)]
            signal: std::os::unix::process::ExitStatusExt::signal(&self.status),
            #[cfg(not(unix))]
            signal: None,
            // Completed captures install these fields when bytes were observed. An
            // absent stream was therefore observed to be empty, not unknown.
            stdout: None,
            stderr: None,
        };
        ProducedOutput::new(serde_json::to_value(output).expect("process output serializes"))
            .with_captures(self.captures)
    }
}

/// A terminated process has exactly one of an exit code or the signal that killed it.
#[serde_with::skip_serializing_none]
#[derive(JsonSchema, Serialize)]
struct ProcessOutput {
    #[schemars(with = "i32")]
    exit_code: Option<i32>,
    #[schemars(with = "i32")]
    signal: Option<i32>,
    #[schemars(with = "String", extend("x-skyhook-truncatable" = true))]
    stdout: Option<String>,
    #[schemars(with = "String", extend("x-skyhook-truncatable" = true))]
    stderr: Option<String>,
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::{
        io::Read as _,
        os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd},
        sync::Arc,
    };

    use serde_json::{Value, json};
    use std::time::Duration;

    use super::*;
    use crate::tests::bounded;
    use crate::tool::ToolRegistryBuilder;
    use crate::{
        job::{CancellationToken, JobState, output::OutputArgs},
        tests::TestRuntime,
        tool::{
            diagnostic::{Cause, Diagnostic, IoKind},
            executor::ToolExecutor,
            policy::CapabilitySet,
        },
    };

    fn executor<F: Future<Output = ()> + Send + 'static>(
        runtime: &TestRuntime,
        interactive: bool,
        sleep: impl Fn(Duration) -> F + Clone + Send + Sync + 'static,
    ) -> ToolExecutor {
        let mut builder = ToolRegistryBuilder::default();
        builder
            .register_local(|builder| register(builder, sleep))
            .unwrap();
        let mut capabilities = CapabilitySet::default();
        capabilities.remove(Capability::Targets);
        if !interactive {
            capabilities.remove(Capability::Interactive);
        }
        runtime.executor(builder).with_capabilities(capabilities)
    }

    /// Argv and shell-string `exec` arguments running the same `/bin/sh` command.
    #[cfg(unix)]
    fn sh_args(argv: bool, command: &str, timeout: Option<u64>) -> Value {
        let mut args = if argv {
            json!({"command":["/bin/sh", "-c", command]})
        } else {
            json!({"command":command})
        };
        if let Some(timeout) = timeout {
            args["timeout"] = json!(timeout);
        }
        args
    }

    /// The first stdout line of the running job, once it has produced one.
    /// Nothing announces live output, so poll for it.
    async fn live_stdout(runtime: &TestRuntime) -> Value {
        bounded(async {
            loop {
                let jobs = runtime.jobs.list(&runtime.agent).await;
                if let Some(job) = jobs.iter().find(|job| job.state == JobState::Running) {
                    let mut args = OutputArgs::new(job.id);
                    args.field = Some("/result/stdout".parse().unwrap());
                    let capabilities = Default::default();
                    let view = runtime.jobs.present_output(args, &capabilities);
                    let view = view.await.unwrap();
                    if let Some(line) = view["presentation"]["preview"]["lines"].get(0) {
                        break line.clone();
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
    }

    /// A sleep that elapses once `deadline` is cancelled, whatever its duration.
    fn elapses(
        deadline: &CancellationToken,
    ) -> impl Fn(Duration) -> tokio_util::sync::WaitForCancellationFutureOwned
    + Clone
    + Send
    + Sync
    + use<> {
        let deadline = deadline.clone();
        move |_| deadline.clone().cancelled_owned()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exit_codes_captures_and_timeout_partial_output() {
        let runtime = TestRuntime::new().await;
        let (agent, jobs) = (&runtime.agent, &runtime.jobs);
        let deadline = CancellationToken::new();
        let executor = executor(&runtime, true, elapses(&deadline));
        let shell = async |command: Value| executor.run_host(agent, "exec", command).await;
        // A signal death reports the signal instead of an exit code; observed
        // empty streams are absent.
        let signal = shell(json!({"command":"kill -TERM $$"})).await.unwrap();
        assert_eq!(signal.output.value, json!({"signal":libc::SIGTERM}));
        for (command, expected, captures) in [
            ("exit 0", json!({"exit_code":0}), json!([])),
            (
                "printf warning >&2",
                json!({"exit_code":0,"stderr":"warning"}),
                json!([]),
            ),
        ] {
            let output = shell(json!({"command":command})).await.unwrap();
            assert_eq!(output.output.value, expected);
            let args = OutputArgs::new(output.job);
            let view = jobs
                .inspect_output(args, CancellationToken::new(), &Default::default())
                .await
                .unwrap();
            assert_eq!(view["presentation"]["notice"], Value::Null);
            assert_eq!(
                view["presentation"].get("captures").unwrap_or(&json!([])),
                &captures
            );
        }
        let output = shell(json!({"command":"printf '\\377'; exit 3"}))
            .await
            .unwrap();
        assert_eq!(output.output.value["exit_code"], 3);
        assert!(
            output.output.value["stdout"]
                .as_str()
                .unwrap()
                .contains('\u{fffd}')
        );

        // The deadline elapses once the tool has captured the partial output.
        let command = json!({"command":"printf partial; exec sleep 3600", "timeout":3600});
        let (result, ()) = tokio::join!(bounded(shell(command)), async {
            live_stdout(&runtime).await;
            deadline.cancel();
        });
        let diagnostic = result.unwrap_err().diagnostic();
        assert_eq!(diagnostic.context.operation, Operation::Wait);
        assert_eq!(diagnostic.context.subject, Subject::Process);
        assert_eq!(diagnostic.context.effects, Effects::Started);
        assert!(matches!(&diagnostic.cause, Cause::Message(text) if text.contains("3600")));
        let failed = jobs.list(agent).await.last().unwrap().id;
        let view = jobs
            .inspect_output(
                OutputArgs::new(failed),
                CancellationToken::new(),
                &Default::default(),
            )
            .await
            .unwrap();
        assert_eq!(view["state"], "failed");
        assert_eq!(
            view["result"],
            json!({"signal":libc::SIGKILL,"stdout":"partial"})
        );
        assert_eq!(view["presentation"]["notice"], "Output incomplete.");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failures_before_spawn_report_their_stage_and_that_nothing_started() {
        let runtime = TestRuntime::new().await;
        let executor = executor(&runtime, true, tokio::time::sleep);
        std::fs::write(runtime.root.path().join("file-cwd"), "not a directory").unwrap();
        for (arguments, operation, argument) in [
            // A missing cwd fails path preflight; a file passes it and fails the handler.
            (
                json!({"command":"touch started", "cwd":"missing-cwd"}),
                Operation::Canonicalize,
                "cwd",
            ),
            (
                json!({"command":"touch started", "cwd":"file-cwd"}),
                Operation::Validate,
                "cwd",
            ),
            (json!({"command":""}), Operation::Validate, "command"),
            (json!({"command":[]}), Operation::Validate, "command"),
            (
                json!({"command":r#" ["touch", "started"] "#}),
                Operation::Validate,
                "command",
            ),
            (
                json!({"command":"true", "timeout":0}),
                Operation::Deserialize,
                "timeout",
            ),
            (
                json!({"command":"true", "timeout":3601}),
                Operation::Deserialize,
                "timeout",
            ),
        ] {
            let jobs = runtime.jobs.list(&runtime.agent).await.len();
            let error = executor
                .run_host(&runtime.agent, "exec", arguments.clone())
                .await
                .unwrap_err();
            let Diagnostic { cause, context } = error.diagnostic();
            // Argument validation fails at admission, before any job exists.
            if argument != "cwd" {
                assert_eq!(runtime.jobs.list(&runtime.agent).await.len(), jobs);
                let Cause::InvalidArguments(message) = cause else {
                    panic!("{cause:?}")
                };
                let encoded = arguments["command"]
                    .as_str()
                    .is_some_and(|text| text.contains('['));
                assert_eq!(message.contains("as an array"), encoded, "{message}");
            }
            assert_eq!(context.operation, operation);
            // Typed deserialization claims no effects; no job exists to have any.
            if operation != Operation::Deserialize {
                assert_eq!(context.effects, Effects::NotStarted);
            }
            match arguments["cwd"].as_str() {
                Some(cwd) => assert!(
                    matches!(&context.subject, Subject::WorkingDirectory(path) if path.ends_with(cwd)),
                    "{context:?}"
                ),
                None => assert_eq!(context.subject, Subject::argument([argument])),
            }
        }
        // Spawn ENOENT can also mean a missing interpreter, so no path is the subject.
        let error = executor
            .run_host(
                &runtime.agent,
                "exec",
                json!({"command":["/skyhook-test-missing-executable"]}),
            )
            .await
            .unwrap_err();
        let diagnostic = error.diagnostic();
        assert_eq!(diagnostic.context.operation, Operation::Spawn);
        assert_eq!(diagnostic.context.subject, Subject::Process);
        assert_eq!(diagnostic.context.effects, Effects::NotStarted);
        assert!(matches!(
            diagnostic.cause,
            Cause::Io {
                kind: IoKind::NotFound,
                ..
            }
        ));
        assert!(!runtime.root.path().join("started").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn capture_and_pipe_io_failures_keep_distinct_phases() {
        use crate::tool::{
            invocation::tests::{Authorizations, CapturedOutput},
            output::{
                OutputContext,
                tests::{CaptureStage, FailingCapture},
            },
        };
        use std::{
            io,
            pin::Pin,
            task::{Context, Poll},
        };
        use tokio::io::ReadBuf;

        fn broken() -> io::Error {
            io::Error::new(io::ErrorKind::BrokenPipe, "private details")
        }
        let opaque = Cause::Io {
            kind: IoKind::BrokenPipe,
            code: None,
            detail: None,
        };
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
            let context = LocalContext::new(
                crate::execution::ExecutionLocation::root(root.path().to_owned()),
                [Capability::Exec, Capability::Interactive]
                    .into_iter()
                    .collect(),
                Default::default(),
                tokio_util::sync::CancellationToken::new(),
                producer.clone(),
                Arc::new(Authorizations::default()),
            );
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", "printf output"])
                .current_dir(root.path());
            let error = bounded(run_process(context, command, std::future::pending()))
                .await
                .err()
                .unwrap();
            let diagnostic = error.diagnostic();
            assert_eq!(diagnostic.context.operation, operation);
            assert_eq!(
                diagnostic.context.subject,
                Subject::Label(TextCaptureField::Stdout.pointer().into())
            );
            assert_eq!(diagnostic.context.effects, Effects::Started);
            assert_eq!(diagnostic.cause, opaque);
            bounded(producer.settle()).await.unwrap();
        }

        struct FailingPipe;
        impl AsyncRead for FailingPipe {
            fn poll_read(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
                _: &mut ReadBuf<'_>,
            ) -> Poll<io::Result<()>> {
                Poll::Ready(Err(broken()))
            }
        }
        let producer = OutputContext::new(Arc::new(CapturedOutput::default()));
        let mut capture = Capture::new(
            producer
                .text_capture(TextCaptureField::Stderr)
                .await
                .unwrap(),
            TextCaptureField::Stderr,
        );
        let diagnostic = capture_stream(FailingPipe, &mut capture, STDERR_PIPE)
            .await
            .unwrap_err()
            .diagnostic();
        assert_eq!(diagnostic.context.operation, Operation::Read);
        assert_eq!(
            diagnostic.context.subject,
            Subject::Label(STDERR_PIPE.to_owned())
        );
        assert_eq!(diagnostic.cause, opaque);
        drop(capture);
        producer.settle().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_and_timeout_kill_descendants_even_after_the_shell_exits() {
        // The descendant outlives the shell holding `held`, a FIFO, open for
        // writing. Its reader sees EOF once every holder has died, whether or
        // not anything reaps them.
        const COMMAND: &str = "exec 3>held; sleep 3600 & printf started; exit 0";
        let stopped = async |interactive: bool, timeout: bool| {
            let runtime = TestRuntime::new().await;
            let held = runtime.root.path().join("held");
            let path = std::ffi::CString::new(held.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: `path` is a valid NUL-terminated string for the call.
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
            let mut held = tokio::net::unix::pipe::OpenOptions::new()
                .open_receiver(&held)
                .unwrap();
            let deadline = CancellationToken::new();
            let mut arguments = json!({"command":COMMAND, crate::tool::registry::BACKGROUND:true});
            if timeout {
                arguments["timeout"] = json!(3600);
            }
            let running = executor(&runtime, interactive, elapses(&deadline))
                .run_host(&runtime.agent, "exec", arguments)
                .await
                .unwrap();
            live_stdout(&runtime).await;
            if timeout {
                deadline.cancel();
            } else {
                runtime.jobs.cancel(running.job).await.unwrap();
            }
            let finished = bounded(runtime.jobs.wait(running.job, None, true)).await;
            bounded(held.read_to_end(&mut Vec::new())).await.unwrap();
            finished.unwrap()
        };
        let (cancelled, interactive, timed_out) = tokio::join!(
            stopped(false, false),
            stopped(true, false),
            stopped(false, true)
        );
        assert_eq!(cancelled.state, JobState::Cancelled);
        assert_eq!(interactive.state, JobState::Cancelled);
        // A timeout kills noninteractive descendants that hold the pipes open.
        assert_eq!(timed_out.state, JobState::Failed);
        let diagnostic = timed_out.diagnostic.unwrap();
        assert!(matches!(&diagnostic.cause, Cause::Message(text) if text.contains("timed out")));
    }

    // Use a separate session with a real controlling PTY; never change the
    // test runner's own session or terminal state.
    #[cfg(unix)]
    const PTY_HELPER: &str = "SKYHOOK_PROCESS_PTY_TEST";

    #[cfg(unix)]
    #[tokio::test]
    async fn capability_controls_terminal_access_under_a_pty() {
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: openpty initializes the two descriptor slots; optional arguments
        // are null. OwnedFd below takes sole ownership on success.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        for fd in [&master, &slave] {
            // SAFETY: these descriptors remain owned and valid throughout setup.
            assert_ne!(
                unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
                -1
            );
        }
        let mut command = Command::new(std::env::current_exe().unwrap());
        let helper = "tool::builtins::process::tests::pty_executor_helper";
        command
            .args(["--exact", helper, "--ignored", "--nocapture"])
            .env(PTY_HELPER, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let slave_fd = slave.as_raw_fd();
        // SAFETY: only async-signal-safe syscalls are used in the child. The slave
        // stays open until spawn finishes and is then closed on exec via CLOEXEC.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 || libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        let _group = ProcessGroup::led_by(&child);
        let output = bounded(child.wait_with_output()).await.unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "PTY helper failed: {stdout}\n{stderr}"
        );
        assert!(
            stdout.contains("1 passed"),
            "PTY helper filter did not execute the regression"
        );

        // Keep the slave open so an empty master returns EAGAIN rather than EIO.
        // No tool should have emitted its attempted credential prompt to the PTY.
        assert_ne!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
            -1
        );
        let mut terminal_output = Vec::new();
        match std::fs::File::from(master).read_to_end(&mut terminal_output) {
            Err(error) if error.kind() != std::io::ErrorKind::WouldBlock => {
                panic!("reading PTY output: {error}")
            }
            _ => {}
        }
        let prompt = String::from_utf8_lossy(&terminal_output);
        assert!(
            terminal_output.is_empty(),
            "unexpected terminal prompt: {prompt:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "invoked in a dedicated PTY/session by capability_controls_terminal_access_under_a_pty"]
    fn pty_executor_helper() {
        if std::env::var_os(PTY_HELPER).is_none() {
            return;
        }
        // Establish that this is a real controlling terminal, not merely a tty FD.
        let _tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .expect("helper must have a controlling terminal");
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            for interactive in [false, true] {
                let runtime = TestRuntime::new().await;
                // The fixture disables Targets: terminal gating must not depend on it.
                let executor = executor(&runtime, interactive, tokio::time::sleep);
                let (command, expected) = if interactive {
                    ("if (: <> /dev/tty) 2>/dev/null; then printf attached; else printf missing; fi", "attached")
                } else {
                    // With only setpgid and piped stdio this emits a terminal prompt
                    // then stops on SIGTTIN. The tool timeout makes that regression
                    // fail promptly instead of hanging the test suite.
                    ("if (: <> /dev/tty) 2>/dev/null; then exec 3<> /dev/tty; printf 'Password: ' >&3; read answer <&3; else printf isolated; fi", "isolated")
                };
                for argv in [false, true] {
                    let output = executor.run_host(&runtime.agent, "exec", sh_args(argv, command, Some(2))).await
                        .unwrap_or_else(|error| panic!("argv={argv} interactive={interactive}: {error}"));
                    assert_eq!(output.output.value["exit_code"], 0);
                    assert_eq!(output.output.value["stdout"], expected);
                }
            }
        });
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn noninteractive_askpass_overrides_provider_without_targets() {
        let inherited = [
            ("SSH_ASKPASS", "/inherited/prompt-helper"),
            ("SSH_ASKPASS_REQUIRE", "prefer"),
            ("DISPLAY", "inherited-display"),
            ("SKYHOOK_ASKPASS_SOCKET", "/inherited/prompt-socket"),
            ("SSH_AUTH_SOCK", "/inherited/agent-socket"),
        ];
        let command = r#"printf '%s\n' "$SSH_ASKPASS" "$SSH_ASKPASS_REQUIRE" "$DISPLAY" "$SKYHOOK_ASKPASS_SOCKET" "$SSH_AUTH_SOCK"; if test -x "$SSH_ASKPASS" && test -S "$SKYHOOK_ASKPASS_SOCKET"; then printf live; fi"#;
        for interactive in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let environment: crate::remote::backend::ProcessEnvironment = inherited
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect();
            let catalog = crate::tool::invocation::LocalCatalog::builtins().unwrap();
            let mut capabilities: crate::tool::policy::CapabilitySet =
                [Capability::Exec].into_iter().collect();
            if interactive {
                capabilities.insert(Capability::Interactive);
            }
            for argv in [false, true] {
                let sink = Arc::new(crate::tool::invocation::tests::CapturedOutput::default());
                let producer = crate::tool::output::OutputContext::new(sink.clone());
                let context = crate::tool::invocation::LocalContext::new(
                    crate::execution::ExecutionLocation::root(root.path().to_owned()),
                    capabilities.clone(),
                    environment.clone(),
                    tokio_util::sync::CancellationToken::new(),
                    producer.clone(),
                    Arc::new(crate::tool::invocation::tests::Authorizations::default()),
                );
                let output = catalog
                    .run("exec", sh_args(argv, command, None), context, root.path())
                    .await
                    .unwrap()
                    .value;
                producer.settle().await.unwrap();
                let stdout = String::from_utf8(sink.bytes()).unwrap();
                assert_eq!(output["exit_code"], 0);
                let lines: Vec<_> = stdout.lines().collect();
                assert_eq!(
                    lines[4], "/inherited/agent-socket",
                    "nonprompting credentials remain usable"
                );
                if interactive {
                    assert_eq!(lines, inherited.map(|(_, value)| value));
                } else {
                    assert_ne!(lines[0], "/inherited/prompt-helper");
                    assert_eq!(lines[1..3], ["force", "skyhook"]);
                    assert_ne!(lines[3], "/inherited/prompt-socket");
                    assert_eq!(
                        lines[5], "live",
                        "rejecting broker must live throughout command"
                    );
                    assert!(
                        !std::path::Path::new(lines[0]).exists(),
                        "helper must be cleaned up"
                    );
                    assert!(
                        !std::path::Path::new(lines[3]).exists(),
                        "socket must be cleaned up"
                    );
                }
            }
        }
    }
}
