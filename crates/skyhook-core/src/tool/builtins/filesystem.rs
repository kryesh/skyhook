use diffy::{Patch, apply};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use tokio::fs;

use super::workspace::{
    atomic_write, relative_path, resolve_existing, resolve_removable, resolve_writable,
};
use crate::{
    media::{ImageReference, MAX_IMAGE_BYTES},
    session::SessionStore,
    tool::{
        PathKind, RegistryError, ToolError, ToolOptions, ToolOutput, ToolRegistryBuilder,
        policy::{Capability, PathAccess},
    },
};

const MAX_WRITE_BYTES: usize = 4 * 1024 * 1024;

pub(super) fn register(
    builder: &mut ToolRegistryBuilder,
    store: SessionStore,
) -> Result<(), RegistryError> {
    register_read(builder, store)?;
    register_writes(builder)
}

fn register_read(
    builder: &mut ToolRegistryBuilder,
    store: SessionStore,
) -> Result<(), RegistryError> {
    let schema = serde_json::to_value(schema_for!(ReadArgs)).expect("read schema serializes");
    let output_schema = serde_json::to_value(schema_for!(ReadOutput))
        .map_err(|error| RegistryError::Schema(error.to_string()))?;
    builder.register_dynamic(
        "read",
        "Capture a complete UTF-8 file or directory listing, or attach a supported image. Use job_output to page or search the saved snapshot.",
        schema,
        ToolOptions::new(vec![Capability::Read])
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

fn register_writes(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    builder.register::<WriteArgs, WriteOutput, _, _>(
        "write",
        "Atomically create or replace a UTF-8 workspace file.",
        ToolOptions::new(vec![Capability::Write])
            .placement(crate::tool::ToolPlacement::InheritWorkspace)
            .path_argument("path", PathAccess::Write, PathKind::Writable),
        |context, args| async move {
            check_write_size(&args.content)?;
            let path = resolve_writable(&context.execution_location.workspace, &args.path).await?;
            atomic_write(&path, args.content.as_bytes()).await?;
            Ok(WriteOutput {
                path: relative_path(&context.execution_location.workspace, &path),
                bytes: args.content.len(),
            })
        },
    )?;
    builder.register::<ReplaceArgs, EditOutput, _, _>(
        "replace",
        "Replace exact text in a UTF-8 workspace file with an expected match count.",
        ToolOptions::new(vec![Capability::Write])
            .placement(crate::tool::ToolPlacement::InheritWorkspace)
            .path_argument("path", PathAccess::Write, PathKind::Existing),
        |context, args| async move {
            if args.old.len().saturating_add(args.new.len()) > MAX_WRITE_BYTES {
                return Err(ToolError::InvalidArguments(format!(
                    "replace input exceeds the {MAX_WRITE_BYTES}-byte limit"
                )));
            }
            if args.old.is_empty() {
                return Err(ToolError::InvalidArguments(
                    "old cannot be empty".to_owned(),
                ));
            }
            let path = resolve_existing(&context.execution_location.workspace, &args.path).await?;
            let text = fs::read_to_string(&path).await?;
            let replacements = text.matches(&args.old).count();
            if replacements != args.count {
                return Err(ToolError::Failed(format!(
                    "expected {} matches, found {replacements}",
                    args.count
                )));
            }
            let output = text.replace(&args.old, &args.new);
            check_write_size(&output)?;
            atomic_write(&path, output.as_bytes()).await?;
            Ok(EditOutput {
                path: relative_path(&context.execution_location.workspace, &path),
                replacements,
                bytes: output.len(),
            })
        },
    )?;
    builder.register::<PatchArgs, EditOutput, _, _>(
        "patch",
        "Apply a unified patch to one UTF-8 workspace file.",
        ToolOptions::new(vec![Capability::Write])
            .placement(crate::tool::ToolPlacement::InheritWorkspace)
            .path_argument("path", PathAccess::Write, PathKind::Existing),
        |context, args| async move {
            check_write_size(&args.patch)?;
            let path = resolve_existing(&context.execution_location.workspace, &args.path).await?;
            let text = fs::read_to_string(&path).await?;
            let patch = Patch::from_str(&args.patch)
                .map_err(|error| ToolError::Failed(error.to_string()))?;
            let replacements = patch.hunks().len();
            let output =
                apply(&text, &patch).map_err(|error| ToolError::Failed(error.to_string()))?;
            check_write_size(&output)?;
            atomic_write(&path, output.as_bytes()).await?;
            Ok(EditOutput {
                path: relative_path(&context.execution_location.workspace, &path),
                replacements,
                bytes: output.len(),
            })
        },
    )?;
    builder.register::<RemoveArgs, RemoveOutput, _, _>(
        "remove",
        "Remove a workspace file, symlink, or directory.",
        ToolOptions::new(vec![Capability::Write])
            .placement(crate::tool::ToolPlacement::InheritWorkspace)
            .path_argument("path", PathAccess::Write, PathKind::Removable),
        |context, args| async move {
            let path = resolve_removable(&context.execution_location.workspace, &args.path).await?;
            let metadata = fs::symlink_metadata(&path).await?;
            let kind = if metadata.file_type().is_symlink() {
                fs::remove_file(&path).await?;
                "symlink"
            } else if metadata.is_file() {
                fs::remove_file(&path).await?;
                "file"
            } else if metadata.is_dir() {
                if args.recursive {
                    fs::remove_dir_all(&path).await?;
                } else {
                    fs::remove_dir(&path).await?;
                }
                "directory"
            } else {
                return Err(ToolError::Failed("unsupported filesystem entry".to_owned()));
            };
            Ok(RemoveOutput {
                path: relative_path(&context.execution_location.workspace, &path),
                kind: kind.to_owned(),
            })
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

fn check_write_size(text: &str) -> Result<(), ToolError> {
    if text.len() > MAX_WRITE_BYTES {
        Err(ToolError::Failed(format!(
            "write exceeds the {MAX_WRITE_BYTES}-byte limit"
        )))
    } else {
        Ok(())
    }
}

fn detect_image(bytes: &[u8]) -> Option<&'static str> {
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
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReadOutput {
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

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    /// File path.
    path: String,
    /// Complete replacement file contents.
    content: String,
}

#[derive(Serialize, JsonSchema)]
struct WriteOutput {
    path: String,
    bytes: usize,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReplaceArgs {
    /// UTF-8 text file to edit.
    path: String,
    /// Exact text to find.
    old: String,
    /// Replacement text.
    new: String,
    /// Required number of matches. The edit fails if the actual count differs.
    #[serde(default = "default_one")]
    count: usize,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PatchArgs {
    /// File the unified diff applies to.
    path: String,
    /// Unified diff hunks for that file.
    patch: String,
}

#[derive(Serialize, JsonSchema)]
struct EditOutput {
    path: String,
    replacements: usize,
    bytes: usize,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoveArgs {
    /// File or directory to remove.
    path: String,
    /// Required for non-empty directory trees.
    #[serde(default)]
    recursive: bool,
}

#[derive(Serialize, JsonSchema)]
struct RemoveOutput {
    path: String,
    kind: String,
}

const fn default_one() -> usize {
    1
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{
        identity::AgentId,
        job::JobManager,
        session::SessionStore,
        tool::{
            ToolRegistryBuilder,
            executor::ToolExecutor,
            policy::{
                AllowAll, AuthorizationRequest, Capability, Policy, PolicyDecision, PolicyFuture,
            },
        },
    };
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn grouped_directories_keep_sizes_and_recover_truncated_entries() {
        use serde_json::json;
        let runtime = crate::test_support::TestRuntime::new().await;
        let directory = runtime.root.path().join("files");
        fs::create_dir(&directory).await.unwrap();
        fs::create_dir(directory.join("nested")).await.unwrap();
        fs::write(directory.join("empty"), "").await.unwrap();
        fs::write(directory.join("utf8"), "λ").await.unwrap();
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, runtime.store.clone()).unwrap();
        let executor = runtime.executor(builder);
        let small = executor
            .execute(runtime.agent.clone(), "read", json!({"path":"files"}), None)
            .await
            .unwrap();
        assert_eq!(
            small.output.value,
            json!({"kind":"directory","path":"files","entries":{
                "files":[{"name":"empty","bytes":0},{"name":"utf8","bytes":2}], "directories":["nested"]
            }})
        );
        let detailed = executor
            .execute(
                runtime.agent.clone(),
                "read",
                json!({"path":"files","details":true}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            detailed.output.value["entries"][0],
            json!({"name":"empty","kind":"file","bytes":0})
        );
        assert!(detailed.output.value["entries"][1].get("bytes").is_none());
        for index in 0..180 {
            fs::write(
                directory.join(format!("file-{index:03}")),
                vec![b'x'; index],
            )
            .await
            .unwrap();
        }
        let captured = executor
            .execute_model(runtime.agent.clone(), "read", json!({"path":"files"}), None)
            .await
            .unwrap();
        let view = captured.output.value;
        assert_eq!(view["truncated"][0]["field"], "/result/entries");
        assert!(
            serde_json::to_vec(&view["result"]["entries"])
                .unwrap()
                .len()
                <= 2048
        );
        assert!(
            view["result"]["entries"]["files"]
                .as_array()
                .unwrap()
                .iter()
                .all(|file| file["bytes"].is_u64())
        );
        let mut query = crate::job::JobOutputQuery::new(captured.job);
        query.field = Some("/result/entries".into());
        query.pattern = Some("file-179".into());
        let page = runtime
            .jobs
            .inspect_output(query, &Default::default())
            .await
            .unwrap();
        assert!(
            page["preview"]["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| line.as_str().unwrap().contains("file-179"))
        );
        let complete = runtime
            .jobs
            .snapshot(captured.job)
            .await
            .unwrap()
            .output
            .unwrap();
        assert_eq!(complete["entries"]["files"].as_array().unwrap().len(), 182);
        assert_eq!(complete["entries"]["files"][180]["bytes"], 179);
    }

    #[derive(Default)]
    struct RecordingPolicy {
        requests: Mutex<Vec<AuthorizationRequest>>,
    }

    impl Policy for RecordingPolicy {
        fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_> {
            Box::pin(async move {
                self.requests.lock().await.push(request);
                PolicyDecision::allow()
            })
        }
    }

    #[tokio::test]
    async fn read_captures_complete_snapshot() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("lines.txt"), "one\ntwo\nthree\n")
            .await
            .unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let jobs = JobManager::new(store.clone());
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, store.clone()).unwrap();
        let executor = ToolExecutor::new(
            builder.build(),
            Arc::new(AllowAll),
            jobs.clone(),
            root.path().to_owned(),
        );
        let result = executor
            .execute(
                AgentId::root(store.id()),
                "read",
                serde_json::json!({"path":"lines.txt"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.output.value["content"], "one\ntwo\nthree\n");
        fs::write(root.path().join("lines.txt"), "changed")
            .await
            .unwrap();
        let mut args = crate::job::output::OutputArgs::new(result.job);
        args.field = Some("/result/content".into());
        args.start = Some(2);
        args.limit = Some(1);
        let page = jobs
            .present_output(args, &Default::default())
            .await
            .unwrap();
        assert_eq!(page["preview"]["lines"][0], "two");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_replacement_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("executable");
        fs::write(&path, "old").await.unwrap();
        fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751))
            .await
            .unwrap();
        atomic_write(&path, b"new").await.unwrap();
        assert_eq!(
            fs::metadata(path).await.unwrap().permissions().mode() & 0o777,
            0o751
        );
    }

    #[tokio::test]
    async fn absolute_and_child_workspace_paths_are_authorized_against_the_root() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let child_workspace = root.path().join("child");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&child_workspace).unwrap();
        std::fs::write(child_workspace.join("input.txt"), "child").unwrap();
        let outside = root.path().join("outside.txt");
        std::fs::write(&outside, "outside").unwrap();
        let sessions = root.path().join("sessions");
        let store = SessionStore::create(&sessions).await.unwrap();
        let agent = AgentId::root(store.id());
        let jobs = JobManager::new(store.clone());
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, store).unwrap();
        let policy = Arc::new(RecordingPolicy::default());
        let workspace = std::fs::canonicalize(workspace).unwrap();
        let executor = ToolExecutor::new(builder.build(), policy.clone(), jobs, workspace.clone());

        let read = executor
            .execute(
                agent.clone(),
                "read",
                serde_json::json!({"path": outside}),
                None,
            )
            .await
            .unwrap();
        let outside = std::fs::canonicalize(root.path().join("outside.txt")).unwrap();
        assert_eq!(
            read.output.value["path"],
            outside.to_string_lossy().as_ref()
        );
        let requests = policy.requests.lock().await;
        let permission = &requests.last().unwrap().permissions[0];
        assert_eq!(permission.capability, Capability::Read);
        assert_eq!(
            permission.resource,
            crate::tool::policy::ResourceId::path("root", &outside)
        );
        drop(requests);

        let destination = root.path().join("new-outside.txt");
        executor
            .execute(
                agent.clone(),
                "write",
                serde_json::json!({"path": destination, "content": "new"}),
                None,
            )
            .await
            .unwrap();
        let requests = policy.requests.lock().await;
        let permission = &requests.last().unwrap().permissions[0];
        assert_eq!(permission.capability, Capability::Write);
        assert_eq!(permission.resource.namespace, "path");
        drop(requests);

        let child_workspace = std::fs::canonicalize(child_workspace).unwrap();
        let child_executor =
            executor
                .clone()
                .with_location(crate::execution::ExecutionLocation::root(
                    child_workspace.clone(),
                ));
        let read = child_executor
            .execute(
                agent,
                "read",
                serde_json::json!({"path": "input.txt"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(read.output.value["path"], "input.txt");
        let requests = policy.requests.lock().await;
        assert!(requests.last().unwrap().permissions.iter().any(
            |permission| permission.capability == Capability::Read
                && permission.resource
                    == crate::tool::policy::ResourceId::path(
                        "root",
                        &child_workspace.join("input.txt"),
                    )
        ));
    }

    #[tokio::test]
    async fn remove_requires_recursion_and_refuses_the_workspace_root() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        fs::create_dir(workspace.path().join("directory"))
            .await
            .unwrap();
        fs::write(workspace.path().join("directory/file"), "data")
            .await
            .unwrap();
        let store = SessionStore::create(sessions.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let jobs = JobManager::new(store.clone());
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, store).unwrap();
        let executor = ToolExecutor::new(
            builder.build(),
            Arc::new(AllowAll),
            jobs,
            workspace.path().to_path_buf(),
        );
        let error = executor
            .execute(
                agent.clone(),
                "remove",
                serde_json::json!({"path":"directory"}),
                None,
            )
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("not empty"));
        executor
            .execute(
                agent.clone(),
                "remove",
                serde_json::json!({"path":"directory", "recursive":true}),
                None,
            )
            .await
            .unwrap();
        assert!(
            !fs::try_exists(workspace.path().join("directory"))
                .await
                .unwrap()
        );
        #[cfg(unix)]
        {
            std::fs::write(workspace.path().join("target"), "safe").unwrap();
            std::os::unix::fs::symlink("target", workspace.path().join("link")).unwrap();
            executor
                .execute(
                    agent.clone(),
                    "remove",
                    serde_json::json!({"path":"link"}),
                    None,
                )
                .await
                .unwrap();
            assert!(
                fs::try_exists(workspace.path().join("target"))
                    .await
                    .unwrap()
            );
            assert!(!fs::try_exists(workspace.path().join("link")).await.unwrap());
        }
        let error = executor
            .execute(agent, "remove", serde_json::json!({"path":"."}), None)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("workspace root"));
    }
}
