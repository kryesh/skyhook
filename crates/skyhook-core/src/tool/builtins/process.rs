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
    command
        .envs(&context.process_environment)
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
    use crate::test_support::TestRuntime;

    use super::*;
    use crate::{
        job::JobState,
        tool::{ToolRegistryBuilder, executor::ExecutionError},
    };

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_kills_descendants_even_after_the_shell_exits() {
        let runtime = TestRuntime::new().await;
        let agent = runtime.agent.clone();
        let jobs = runtime.jobs.clone();
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder).unwrap();
        let executor = runtime.executor(builder);
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

    #[tokio::test]
    async fn nonzero_is_success_and_timeout_persists_partial_output() {
        let runtime = TestRuntime::new().await;
        let agent = runtime.agent.clone();
        let jobs = runtime.jobs.clone();
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder).unwrap();
        let executor = runtime.executor(builder);
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
}
