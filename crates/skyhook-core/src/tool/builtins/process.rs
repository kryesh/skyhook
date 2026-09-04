use std::{process::Stdio, time::Duration};

use schemars::JsonSchema;
use schemars::schema_for;
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
    let exec_schema = serde_json::to_value(schema_for!(ExecArgs))
        .map_err(|error| RegistryError::Schema(error.to_string()))?;
    let process_output_schema = serde_json::to_value(schema_for!(ProcessOutput))
        .map_err(|error| RegistryError::Schema(error.to_string()))?;
    builder.register_dynamic_targeted(
        "exec",
        "Run an exact argument vector without shell parsing.",
        exec_schema,
        ToolOptions::new(vec![Capability::Exec])
            .output_schema(process_output_schema.clone())
            .background()
            .input()
            .default_path_argument("cwd", ".", PathAccess::Read, PathKind::Existing),
        move |context, arguments| async move {
            let args: ExecArgs = serde_json::from_value(arguments)
                .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
            let (program, arguments) = args
                .argv
                .split_first()
                .ok_or_else(|| ToolError::InvalidArguments("argv cannot be empty".to_owned()))?;
            let cwd = resolve_directory(&context.execution_location.workspace, &args.cwd).await?;
            let mut command = Command::new(program);
            command.args(arguments).current_dir(cwd);
            run_process(context, command, args.timeout)
                .await
                .map(|output| {
                    ToolOutput::new(
                        serde_json::to_value(output).expect("process output serializes"),
                    )
                })
        },
    )?;
    let shell_schema = serde_json::to_value(schema_for!(ShellArgs))
        .map_err(|error| RegistryError::Schema(error.to_string()))?;
    builder.register_dynamic_targeted(
        "shell",
        "Run /bin/sh -lc in the workspace.",
        shell_schema,
        ToolOptions::new(vec![Capability::Exec])
            .output_schema(process_output_schema)
            .background()
            .input()
            .default_path_argument("cwd", ".", PathAccess::Read, PathKind::Existing),
        move |context, arguments| async move {
            let args: ShellArgs = serde_json::from_value(arguments)
                .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
            if args.command.is_empty() {
                return Err(ToolError::InvalidArguments(
                    "command cannot be empty".to_owned(),
                ));
            }
            let cwd = resolve_directory(&context.execution_location.workspace, &args.cwd).await?;
            let mut command = Command::new("/bin/sh");
            command.arg("-lc").arg(args.command).current_dir(cwd);
            run_process(context, command, args.timeout)
                .await
                .map(|output| {
                    ToolOutput::new(
                        serde_json::to_value(output).expect("process output serializes"),
                    )
                })
        },
    )?;
    Ok(())
}

async fn run_process(
    context: ToolContext,
    mut command: Command,
    timeout: u64,
) -> Result<ProcessOutput, ToolError> {
    if !(1..=3_600).contains(&timeout) {
        return Err(ToolError::InvalidArguments(
            "timeout must be 1 through 3600".to_owned(),
        ));
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
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

    let stdout_task = tokio::spawn(read_stream(context.clone(), "stdout", stdout));
    let stderr_task = tokio::spawn(read_stream(context.clone(), "stderr", stderr));
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

    let deadline = tokio::time::sleep(Duration::from_secs(timeout));
    tokio::pin!(deadline);
    let (status, timed_out, cancelled) = tokio::select! {
        status = child.wait() => (status?, false, false),
        () = &mut deadline => {
            child.kill().await?;
            (child.wait().await?, true, false)
        }
        () = context.cancelled() => {
            child.kill().await?;
            (child.wait().await?, false, true)
        }
    };
    input_task.abort();
    let stdout = stdout_task
        .await
        .map_err(|error| ToolError::Failed(error.to_string()))??;
    let stderr = stderr_task
        .await
        .map_err(|error| ToolError::Failed(error.to_string()))??;
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
            format!("process timed out after {timeout} seconds"),
            ToolOutput::new(value),
        ));
    }
    Ok(output)
}

async fn read_stream<R>(
    context: ToolContext,
    kind: &'static str,
    mut stream: R,
) -> Result<Capture, ToolError>
where
    R: AsyncRead + Unpin,
{
    let mut capture = Capture::default();
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
    Ok(capture)
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
    /// Timeout in seconds.
    #[serde(default = "default_timeout")]
    #[schemars(range(min = 1, max = 3600))]
    timeout: u64,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ShellArgs {
    /// Command interpreted by the execution environment's shell.
    command: String,
    /// Working directory relative to the execution workspace by default.
    #[serde(default = "default_dot")]
    cwd: String,
    /// Timeout in seconds.
    #[serde(default = "default_timeout")]
    #[schemars(range(min = 1, max = 3600))]
    timeout: u64,
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
const fn default_timeout() -> u64 {
    60
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
