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

const MAX_PROCESS_OUTPUT: usize = 1024 * 1024;
const PROCESS_CHUNK: usize = 8 * 1024;

pub(super) fn register(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    builder.register_targeted::<ExecArgs, ProcessOutput, _, _>(
        "exec",
        "Run an exact argument vector without shell parsing.",
        ToolOptions::new(vec![Capability::Exec])
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
    builder.register_targeted::<ShellArgs, ProcessOutput, _, _>(
        "shell",
        "Run /bin/sh -lc in the workspace.",
        ToolOptions::new(vec![Capability::Exec])
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
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
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
    let mut stdout_capture = Capture::default();
    let mut stderr_capture = Capture::default();
    let (status, timed_out, cancelled) = {
        let execution = async {
            let (status, out, err) = tokio::join!(
                child.wait(),
                capture_stream(context.clone(), "stdout", stdout, &mut stdout_capture),
                capture_stream(context.clone(), "stderr", stderr, &mut stderr_capture),
            );
            out?;
            err?;
            status.map_err(ToolError::from)
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
    let stdout = stdout_capture;
    let stderr = stderr_capture;
    let (stdout_text, stdout_truncated) = lossy_output(&stdout);
    let (stderr_text, stderr_truncated) = lossy_output(&stderr);
    let output = ProcessOutput {
        exit_code: status.code(),
        stdout: stdout_text,
        stderr: stderr_text,
        stdout_truncated,
        stderr_truncated,
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
    kind: &'static str,
    mut stream: R,
    capture: &mut Capture,
) -> Result<(), ToolError>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; PROCESS_CHUNK];
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = MAX_PROCESS_OUTPUT.saturating_sub(capture.bytes.len());
        let retained = read.min(remaining);
        if retained > 0 {
            capture.bytes.extend_from_slice(&buffer[..retained]);
            context
                .progress(
                    kind,
                    serde_json::json!({
                        "text": String::from_utf8_lossy(&buffer[..retained]),
                    }),
                )
                .await?;
        }
        if retained < read {
            capture.truncated = true;
        }
    }
    if capture.truncated {
        context
            .progress(kind, serde_json::json!({"truncated": true}))
            .await?;
    }
    Ok(())
}

fn lossy_output(capture: &Capture) -> (String, bool) {
    let mut output = String::from_utf8_lossy(&capture.bytes).into_owned();
    let mut truncated = capture.truncated;
    if output.len() > MAX_PROCESS_OUTPUT {
        let mut boundary = MAX_PROCESS_OUTPUT;
        while !output.is_char_boundary(boundary) {
            boundary -= 1;
        }
        output.truncate(boundary);
        truncated = true;
    }
    (output, truncated)
}

#[derive(Default)]
struct Capture {
    bytes: Vec<u8>,
    truncated: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ExecArgs {
    /// Program and arguments without shell parsing, for example `["cargo","test"]`.
    argv: Vec<String>,
    /// Working directory relative to the execution workspace by default.
    #[serde(default = "default_dot")]
    cwd: String,
    /// Execution timeout in seconds (1-3600). Omit or use null for no deadline.
    #[schemars(range(min = 1, max = 3600))]
    timeout: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ShellArgs {
    /// Command interpreted by the execution environment's shell.
    command: String,
    /// Working directory relative to the execution workspace by default.
    #[serde(default = "default_dot")]
    cwd: String,
    /// Execution timeout in seconds (1-3600). Omit or use null for no deadline.
    #[schemars(range(min = 1, max = 3600))]
    timeout: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct ProcessOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub timed_out: bool,
}

fn default_dot() -> String {
    ".".to_owned()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{
        identity::{AgentId, JobId},
        job::{JobManager, JobState},
        session::SessionStore,
        tool::{
            ToolRegistryBuilder,
            executor::{ExecutionError, ToolExecutor},
            policy::AllowAll,
        },
    };

    #[test]
    fn command_timeouts_are_optional_and_null_means_no_deadline() {
        assert_eq!(
            serde_json::from_value::<ShellArgs>(serde_json::json!({"command":"server"}))
                .unwrap()
                .timeout,
            None
        );
        assert_eq!(
            serde_json::from_value::<ExecArgs>(
                serde_json::json!({"argv":["server"],"timeout":null})
            )
            .unwrap()
            .timeout,
            None
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_kills_descendants_even_after_the_shell_exits() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let store = SessionStore::create(sessions.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let jobs = JobManager::new(store);
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder).unwrap();
        let executor = ToolExecutor::new(
            builder.build(),
            Arc::new(AllowAll),
            jobs.clone(),
            workspace.path().to_path_buf(),
        );
        let running = executor.execute(agent, "shell", serde_json::json!({
            "command":"(sleep 0.3; printf escaped > escaped) & printf ready; exit 0", "bg":true
        }), None).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !jobs.events(running.job, 0, 10).await.unwrap().is_empty() {
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
        assert!(!workspace.path().join("escaped").exists());
    }

    #[tokio::test]
    async fn nonzero_is_success_and_timeout_persists_partial_output() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let store = SessionStore::create(sessions.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let jobs = JobManager::new(store);
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder).unwrap();
        let executor = ToolExecutor::new(
            builder.build(),
            Arc::new(AllowAll),
            jobs.clone(),
            workspace.path().to_path_buf(),
        );
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
        let envelope = jobs.snapshot(JobId::new(2).unwrap()).await.unwrap();
        assert_eq!(envelope.state, JobState::Failed);
        assert_eq!(envelope.output.unwrap()["timed_out"], true);
    }
}
