//! File snapshots, directory listings, images, and expected read failures.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::fs;

use super::super::workspace::relative_path;
use crate::{
    bounded_io::{BoundedReadError, read_bounded},
    job::output::{CaptureWriter, CompletedCapture, TextCaptureField},
    media::{ImageRef, MAX_IMAGE_BYTES},
    session::SessionStore,
    tool::{
        PathKind, RegistryError, ToolError, ToolOptions, ToolOutput, ToolRegistryBuilder,
        policy::{Capability, PathAccess},
    },
};

pub(super) fn register(
    builder: &mut ToolRegistryBuilder,
    store: SessionStore,
) -> Result<(), RegistryError> {
    builder.register_product::<ReadArgs, ReadOutput, _, _>(
        "read",
        "Capture a complete UTF-8 file or directory listing, or attach a supported image. Use job_output to page or search the saved snapshot. Missing or OS-inaccessible paths return {kind:\"error\",path,error:{code:\"not_found\"|\"permission_denied\",message}} as a completed result; policy/approval denials remain denials.",
        ToolOptions::new(vec![Capability::Read])
            .read_error_output(read_error_output)
            .placement(crate::tool::ToolPlacement::TargetedWorkspace)
            .path_argument("path", PathAccess::Read, PathKind::Existing),
        move |context, args| {
            let store = store.clone();
            async move {
                let path = std::path::PathBuf::from(&args.path);
                if fs::metadata(&path).await?.is_dir() {
                    let mut directory = fs::read_dir(&path).await?;
                    let mut entries = Vec::new();
                    while let Some(entry) = directory.next_entry().await? {
                        let metadata = fs::symlink_metadata(entry.path()).await?;
                        entries.push(DirectoryEntry::from_metadata(
                            entry.file_name().to_string_lossy().into_owned(), &metadata,
                        ));
                    }
                    entries.sort_by(|left, right| left.name().cmp(right.name()));
                    let directory_path =
                        relative_path(&context.execution_location().workspace, &path);
                    return Ok(ToolOutput::new(serde_json::to_value(
                        ReadOutput::Directory {
                            path: directory_path,
                            entries: if args.details { DirectoryEntries::Detailed(entries) } else { DirectoryEntries::Grouped(DirectoryGroups::from(entries)) },
                        },
                    )?));
                }

                let output_path = relative_path(&context.execution_location().workspace, &path);
                match read_text(&path, &context).await {
                    Ok(TextReadOutcome::Captured(capture)) => {
                        return Ok(ToolOutput::new(serde_json::to_value(ReadOutput::File {
                            path: output_path,
                            content: String::new(),
                        })?)
                        .with_captures(vec![capture]));
                    }
                    Ok(TextReadOutcome::NotUtf8) => {}
                    Err(error) => return Err(error),
                }

                let metadata = fs::metadata(&path).await?;
                if metadata.len() > MAX_IMAGE_BYTES {
                    return Err(ToolError::Failed(format!(
                        "non-UTF-8 file exceeds the {MAX_IMAGE_BYTES}-byte image limit"
                    )));
                }
                let mut input = fs::File::open(&path).await?;
                let bytes = read_bounded(&mut input, MAX_IMAGE_BYTES as usize).await
                    .map_err(|error| match error {
                        BoundedReadError::Io(error) => ToolError::Io(error),
                        error => ToolError::Failed(error.to_string()),
                    })?;
                let image = crate::media::Image::new(bytes).map_err(|_| {
                    ToolError::Failed("file is neither UTF-8 nor a supported image".to_owned())
                })?;
                let reference = store
                    .store_image(Some(output_path.clone()), &image)
                    .await
                    .map_err(|error| ToolError::Failed(error.to_string()))?;
                Ok(ToolOutput::new(serde_json::to_value(ReadOutput::Image {
                    path: output_path,
                    image: reference.clone(),
                })?)
                .with_images(vec![reference]))
            }
        },
    )?;
    Ok(())
}

enum TextReadOutcome {
    Captured(CompletedCapture),
    NotUtf8,
}

async fn read_text(
    path: &std::path::Path,
    context: &crate::tool::ToolContext,
) -> Result<TextReadOutcome, ToolError> {
    let mut capture = context
        .text_capture(TextCaptureField::Content)
        .await?
        .open();
    let path = path.to_owned();
    let cancellation = context.cancellation_token().child_token();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    // All filesystem operations and cleanup run in one owner. Tokio fs creation or
    // writes in separate tasks could otherwise finish after a dropped guard's unlink.
    let pending =
        tokio::task::spawn_blocking(move || -> Result<Option<CaptureWriter>, ToolError> {
            let mut input = std::fs::File::open(path)?;
            match copy_utf8(&mut input, |text| capture.write_text(text), &cancellation)? {
                Utf8Read::Complete => Ok(Some(capture)),
                Utf8Read::NotUtf8 => Ok(None),
            }
        })
        .await
        .map_err(|error| ToolError::Failed(error.to_string()))??;
    // Every write_text flushed, so finishing here is synchronous and performs no
    // blocking IO; a dropped awaiter still discards the returned writer's file.
    if let Some(capture) = pending {
        Ok(TextReadOutcome::Captured(capture.finish()?))
    } else {
        Ok(TextReadOutcome::NotUtf8)
    }
}

#[derive(Debug, PartialEq)]
enum Utf8Read {
    Complete,
    NotUtf8,
}

fn copy_utf8(
    input: &mut impl std::io::Read,
    mut output: impl FnMut(&str) -> std::io::Result<()>,
    cancellation: &crate::job::CancellationToken,
) -> Result<Utf8Read, ToolError> {
    let mut buffer = [0; 64 * 1024];
    let mut pending = Vec::new();
    loop {
        if cancellation.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let size = input.read(&mut buffer)?;
        if cancellation.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        pending.extend_from_slice(&buffer[..size]);
        match std::str::from_utf8(&pending) {
            Ok(text) => {
                output(text)?;
                pending.clear();
            }
            Err(error) if error.error_len().is_none() && size != 0 => {
                let valid = error.valid_up_to();
                output(std::str::from_utf8(&pending[..valid]).expect("validated UTF-8 prefix"))?;
                pending.drain(..valid);
            }
            Err(_) => return Ok(Utf8Read::NotUtf8),
        }
        if size == 0 {
            break;
        }
    }
    Ok(Utf8Read::Complete)
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    /// File or directory path.
    path: String,
    /// Include explicit entry kinds in a flat list. File sizes are always included.
    #[serde(default)]
    details: bool,
}

#[derive(Serialize, JsonSchema)]
struct ReadError {
    code: ReadErrorCode,
    message: String,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ReadErrorCode {
    NotFound,
    PermissionDenied,
}

fn read_error_output(path: &str, error: &ToolError) -> Option<ToolOutput> {
    let ToolError::Io(error) = error else {
        return None;
    };
    let code = match error.kind() {
        std::io::ErrorKind::NotFound => ReadErrorCode::NotFound,
        std::io::ErrorKind::PermissionDenied => ReadErrorCode::PermissionDenied,
        _ => return None,
    };
    Some(ToolOutput::new(
        serde_json::to_value(ReadOutput::Error {
            path: path.to_owned(),
            error: ReadError {
                code,
                message: error.to_string(),
            },
        })
        .expect("read errors serialize"),
    ))
}

#[derive(Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReadOutput {
    /// Expected filesystem failures, not policy or approval denials.
    Error {
        path: String,
        error: ReadError,
    },
    File {
        path: String,
        #[schemars(extend("x-skyhook-truncatable" = true))]
        content: String,
    },
    Directory {
        path: String,
        #[schemars(extend("x-skyhook-truncatable" = true))]
        entries: DirectoryEntries,
    },
    Image {
        path: String,
        image: ImageRef,
    },
}

#[derive(Clone, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DirectoryEntry {
    File { name: String, bytes: u64 },
    Directory { name: String },
    Symlink { name: String },
    Other { name: String },
}

impl DirectoryEntry {
    fn from_metadata(name: String, metadata: &std::fs::Metadata) -> Self {
        if metadata.file_type().is_symlink() {
            Self::Symlink { name }
        } else if metadata.is_dir() {
            Self::Directory { name }
        } else if metadata.is_file() {
            Self::File {
                name,
                bytes: metadata.len(),
            }
        } else {
            Self::Other { name }
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::File { name, .. }
            | Self::Directory { name }
            | Self::Symlink { name }
            | Self::Other { name } => name,
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[serde(untagged)]
enum DirectoryEntries {
    Grouped(DirectoryGroups),
    Detailed(Vec<DirectoryEntry>),
}

#[derive(Default, Serialize, JsonSchema)]
struct DirectoryGroups {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    files: Vec<DirectoryFile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    directories: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    symlinks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    other: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
struct DirectoryFile {
    name: String,
    bytes: u64,
}

impl From<Vec<DirectoryEntry>> for DirectoryGroups {
    fn from(entries: Vec<DirectoryEntry>) -> Self {
        let mut groups = Self::default();
        for entry in entries {
            match entry {
                DirectoryEntry::File { name, bytes } => {
                    groups.files.push(DirectoryFile { name, bytes })
                }
                DirectoryEntry::Directory { name } => groups.directories.push(name),
                DirectoryEntry::Symlink { name } => groups.symlinks.push(name),
                DirectoryEntry::Other { name } => groups.other.push(name),
            }
        }
        groups
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::job::{CancellationToken, JobState};
    use crate::tool::executor::{ExecutionError, ToolExecutor};

    fn read_executor(
        runtime: &crate::tests::TestRuntime,
    ) -> (ToolExecutor, Arc<std::sync::OnceLock<ToolExecutor>>) {
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, runtime.store.clone()).unwrap();
        let slot = Arc::new(std::sync::OnceLock::new());
        crate::tool::builtins::install_script_tool(&mut builder, Arc::downgrade(&slot)).unwrap();
        let executor = runtime.executor(builder);
        assert!(slot.set(executor.clone()).is_ok());
        (executor, slot)
    }

    /// Schema-valid input rejected by typed admission still owns a job, which
    /// fails with the admission error before any IO or script evaluation.
    #[tokio::test]
    async fn typed_read_and_script_admission_fails_the_job_before_io_or_evaluation() {
        let runtime = crate::tests::TestRuntime::new().await;
        let (executor, _slot) = read_executor(&runtime);
        for (index, (tool, arguments)) in [
            ("read", json!({"path":".", "details":"invalid"})),
            ("script", json!({"source":42})),
        ]
        .into_iter()
        .enumerate()
        {
            let error = executor.run_host(&runtime.agent, tool, arguments).await;
            let error =
                error.expect_err("typed arguments must reject before IO or script evaluation");
            assert!(
                matches!(&error, ExecutionError::Failed { message, .. } if message.starts_with("invalid tool arguments:")),
                "{error}"
            );
            let jobs = runtime.jobs.list(&runtime.agent).await;
            assert_eq!(jobs.len(), index + 1);
            assert!(jobs.iter().all(|job| job.state == JobState::Failed));
        }
    }

    #[cfg(unix)]
    #[test]
    fn directory_classification_uses_entry_metadata_not_symlink_referent() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), b"abc").unwrap();
        std::fs::create_dir(root.path().join("directory")).unwrap();
        std::os::unix::fs::symlink(root.path().join("file"), root.path().join("link")).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(root.path().join("socket")).unwrap();
        for (name, kind) in [
            ("file", "file"),
            ("directory", "directory"),
            ("link", "symlink"),
            ("socket", "other"),
        ] {
            let metadata = std::fs::symlink_metadata(root.path().join(name)).unwrap();
            let entry = DirectoryEntry::from_metadata(name.into(), &metadata);
            assert_eq!(entry.name(), name);
            assert_eq!(serde_json::to_value(entry).unwrap()["kind"], kind);
        }
    }

    /// Invalid UTF-8 is not an IO error, real invalid data stays IO, and
    /// cancellation before or during a read is typed.
    #[test]
    fn text_copy_classifies_utf8_io_and_cancellation() {
        struct Reader(Option<CancellationToken>);
        impl std::io::Read for Reader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let Some(cancellation) = &self.0 else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "device data error",
                    ));
                };
                cancellation.cancel();
                buf[0] = b'x';
                Ok(1)
            }
        }
        let cancellation = CancellationToken::new();
        let result = copy_utf8(&mut &b"valid\xff"[..], |_| Ok(()), &cancellation);
        assert_eq!(result.unwrap(), Utf8Read::NotUtf8);
        assert!(
            matches!(copy_utf8(&mut Reader(None), |_| Ok(()), &cancellation), Err(ToolError::Io(error)) if error.kind() == std::io::ErrorKind::InvalidData)
        );
        let during = Reader(Some(cancellation.clone()));
        assert!(matches!(
            copy_utf8(&mut { during }, |_| Ok(()), &cancellation),
            Err(ToolError::Cancelled)
        ));
        assert!(matches!(
            copy_utf8(&mut &b"data"[..], |_| Ok(()), &cancellation),
            Err(ToolError::Cancelled)
        ));
    }

    #[tokio::test]
    async fn missing_reads_complete_direct_model_and_script_jobs() {
        let runtime = crate::tests::TestRuntime::new().await;
        let (executor, _slot) = read_executor(&runtime);
        let agent = &runtime.agent;
        let state = async |job| runtime.jobs.snapshot(job).await.unwrap().state;
        for path in ["missing", "missing/nested/file.txt"] {
            let direct = executor
                .run_host(agent, "read", json!({"path":path}))
                .await
                .unwrap();
            let value = &direct.output.value;
            assert_eq!(
                (&value["kind"], &value["path"]),
                (&json!("error"), &json!(path))
            );
            assert_eq!(value["error"]["code"], "not_found");
            assert!(
                value["error"]["message"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
            );
            assert_eq!(state(direct.job).await, JobState::Completed);
            let model = executor
                .run_model(agent, "read", json!({"path":path}))
                .await
                .unwrap();
            assert_eq!(model.output.value["state"], "completed");
            assert_eq!(model.output.value["result"]["error"]["code"], "not_found");
            let path = serde_json::to_string(path).unwrap();
            let source = format!(
                "const result = await tool.read({{path:{path}}}); return {{resolved:true, result}};"
            );
            let script = executor
                .run_host(agent, "script", json!({"source": source}))
                .await
                .unwrap();
            assert_eq!(script.output.value["value"]["resolved"], true);
            assert_eq!(
                script.output.value["value"]["result"]["error"]["code"],
                "not_found"
            );
            assert_eq!(state(script.job).await, JobState::Completed);
        }
    }

    #[tokio::test]
    async fn read_captures_complete_snapshot() {
        let runtime = crate::tests::TestRuntime::new().await;
        let (executor, _slot) = read_executor(&runtime);
        let path = runtime.root.path().join("lines.txt");
        fs::write(&path, "one\ntwo\nthree\n").await.unwrap();
        let result = executor
            .run_host(&runtime.agent, "read", json!({"path":"lines.txt"}))
            .await
            .unwrap();
        assert_eq!(result.output.value["content"], "one\ntwo\nthree\n");
        fs::write(&path, "changed").await.unwrap();
        let mut args = crate::job::output::OutputArgs::new(result.job);
        (args.field, args.start, args.limit) = (Some("/result/content".into()), Some(2), Some(1));
        let page = runtime
            .jobs
            .present_output(args, &Default::default())
            .await
            .unwrap();
        assert_eq!(page["preview"]["lines"][0], "two");
    }
}
