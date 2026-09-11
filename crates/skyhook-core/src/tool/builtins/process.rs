use std::{process::Stdio, time::Duration};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _},
    process::Command,
};

use super::workspace::resolve_directory;
use crate::tool::{
    PathKind, RegistryError, ToolContext, ToolError, ToolOptions, ToolOutput, ToolRegistryBuilder,
    policy::{Capability, PathAccess},
};

const PROCESS_CHUNK: usize = 8 * 1024;

pub(super) fn register(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    builder.register::<ExecArgs, ProcessOutput, _, _>(
        "exec",
        "Run an exact argument vector without shell parsing.",
        ToolOptions::new(vec![Capability::Exec])
            .placement(crate::tool::ToolPlacement::TargetedWorkspace)
            .named()
            .background()
            .input()
            .default_path_argument("cwd", ".", PathAccess::Read, PathKind::Existing),
        move |context, args| async move {
            let (program, arguments) = args
                .argv
                .split_first()
                .ok_or_else(|| ToolError::InvalidArguments("argv cannot be empty".to_owned()))?;
            let cwd = resolve_directory(&context.execution_location.workspace, &args.cwd).await?;
            let mut command = Command::new(program);
            command.args(arguments).current_dir(cwd);
            run_process(context, command, args.timeout).await
        },
    )?;
    builder.register::<ShellArgs, ProcessOutput, _, _>(
        "shell",
        "Run /bin/sh -lc in the workspace.",
        ToolOptions::new(vec![Capability::Exec])
            .placement(crate::tool::ToolPlacement::TargetedWorkspace)
            .named()
            .background()
            .input()
            .default_path_argument("cwd", ".", PathAccess::Read, PathKind::Existing),
        move |context, args| async move {
            if args.command.is_empty() {
                return Err(ToolError::InvalidArguments(
                    "command cannot be empty".to_owned(),
                ));
            }
            let cwd = resolve_directory(&context.execution_location.workspace, &args.cwd).await?;
            let mut command = Command::new("/bin/sh");
            command.arg("-lc").arg(args.command).current_dir(cwd);
            run_process(context, command, args.timeout).await
        },
    )?;
    Ok(())
}

async fn run_process(
    context: ToolContext,
    mut command: Command,
    timeout: Option<u64>,
) -> Result<ProcessOutput, ToolError> {
    if timeout.is_some_and(|seconds| !(1..=3_600).contains(&seconds)) {
        return Err(ToolError::InvalidArguments(
            "timeout must be 1 through 3600".to_owned(),
        ));
    }
    // Override inherited/provider askpass settings even without Targets. Keep the
    // rejecting broker alive until the command and its cleanup have completed.
    let rejecting_askpass = if context.capabilities.contains(Capability::Interactive) {
        None
    } else {
        Some(
            crate::remote::AskpassServer::start(std::sync::Arc::new(
                crate::remote::RejectSensitivePrompts,
            ))
            .map_err(|error| {
                ToolError::Failed(format!("failed to start noninteractive askpass: {error}"))
            })?,
        )
    };
    command.envs(&context.process_environment);
    if let Some(askpass) = &rejecting_askpass {
        command.envs(askpass.environment());
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    if context.capabilities.contains(Capability::Interactive) {
        command.process_group(0);
    } else {
        // Piped stdio alone still permits /dev/tty access (and SIGTTIN stops).
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
        return Err(ToolError::Cancelled);
    }
    let mut child = command.spawn()?;
    #[cfg(unix)]
    let group = ProcessGroup(
        i32::try_from(child.id().expect("spawned process has an ID"))
            .map_err(|_| ToolError::Failed("process ID is out of range".to_owned()))?,
    );
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| ToolError::Failed("process stdin unavailable".to_owned()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ToolError::Failed("process stdout unavailable".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ToolError::Failed("process stderr unavailable".to_owned()))?;

    let input_context = context.clone();
    let input_task = tokio::spawn(async move {
        let mut stdin = stdin;
        while let Ok(value) = input_context.receive().await {
            let text = value.as_str().ok_or_else(|| {
                ToolError::InvalidArguments("process input must be a string".to_owned())
            })?;
            stdin.write_all(text.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await?;
        }
        Ok::<(), ToolError>(())
    });

    let _input_guard = AbortTask(input_task.abort_handle());
    let deadline = async {
        match timeout {
            Some(seconds) => tokio::time::sleep(Duration::from_secs(seconds)).await,
            None => std::future::pending::<()>().await,
        }
    };
    // Keep cancellation and the deadline active while descendants hold output pipes open.
    let mut stdout_capture = Capture::new(context.capture_path("/result/stdout").await?).await?;
    let mut stderr_capture = Capture::new(context.capture_path("/result/stderr").await?).await?;
    let (status, timed_out, cancelled) = {
        let execution = async {
            let (status, (), ()) = tokio::try_join!(
                async { child.wait().await.map_err(ToolError::from) },
                capture_stream(context.clone(), "stdout", stdout, &mut stdout_capture),
                capture_stream(context.clone(), "stderr", stderr, &mut stderr_capture),
            )?;
            Ok::<_, ToolError>(status)
        };
        tokio::pin!(execution);
        tokio::select! {
            status = &mut execution => (Some(status?), false, false),
            () = deadline => (None, true, false),
            () = context.cancelled() => (None, false, true),
        }
    };
    let status = if let Some(status) = status {
        status
    } else {
        #[cfg(unix)]
        group.kill();
        child.kill().await?;
        child.wait().await?
    };
    let stdout_text = (stdout_capture.file.metadata().await?.len() > 0).then(String::new);
    let stderr_text = (stderr_capture.file.metadata().await?.len() > 0).then(String::new);
    let output = ProcessOutput {
        exit_code: status.code(),
        stdout: stdout_text,
        stderr: stderr_text,
        timed_out,
    };
    if cancelled {
        return Err(ToolError::Cancelled);
    }
    if timed_out {
        let value = serde_json::to_value(&output)?;
        return Err(ToolError::with_output(
            format!(
                "process timed out after {} seconds",
                timeout.expect("finite deadline expired")
            ),
            ToolOutput::new(value),
        ));
    }
    Ok(output)
}

struct AbortTask(tokio::task::AbortHandle);
impl Drop for AbortTask {
    fn drop(&mut self) {
        self.0.abort();
    }
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

async fn capture_stream<R>(
    context: ToolContext,
    _kind: &'static str,
    mut stream: R,
    capture: &mut Capture,
) -> Result<(), ToolError>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; PROCESS_CHUNK];
    let mut pending = Vec::new();
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            capture
                .file
                .write_all(String::from_utf8_lossy(&pending).as_bytes())
                .await?;
            break;
        }
        pending.extend_from_slice(&buffer[..read]);
        loop {
            match std::str::from_utf8(&pending) {
                Ok(text) => {
                    capture.file.write_all(text.as_bytes()).await?;
                    pending.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    capture.file.write_all(&pending[..valid]).await?;
                    pending.drain(..valid);
                    if let Some(length) = error.error_len() {
                        capture.file.write_all("�".as_bytes()).await?;
                        pending.drain(..length);
                    } else {
                        break;
                    }
                }
            }
        }
        capture.file.flush().await?;
        context.output_changed().await;
    }
    Ok(())
}

struct Capture {
    file: tokio::fs::File,
}
impl Capture {
    async fn new(path: std::path::PathBuf) -> Result<Self, ToolError> {
        Ok(Self {
            file: tokio::fs::File::create(&path).await?,
        })
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ExecArgs {
    /// Program and arguments without shell parsing, for example `["cargo","test"]`.
    argv: Vec<String>,
    /// Working directory.
    #[serde(default = "default_dot")]
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
    #[serde(default = "default_dot")]
    cwd: String,
    /// Timeout seconds; omitted/null means no deadline.
    #[schemars(range(min = 1, max = 3600))]
    timeout: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct ProcessOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[schemars(extend("x-skyhook-truncatable" = true))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[schemars(extend("x-skyhook-truncatable" = true))]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub timed_out: bool,
}

fn default_dot() -> String {
    ".".to_owned()
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::{
        io::Read as _,
        os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd},
    };

    use crate::tests::TestRuntime;

    use super::*;
    use crate::{
        job::JobState,
        tool::{
            ToolRegistryBuilder,
            executor::{ExecutionError, ToolExecutor},
            policy::CapabilitySet,
        },
    };

    fn executor(runtime: &TestRuntime, interactive: bool) -> ToolExecutor {
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder).unwrap();
        let mut capabilities = CapabilitySet::default();
        capabilities.remove(Capability::Targets);
        if !interactive {
            capabilities.remove(Capability::Interactive);
        }
        runtime.executor(builder).with_capabilities(capabilities)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_kills_descendants_even_after_the_shell_exits() {
        for interactive in [false, true] {
            let runtime = TestRuntime::new().await;
            let agent = runtime.agent.clone();
            let jobs = runtime.jobs.clone();
            let executor = executor(&runtime, interactive);
            let running = executor.execute(agent, "shell", serde_json::json!({
                "command":"(sleep 0.3; printf escaped > escaped) & printf ready; exit 0", "bg":true
            }), None).await.unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let mut args = crate::job::output::OutputArgs::new(running.job);
                    args.field = Some("/result/stdout".into());
                    if jobs
                        .present_output(args, &Default::default())
                        .await
                        .unwrap()["preview"]["lines"]
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
            assert_eq!(
                jobs.wait(running.job, Some(Duration::from_secs(2)), true)
                    .await
                    .unwrap()
                    .state,
                crate::job::JobState::Cancelled
            );
            tokio::time::sleep(Duration::from_millis(400)).await;
            assert!(!runtime.root.path().join("escaped").exists());
        }
    }

    #[tokio::test]
    async fn nonzero_is_success_and_timeout_persists_partial_output() {
        let runtime = TestRuntime::new().await;
        let agent = runtime.agent.clone();
        let jobs = runtime.jobs.clone();
        let executor = executor(&runtime, true);
        for (command, expected) in [
            ("exit 0", serde_json::json!({"exit_code":0})),
            (
                "printf warning >&2",
                serde_json::json!({"exit_code":0,"stderr":"warning"}),
            ),
        ] {
            let output = executor
                .execute(
                    agent.clone(),
                    "shell",
                    serde_json::json!({"command":command}),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(output.output.value, expected);
        }
        let output = executor
            .execute(
                agent.clone(),
                "shell",
                serde_json::json!({"command":"printf '\\377'; exit 3"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(output.output.value["exit_code"], 3);
        assert!(
            output.output.value["stdout"]
                .as_str()
                .unwrap()
                .contains('\u{fffd}')
        );

        let error = executor
            .execute(
                agent,
                "shell",
                serde_json::json!({"command":"printf partial; sleep 2", "timeout":1}),
                None,
            )
            .await
            .err()
            .unwrap();
        let output = match error {
            ExecutionError::Failed {
                message,
                output: Some(output),
            } => {
                assert!(message.contains("timed out"));
                output
            }
            error => panic!("expected failed output, got {error}"),
        };
        assert_eq!(output.value["timed_out"], true);
        let failed_job = jobs.list(&runtime.agent).await.last().unwrap().id;
        let envelope = jobs.snapshot(failed_job).await.unwrap();
        assert_eq!(envelope.state, JobState::Failed);
        assert_eq!(envelope.output.unwrap()["timed_out"], true);
    }

    // Use a separate session with a real controlling PTY; never change the
    // test runner's own session or terminal state.
    #[cfg(unix)]
    const PTY_HELPER: &str = "SKYHOOK_PROCESS_PTY_TEST";

    #[cfg(unix)]
    #[tokio::test]
    async fn capability_controls_terminal_access_under_a_pty() {
        let mut master = -1;
        let mut slave = -1;
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
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };
        for fd in [&master, &slave] {
            // SAFETY: these descriptors remain owned and valid throughout setup.
            assert_ne!(
                unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
                -1
            );
        }
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "tool::builtins::process::tests::pty_executor_helper",
                "--ignored",
                "--nocapture",
            ])
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
        let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
            .await
            .expect("PTY executor helper hung")
            .unwrap();
        assert!(
            output.status.success(),
            "PTY helper failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "PTY helper filter did not execute the regression"
        );

        // Keep the slave open so an empty master returns EAGAIN rather than EIO.
        // No tool should have emitted its attempted credential prompt to the PTY.
        assert_ne!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
            -1
        );
        let mut master = std::fs::File::from(master);
        let mut terminal_output = Vec::new();
        match master.read_to_end(&mut terminal_output) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("reading PTY output: {error}"),
        }
        assert!(
            terminal_output.is_empty(),
            "unexpected terminal prompt: {:?}",
            String::from_utf8_lossy(&terminal_output)
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
                    let args = if tool == "exec" {
                        serde_json::json!({"argv":["/bin/sh", "-c", command], "timeout":2})
                    } else {
                        serde_json::json!({"command":command, "timeout":2})
                    };
                    let output = executor.execute(runtime.agent.clone(), tool, args, None).await
                        .unwrap_or_else(|error| panic!("{tool} interactive={interactive}: {error}"));
                    assert_eq!(output.output.value["exit_code"], 0);
                    assert_eq!(output.output.value["stdout"], expected);
                }
            }
        });
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_noninteractive_descendants_after_shell_exit() {
        let runtime = TestRuntime::new().await;
        let executor = executor(&runtime, false);
        let error = executor.execute(runtime.agent.clone(), "shell", serde_json::json!({
            "command":"(sleep 1.5; printf escaped > escaped) & printf ready; exit 0", "timeout":1
        }), None).await.expect_err("descendant-held pipes must time out");
        assert!(error.to_string().contains("timed out"), "{error}");
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(!runtime.root.path().join("escaped").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn noninteractive_askpass_overrides_provider_without_targets() {
        for interactive in [false, true] {
            let runtime = TestRuntime::new().await;
            let environment = std::collections::BTreeMap::from([
                ("SSH_ASKPASS".into(), "/inherited/prompt-helper".into()),
                ("SSH_ASKPASS_REQUIRE".into(), "prefer".into()),
                ("DISPLAY".into(), "inherited-display".into()),
                (
                    "SKYHOOK_ASKPASS_SOCKET".into(),
                    "/inherited/prompt-socket".into(),
                ),
                ("SSH_AUTH_SOCK".into(), "/inherited/agent-socket".into()),
            ]);
            let executor = executor(&runtime, interactive).with_process_environment(environment);
            for tool in ["exec", "shell"] {
                let command = r#"printf '%s\n' "$SSH_ASKPASS" "$SSH_ASKPASS_REQUIRE" "$DISPLAY" "$SKYHOOK_ASKPASS_SOCKET" "$SSH_AUTH_SOCK"; if test -x "$SSH_ASKPASS" && test -S "$SKYHOOK_ASKPASS_SOCKET"; then printf live; fi"#;
                let args = if tool == "exec" {
                    serde_json::json!({"argv":["/bin/sh", "-c", command]})
                } else {
                    serde_json::json!({"command":command})
                };
                let output = executor
                    .execute(runtime.agent.clone(), tool, args, None)
                    .await
                    .unwrap();
                assert_eq!(output.output.value["exit_code"], 0);
                let lines: Vec<_> = output.output.value["stdout"]
                    .as_str()
                    .unwrap()
                    .lines()
                    .collect();
                assert_eq!(
                    lines[4], "/inherited/agent-socket",
                    "nonprompting credentials remain usable"
                );
                if interactive {
                    assert_eq!(
                        lines,
                        [
                            "/inherited/prompt-helper",
                            "prefer",
                            "inherited-display",
                            "/inherited/prompt-socket",
                            "/inherited/agent-socket"
                        ]
                    );
                } else {
                    assert_ne!(lines[0], "/inherited/prompt-helper");
                    assert_eq!(lines[1], "force");
                    assert_eq!(lines[2], "skyhook");
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
