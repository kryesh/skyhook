use crate::tool::ToolOptions;
use crate::tool::invocation::{LocalCatalogBuilder, LocalContext, LocalError};
use crate::tool::output::ProducedOutput;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _},
    process::Command,
};

use crate::tool::StreamEnd;
use crate::tool::output::{FinishedOutput, TextCaptureField};
use crate::tool::{
    PathKind, RegistryError,
    policy::{Capability, PathAccess},
};

mod capture;
use capture::Capture;

const PROCESS_CHUNK: usize = 8 * 1024;

pub(super) fn register(builder: &mut LocalCatalogBuilder) -> Result<(), RegistryError> {
    let options = || {
        ToolOptions::new(vec![Capability::Exec])
            .placement(crate::tool::ToolPlacement::TargetedWorkspace)
            .target_authentication()
            .named()
            .background()
            .default_path_argument("cwd", ".", PathAccess::Read, PathKind::Existing)
    };
    builder.register_product::<ExecArgs, ProcessOutput, _, _>(
        "exec",
        "Run an exact argument vector without shell parsing. Stdin is closed.",
        options(),
        move |context, args| async move {
            let (program, arguments) = args
                .argv
                .split_first()
                .ok_or_else(|| LocalError::InvalidArguments("argv cannot be empty".to_owned()))?;
            let cwd = working_directory(&args.cwd).await?;
            let mut command = Command::new(program);
            command.args(arguments).current_dir(cwd);
            run_process(context, command, args.timeout)
                .await
                .map(ProcessResult::into_output)
        },
    )?;
    builder.register_product::<ShellArgs, ProcessOutput, _, _>(
        "shell",
        "Run /bin/sh -lc in the workspace. Stdin is closed.",
        options(),
        move |context, args| async move {
            if args.command.is_empty() {
                return Err(LocalError::InvalidArguments(
                    "command cannot be empty".to_owned(),
                ));
            }
            let cwd = working_directory(&args.cwd).await?;
            let mut command = Command::new("/bin/sh");
            command.arg("-lc").arg(args.command).current_dir(cwd);
            run_process(context, command, args.timeout)
                .await
                .map(ProcessResult::into_output)
        },
    )?;
    Ok(())
}

async fn working_directory(cwd: &str) -> Result<&std::path::Path, LocalError> {
    let path = std::path::Path::new(cwd);
    if !tokio::fs::metadata(path).await?.is_dir() {
        return Err(LocalError::Failed(format!(
            "working directory is not a directory: {}",
            path.display()
        )));
    }
    Ok(path)
}

async fn run_process(
    context: LocalContext,
    mut command: Command,
    timeout: Option<u64>,
) -> Result<ProcessResult, LocalError> {
    if timeout.is_some_and(|seconds| !(1..=3_600).contains(&seconds)) {
        return Err(LocalError::InvalidArguments(
            "timeout must be 1 through 3600".to_owned(),
        ));
    }
    // Override inherited/provider askpass settings even without Targets. Keep the
    // rejecting broker alive until the command and its cleanup have completed.
    let rejecting_askpass = if context.capabilities().contains(Capability::Interactive) {
        None
    } else {
        Some(
            crate::remote::AskpassServer::start(std::sync::Arc::new(
                crate::remote::RejectSensitivePrompts,
            ))
            .map_err(|error| {
                LocalError::Failed(format!("failed to start noninteractive askpass: {error}"))
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
        return Err(LocalError::Cancelled);
    }
    let mut child = command.spawn()?;
    #[cfg(unix)]
    let group = ProcessGroup(
        i32::try_from(child.id().expect("spawned process has an ID"))
            .map_err(|_| LocalError::Failed("process ID is out of range".to_owned()))?,
    );
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| LocalError::Failed("process stdout unavailable".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| LocalError::Failed("process stderr unavailable".to_owned()))?;

    let deadline = async {
        match timeout {
            Some(seconds) => {
                tokio::time::sleep(Duration::from_secs(seconds)).await;
                seconds
            }
            None => std::future::pending::<u64>().await,
        }
    };
    // Keep cancellation and the deadline active while descendants hold output pipes open.
    let mut stdout_capture = Capture::new(context.text_capture(TextCaptureField::Stdout).await?);
    let mut stderr_capture = Capture::new(context.text_capture(TextCaptureField::Stderr).await?);
    let completion = {
        let execution = async {
            let (status, (), ()) = tokio::try_join!(
                async { child.wait().await.map_err(LocalError::from) },
                capture_stream(stdout, &mut stdout_capture),
                capture_stream(stderr, &mut stderr_capture),
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
        child.kill().await?;
        child.wait().await.map_err(LocalError::from)
    };
    let finish = async move |status: ExitStatus| {
        Ok::<_, LocalError>(ProcessResult {
            exit_code: status.code(),
            captures: [
                stdout_capture.finish().await?,
                stderr_capture.finish().await?,
            ]
            .into_iter()
            .flatten()
            .collect(),
            timed_out: false,
        })
    };
    // Keep select! unbiased above. The selected variant alone controls both
    // cleanup and presentation; only expiry can carry a finite deadline.
    match completion {
        ProcessCompletion::Exited(status) => finish(status).await,
        ProcessCompletion::TimedOut(seconds) => {
            let output = ProcessResult {
                timed_out: true,
                ..finish(stop().await?).await?
            };
            Err(LocalError::with_output(
                format!("process timed out after {seconds} seconds"),
                output.into_output(),
            ))
        }
        ProcessCompletion::Cancelled => {
            finish(stop().await?).await?;
            Err(LocalError::Cancelled)
        }
    }
}

enum ProcessCompletion {
    Exited(ExitStatus),
    TimedOut(u64),
    Cancelled,
}

#[cfg(unix)]
struct ProcessGroup(i32);
#[cfg(unix)]
impl ProcessGroup {
    fn kill(&self) {
        // SAFETY: kill takes an integer process group ID and does not access memory.
        unsafe {
            libc::kill(-self.0, libc::SIGKILL);
        }
    }
}
#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

async fn capture_stream<R>(mut stream: R, capture: &mut Capture) -> Result<(), LocalError>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; PROCESS_CHUNK];
    loop {
        let read = stream.read(&mut buffer).await?;
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
    /// Program and arguments without shell parsing, for example `["cargo","test"]`.
    argv: Vec<String>,
    /// Working directory.
    #[serde(default = "super::default_dot")]
    cwd: String,
    /// Timeout seconds; omitted/null means no deadline.
    #[schemars(range(min = 1, max = 3600))]
    timeout: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ShellArgs {
    /// Command interpreted by the execution environment's shell.
    command: String,
    /// Working directory.
    #[serde(default = "super::default_dot")]
    cwd: String,
    /// Timeout seconds; omitted/null means no deadline.
    #[schemars(range(min = 1, max = 3600))]
    timeout: Option<u64>,
}

/// Internal output retains completed field bindings until the canonical product handoff.
struct ProcessResult {
    exit_code: Option<i32>,
    captures: Vec<FinishedOutput>,
    timed_out: bool,
}

impl ProcessResult {
    fn into_output(self) -> ProducedOutput {
        let output = ProcessOutput {
            exit_code: self.exit_code,
            // Completed captures replace these fields when bytes were observed. An
            // absent capture therefore means the stream was observed to be empty,
            // not that its value is unknown.
            stdout: String::new(),
            stderr: String::new(),
            timed_out: self.timed_out,
        };
        let mut output =
            ProducedOutput::new(serde_json::to_value(output).expect("process output serializes"))
                .with_captures(self.captures);
        if self.timed_out {
            output.streams = StreamEnd::Cut;
        }
        output
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct ProcessOutput {
    pub exit_code: Option<i32>,
    #[schemars(extend("x-skyhook-truncatable" = true))]
    pub stdout: String,
    #[schemars(extend("x-skyhook-truncatable" = true))]
    pub stderr: String,
    pub timed_out: bool,
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

    use super::*;
    use crate::tool::ToolRegistryBuilder;
    use crate::{
        job::{JobState, output::OutputArgs},
        tests::TestRuntime,
        tool::{
            executor::{ExecutionError, ToolExecutor},
            policy::CapabilitySet,
        },
    };

    fn executor(runtime: &TestRuntime, interactive: bool) -> ToolExecutor {
        let mut builder = ToolRegistryBuilder::default();
        builder.register_local(register).unwrap();
        let mut capabilities = CapabilitySet::default();
        capabilities.remove(Capability::Targets);
        if !interactive {
            capabilities.remove(Capability::Interactive);
        }
        runtime.executor(builder).with_capabilities(capabilities)
    }

    #[test]
    fn output_schema_requires_stable_fields_but_keeps_exit_code_nullable() {
        let schema = serde_json::to_value(
            schemars::generate::SchemaSettings::default()
                .for_serialize()
                .into_generator()
                .into_root_schema_for::<ProcessOutput>(),
        )
        .unwrap();
        let required = schema["required"].as_array().unwrap();
        for field in ["exit_code", "stdout", "stderr", "timed_out"] {
            assert!(required.contains(&json!(field)), "{field}: {schema}");
        }
        fn allows_null(schema: &Value) -> bool {
            schema["type"] == "null"
                || schema["type"]
                    .as_array()
                    .is_some_and(|types| types.contains(&json!("null")))
                || ["anyOf", "oneOf"]
                    .into_iter()
                    .filter_map(|key| schema[key].as_array())
                    .flatten()
                    .any(allows_null)
        }
        assert!(allows_null(&schema["properties"]["exit_code"]), "{schema}");
        assert!(!allows_null(&schema["properties"]["stdout"]), "{schema}");
    }

    /// `exec` and `shell` arguments running the same `/bin/sh` command.
    #[cfg(unix)]
    fn sh_args(tool: &str, command: &str, timeout: Option<u64>) -> Value {
        let mut args = if tool == "exec" {
            json!({"argv":["/bin/sh", "-c", command]})
        } else {
            json!({"command":command})
        };
        if let Some(timeout) = timeout {
            args["timeout"] = json!(timeout);
        }
        args
    }

    #[tokio::test]
    async fn exit_codes_captures_and_timeout_partial_output() {
        let runtime = TestRuntime::new().await;
        let (agent, jobs) = (&runtime.agent, &runtime.jobs);
        let executor = executor(&runtime, true);
        let shell = async |command: Value| executor.run_host(agent, "shell", command).await;
        // A signal exit has an unknown (null) exit code, while both observed
        // streams and the timeout flag still have concrete defaults.
        let signal = shell(json!({"command":"kill -TERM $$"})).await.unwrap();
        assert_eq!(
            signal.output.value,
            json!({"exit_code":null,"stdout":"","stderr":"","timed_out":false})
        );
        for (command, expected, captures) in [
            (
                "exit 0",
                json!({"exit_code":0,"stdout":"","stderr":"","timed_out":false}),
                json!([]),
            ),
            (
                "printf warning >&2",
                json!({"exit_code":0,"stdout":"","stderr":"warning","timed_out":false}),
                json!([{"field": "/result/stderr", "kind": "text", "complete": true, "output": null}]),
            ),
        ] {
            let output = shell(json!({"command":command})).await.unwrap();
            assert_eq!(output.output.value, expected);
            let args = OutputArgs::new(output.job);
            let view = jobs
                .inspect_output(args, &Default::default())
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

        let error = shell(json!({"command":"printf partial; sleep 2", "timeout":1})).await;
        let Err(ExecutionError::Failed {
            message,
            output: Some(output),
        }) = error
        else {
            panic!("expected failed output, got {error:?}");
        };
        assert!(message.contains("timed out"));
        assert_eq!(output.value["timed_out"], true);
        let failed_job = jobs.list(agent).await.last().unwrap().id;
        let envelope = jobs.snapshot(failed_job).await.unwrap();
        assert_eq!(envelope.state, JobState::Failed);
        assert_eq!(envelope.output.unwrap()["timed_out"], true);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_and_timeout_kill_descendants_even_after_the_shell_exits() {
        let cancelled = async |interactive: bool| {
            let runtime = TestRuntime::new().await;
            let jobs = runtime.jobs.clone();
            let executor = executor(&runtime, interactive);
            let command = "(sleep 0.3; printf escaped > escaped) & printf ready; exit 0";
            let arguments = json!({"command":command, "bg":true});
            let running = executor
                .run_host(&runtime.agent, "shell", arguments)
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let mut args = OutputArgs::new(running.job);
                    args.field = Some("/result/stdout".into());
                    let view = jobs
                        .present_output(args, &Default::default())
                        .await
                        .unwrap();
                    if view["presentation"]["preview"]["lines"]
                        .as_array()
                        .is_some_and(|lines| !lines.is_empty())
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            jobs.cancel(running.job).await.unwrap();
            let waited = jobs
                .wait(running.job, Some(Duration::from_secs(2)), true)
                .await;
            assert_eq!(waited.unwrap().state, JobState::Cancelled);
            tokio::time::sleep(Duration::from_millis(400)).await;
            assert!(!runtime.root.path().join("escaped").exists());
        };
        // Timeout kills noninteractive descendants that hold the pipes open.
        let timed_out = async {
            let runtime = TestRuntime::new().await;
            let command = "(sleep 1.5; printf escaped > escaped) & printf ready; exit 0";
            let arguments = json!({"command":command, "timeout":1});
            let error = executor(&runtime, false)
                .run_host(&runtime.agent, "shell", arguments)
                .await;
            let error = error.expect_err("descendant-held pipes must time out");
            assert!(error.to_string().contains("timed out"), "{error}");
            tokio::time::sleep(Duration::from_millis(700)).await;
            assert!(!runtime.root.path().join("escaped").exists());
        };
        tokio::join!(cancelled(false), cancelled(true), timed_out);
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
        let _group = ProcessGroup(i32::try_from(child.id().unwrap()).unwrap());
        let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output());
        let output = output.await.expect("PTY executor helper hung").unwrap();
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
                let executor = executor(&runtime, interactive);
                let (command, expected) = if interactive {
                    ("if (: <> /dev/tty) 2>/dev/null; then printf attached; else printf missing; fi", "attached")
                } else {
                    // With only setpgid and piped stdio this emits a terminal prompt
                    // then stops on SIGTTIN. The tool timeout makes that regression
                    // fail promptly instead of hanging the test suite.
                    ("if (: <> /dev/tty) 2>/dev/null; then exec 3<> /dev/tty; printf 'Password: ' >&3; read answer <&3; else printf isolated; fi", "isolated")
                };
                for tool in ["exec", "shell"] {
                    let output = executor.run_host(&runtime.agent, tool, sh_args(tool, command, Some(2))).await
                        .unwrap_or_else(|error| panic!("{tool} interactive={interactive}: {error}"));
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
            for tool in ["exec", "shell"] {
                let sink = Arc::new(crate::tool::invocation::tests::CapturedOutput::default());
                let producer = crate::tool::output::OutputContext::new(sink.clone());
                let arguments = sh_args(tool, command, None);
                let context = crate::tool::invocation::LocalContext::new(
                    crate::execution::ExecutionLocation::root(root.path().to_owned()),
                    capabilities.clone(),
                    environment.clone(),
                    tokio_util::sync::CancellationToken::new(),
                    producer.clone(),
                    Arc::new(crate::tool::invocation::tests::Authorizations::default()),
                    arguments.clone(),
                );
                let output = catalog
                    .run(tool, arguments, context, root.path())
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
