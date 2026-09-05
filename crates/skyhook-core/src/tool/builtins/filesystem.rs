use diffy::{Patch, apply};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    io::{AsyncBufReadExt as _, BufReader},
};

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

const MAX_READ_LINES: usize = 2_000;
const MAX_READ_BYTES: usize = 1024 * 1024;
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
        "Read UTF-8 lines, list a directory, or attach a supported image from the workspace.",
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
                            path: String::new(),
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
                    validate_read_range(args.start, args.limit)?;
                    let directory_path =
                        relative_path(&context.execution_location.workspace, &path);
                    for entry in &mut entries {
                        if directory_path == "." {
                            entry.name.clone_into(&mut entry.path);
                        } else {
                            entry.path = format!("{directory_path}/{}", entry.name);
                        }
                    }
                    let first = args.start - 1;
                    let end_index = first.saturating_add(args.limit).min(entries.len());
                    let selected = if first < entries.len() {
                        entries[first..end_index].to_vec()
                    } else {
                        Vec::new()
                    };
                    let visible = selected.len();
                    return Ok(ToolOutput::new(serde_json::to_value(
                        ReadOutput::Directory {
                            path: directory_path,
                            entries: selected,
                            start: args.start,
                            end: range_end(args.start, visible),
                            truncated: end_index < entries.len(),
                        },
                    )?));
                }

                validate_read_range(args.start, args.limit)?;
                let output_path = relative_path(&context.execution_location.workspace, &path);
                match read_text_range(&path, output_path.clone(), args.start, args.limit).await {
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

async fn read_text_range(
    path: &std::path::Path,
    output_path: String,
    start: usize,
    limit: usize,
) -> Result<ReadOutput, ToolError> {
    let mut reader = BufReader::new(fs::File::open(path).await?);
    let mut line = String::new();
    for _ in 1..start {
        if reader.read_line(&mut line).await? == 0 {
            return Ok(ReadOutput::File {
                path: output_path,
                content: String::new(),
                start,
                end: 0,
                truncated: false,
            });
        }
        line.clear();
    }
    let mut content = String::new();
    let mut visible = 0;
    let mut byte_truncated = false;
    while visible < limit {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        while line.ends_with('\n') || line.ends_with('\r') {
            line.pop();
        }
        let separator = usize::from(visible > 0);
        let available = MAX_READ_BYTES.saturating_sub(content.len());
        if separator.saturating_add(line.len()) > available {
            let mut remaining = available;
            if separator == 1 && remaining > 0 {
                content.push('\n');
                remaining -= 1;
            }
            let mut boundary = remaining.min(line.len());
            while !line.is_char_boundary(boundary) {
                boundary -= 1;
            }
            content.push_str(&line[..boundary]);
            visible += 1;
            byte_truncated = true;
            break;
        }
        if separator == 1 {
            content.push('\n');
        }
        content.push_str(&line);
        visible += 1;
    }
    line.clear();
    let more = byte_truncated || (visible == limit && reader.read_line(&mut line).await? != 0);
    Ok(ReadOutput::File {
        path: output_path,
        content,
        start,
        end: range_end(start, visible),
        truncated: more,
    })
}

fn validate_read_range(start: usize, limit: usize) -> Result<(), ToolError> {
    if start == 0 || !(1..=MAX_READ_LINES).contains(&limit) {
        return Err(ToolError::InvalidArguments(format!(
            "start must be at least 1 and limit must be 1 through {MAX_READ_LINES}"
        )));
    }
    Ok(())
}

const fn range_end(start: usize, visible: usize) -> usize {
    if visible == 0 { 0 } else { start + visible - 1 }
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
    /// File or directory path, relative to the selected workspace by default.
    path: String,
    /// One-based first line or directory entry to return.
    #[serde(default = "default_one")]
    #[schemars(range(min = 1))]
    start: usize,
    /// Maximum number of lines or directory entries to return.
    #[serde(default = "default_read_lines")]
    #[schemars(range(min = 1, max = 2000))]
    limit: usize,
}

#[derive(Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReadOutput {
    File {
        path: String,
        content: String,
        start: usize,
        end: usize,
        truncated: bool,
    },
    Directory {
        path: String,
        entries: Vec<DirectoryEntry>,
        start: usize,
        end: usize,
        truncated: bool,
    },
    Image {
        path: String,
        image: ImageReference,
    },
}

#[derive(Clone, Serialize, JsonSchema)]
struct DirectoryEntry {
    name: String,
    #[serde(default)]
    path: String,
    kind: String,
    bytes: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    /// File path, relative to the selected workspace by default.
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
const fn default_read_lines() -> usize {
    200
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
    async fn text_ranges_are_one_based_and_report_lookahead() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("lines.txt");
        fs::write(&path, "one\ntwo\nthree\n").await.unwrap();
        let output = read_text_range(&path, "lines.txt".to_owned(), 2, 1)
            .await
            .unwrap();
        match output {
            ReadOutput::File {
                path,
                content,
                start,
                end,
                truncated,
            } => {
                assert_eq!(path, "lines.txt");
                assert_eq!(content, "two");
                assert_eq!((start, end), (2, 2));
                assert!(truncated);
            }
            ReadOutput::Directory { .. } | ReadOutput::Image { .. } => {
                panic!("expected file output")
            }
        }
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
