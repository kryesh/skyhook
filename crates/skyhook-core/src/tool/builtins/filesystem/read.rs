//! File snapshots, directory listings, images, and expected read failures.

use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use tokio::fs;

use super::super::workspace::{relative_path, resolve_existing};
use crate::{
    media::{ImageReference, MAX_IMAGE_BYTES},
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
    let schema = serde_json::to_value(schema_for!(ReadArgs)).expect("read schema serializes");
    let output_schema = serde_json::to_value(schema_for!(ReadOutput))
        .map_err(|error| RegistryError::Schema(error.to_string()))?;
    builder.register_dynamic(
        "read",
        "Capture a complete UTF-8 file or directory listing, or attach a supported image. Use job_output to page or search the saved snapshot. Missing or OS-inaccessible paths return {kind:\"error\",path,error:{code:\"not_found\"|\"permission_denied\",message}} as a completed result; policy/approval denials remain denials.",
        schema,
        ToolOptions::new(vec![Capability::Read])
            .read_error_output(read_error_output)
            .placement(crate::tool::ToolPlacement::TargetedWorkspace)
            .output_schema(output_schema)
            .path_argument("path", PathAccess::Read, PathKind::Existing),
        move |context, arguments| {
            let store = store.clone();
            async move {
                let args: ReadArgs = serde_json::from_value(arguments)
                    .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
                let path =
                    resolve_existing(&context.execution_location.workspace, &args.path).await?;
                if fs::metadata(&path).await?.is_dir() {
                    let mut directory = fs::read_dir(&path).await?;
                    let mut entries = Vec::new();
                    while let Some(entry) = directory.next_entry().await? {
                        let metadata = fs::symlink_metadata(entry.path()).await?;
                        entries.push(DirectoryEntry {
                            name: entry.file_name().to_string_lossy().into_owned(),
                            kind: if metadata.file_type().is_symlink() {
                                "symlink"
                            } else if metadata.is_dir() {
                                "directory"
                            } else if metadata.is_file() {
                                "file"
                            } else {
                                "other"
                            }
                            .to_owned(),
                            bytes: metadata.is_file().then_some(metadata.len()),
                        });
                    }
                    entries.sort_by(|left, right| left.name.cmp(&right.name));
                    let directory_path =
                        relative_path(&context.execution_location.workspace, &path);
                    return Ok(ToolOutput::new(serde_json::to_value(
                        ReadOutput::Directory {
                            path: directory_path,
                            entries: if args.details { DirectoryEntries::Detailed(entries) } else { DirectoryEntries::Grouped(DirectoryGroups::from(entries)) },
                        },
                    )?));
                }

                let output_path = relative_path(&context.execution_location.workspace, &path);
                match read_text(&path, output_path.clone(), &context).await {
                    Ok(output) => {
                        return Ok(ToolOutput::new(serde_json::to_value(output)?));
                    }
                    Err(ToolError::Io(error))
                        if error.kind() == std::io::ErrorKind::InvalidData => {}
                    Err(error) => return Err(error),
                }

                let metadata = fs::metadata(&path).await?;
                if metadata.len() > MAX_IMAGE_BYTES {
                    return Err(ToolError::Failed(format!(
                        "non-UTF-8 file exceeds the {MAX_IMAGE_BYTES}-byte image limit"
                    )));
                }
                let bytes = fs::read(&path).await?;
                let media_type = detect_image(&bytes).ok_or_else(|| {
                    ToolError::Failed("file is neither UTF-8 nor a supported image".to_owned())
                })?;
                if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_IMAGE_BYTES {
                    return Err(ToolError::Failed(format!(
                        "image exceeds the {MAX_IMAGE_BYTES}-byte limit"
                    )));
                }
                let name = path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .ok_or_else(|| ToolError::Failed("image filename is not UTF-8".to_owned()))?
                    .to_owned();
                let reference = store
                    .import_blob(&bytes, name, media_type.to_owned())
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

async fn read_text(
    path: &std::path::Path,
    output_path: String,
    context: &crate::tool::ToolContext,
) -> Result<ReadOutput, ToolError> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let capture = context.capture_path("/result/content").await?;
    let mut input = fs::File::open(path).await?;
    let mut output = fs::File::create(&capture).await?;
    let result = async {
        let mut buffer = vec![0; 64 * 1024];
        let mut pending = Vec::new();
        loop {
            if context.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let size = input.read(&mut buffer).await?;
            pending.extend_from_slice(&buffer[..size]);
            match std::str::from_utf8(&pending) {
                Ok(_) => {
                    output.write_all(&pending).await?;
                    pending.clear();
                }
                Err(error) if error.error_len().is_none() && size != 0 => {
                    let valid = error.valid_up_to();
                    output.write_all(&pending[..valid]).await?;
                    pending.drain(..valid);
                }
                Err(_) => {
                    return Err(ToolError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "file is not UTF-8",
                    )));
                }
            }
            if size == 0 {
                break;
            }
        }
        output.flush().await?;
        Ok(ReadOutput::File {
            path: output_path,
            content: String::new(),
        })
    }
    .await;
    if result.is_err() {
        drop(output);
        let _ = fs::remove_file(capture).await;
    }
    result
}

pub(in crate::tool::builtins) fn detect_image(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
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
        image: ImageReference,
    },
}

#[derive(Clone, Serialize, JsonSchema)]
struct DirectoryEntry {
    name: String,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<u64>,
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
            match entry.kind.as_str() {
                "file" => groups.files.push(DirectoryFile {
                    name: entry.name,
                    bytes: entry.bytes.expect("regular file size"),
                }),
                "directory" => groups.directories.push(entry.name),
                "symlink" => groups.symlinks.push(entry.name),
                _ => groups.other.push(entry.name),
            }
        }
        groups
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::tool::executor::ToolExecutor;

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

    #[tokio::test]
    async fn missing_reads_complete_direct_model_and_script_jobs() {
        use serde_json::json;
        let runtime = crate::tests::TestRuntime::new().await;
        let (executor, _slot) = read_executor(&runtime);
        for path in ["missing", "missing/nested/file.txt"] {
            let direct = executor
                .execute(runtime.agent.clone(), "read", json!({"path":path}), None)
                .await
                .unwrap();
            assert_eq!(direct.output.value["kind"], "error");
            assert_eq!(direct.output.value["path"], path);
            assert_eq!(direct.output.value["error"]["code"], "not_found");
            assert!(
                direct.output.value["error"]["message"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
            );
            assert_eq!(
                runtime.jobs.snapshot(direct.job).await.unwrap().state,
                crate::job::JobState::Completed
            );
            let model = executor
                .execute_model(runtime.agent.clone(), "read", json!({"path":path}), None)
                .await
                .unwrap();
            assert_eq!(model.output.value["state"], "completed");
            assert_eq!(model.output.value["result"]["error"]["code"], "not_found");
            let script = executor.execute(runtime.agent.clone(), "script", json!({"source": format!("const result = await tool.read({{path:{}}}); return {{resolved:true, result}};", serde_json::to_string(path).unwrap())}), None).await.unwrap();
            assert_eq!(script.output.value["value"]["resolved"], true);
            assert_eq!(
                script.output.value["value"]["result"]["error"]["code"],
                "not_found"
            );
            assert_eq!(
                runtime.jobs.snapshot(script.job).await.unwrap().state,
                crate::job::JobState::Completed
            );
        }
    }

    #[tokio::test]
    async fn read_captures_complete_snapshot() {
        let runtime = crate::tests::TestRuntime::new().await;
        let (executor, _slot) = read_executor(&runtime);
        let path = runtime.root.path().join("lines.txt");
        fs::write(&path, "one\ntwo\nthree\n").await.unwrap();
        let result = executor
            .execute(
                runtime.agent.clone(),
                "read",
                serde_json::json!({"path":"lines.txt"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.output.value["content"], "one\ntwo\nthree\n");
        fs::write(&path, "changed").await.unwrap();
        let mut args = crate::job::output::OutputArgs::new(result.job);
        args.field = Some("/result/content".into());
        args.start = Some(2);
        args.limit = Some(1);
        let page = runtime
            .jobs
            .present_output(args, &Default::default())
            .await
            .unwrap();
        assert_eq!(page["preview"]["lines"][0], "two");
    }
}
