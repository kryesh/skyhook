//! Atomic writes, exact replacements, unified patches, and removals.
use crate::tool::ToolOptions;
use crate::tool::invocation::{LocalCatalogBuilder, LocalError};

use diffy::{Patch, apply};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::fs;

use super::super::workspace::{atomic_write, relative_path};
use crate::tool::{
    PathKind, RegistryError,
    policy::{Capability, PathAccess},
};

const MAX_WRITE_BYTES: usize = 4 * 1024 * 1024;

pub(super) fn register(builder: &mut LocalCatalogBuilder) -> Result<(), RegistryError> {
    builder.register::<WriteArgs, WriteOutput, _, _>(
        "write",
        "Atomically create or replace a UTF-8 workspace file. Set create_parents to create missing parent directories recursively.",
        ToolOptions::new(vec![Capability::Write])
            .placement(crate::tool::ToolPlacement::InheritWorkspace)
            .argument_paths(|arguments| {
                let args: WriteArgs = serde_json::from_value(arguments.clone())
                    .map_err(crate::tool::invocation::AdmissionError::invalid)?;
                Ok(vec![crate::tool::PathArgument::top_level(
                    "path", None, PathAccess::Write,
                    if args.create_parents { PathKind::WritableWithParents } else { PathKind::Writable },
                )])
            }),
        |context, args| async move {
            check_write_size(&args.content)?;
            let path = std::path::PathBuf::from(&args.path);
            if args.create_parents {
                let parent = path
                    .parent()
                    .ok_or_else(|| LocalError::Failed("path has no parent".to_owned()))?;
                fs::create_dir_all(parent).await?;
            }
            atomic_write(&path, args.content.as_bytes()).await?;
            Ok(WriteOutput {
                path: relative_path(&context.execution_location().workspace, &path),
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
            // Reject oversized input before reading or building a replacement.
            if args.old.len().saturating_add(args.new.len()) > MAX_WRITE_BYTES {
                return Err(LocalError::InvalidArguments(format!(
                    "replace input exceeds the {MAX_WRITE_BYTES}-byte limit"
                )));
            }
            if args.old.is_empty() {
                return Err(LocalError::InvalidArguments(
                    "old cannot be empty".to_owned(),
                ));
            }
            let path = std::path::PathBuf::from(&args.path);
            let text = fs::read_to_string(&path).await?;
            let replacements = text.matches(&args.old).count();
            if replacements != args.count {
                return Err(LocalError::Failed(format!(
                    "expected {} matches, found {replacements}",
                    args.count
                )));
            }
            let output = text.replace(&args.old, &args.new);
            check_write_size(&output)?;
            atomic_write(&path, output.as_bytes()).await?;
            Ok(EditOutput {
                path: relative_path(&context.execution_location().workspace, &path),
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
            let path = std::path::PathBuf::from(&args.path);
            let text = fs::read_to_string(&path).await?;
            let patch = Patch::from_str(&args.patch).map_err(LocalError::failed)?;
            let replacements = patch.hunks().len();
            let output = apply(&text, &patch).map_err(LocalError::failed)?;
            check_write_size(&output)?;
            atomic_write(&path, output.as_bytes()).await?;
            Ok(EditOutput {
                path: relative_path(&context.execution_location().workspace, &path),
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
            let path = std::path::PathBuf::from(&args.path);
            let metadata = fs::symlink_metadata(&path).await?;
            let kind = if metadata.file_type().is_symlink() {
                fs::remove_file(&path).await?;
                RemoveKind::Symlink
            } else if metadata.is_file() {
                fs::remove_file(&path).await?;
                RemoveKind::File
            } else if metadata.is_dir() {
                if args.recursive {
                    fs::remove_dir_all(&path).await?;
                } else {
                    fs::remove_dir(&path).await?;
                }
                RemoveKind::Directory
            } else {
                return Err(LocalError::Failed(
                    "unsupported filesystem entry".to_owned(),
                ));
            };
            Ok(RemoveOutput {
                path: relative_path(&context.execution_location().workspace, &path),
                kind,
            })
        },
    )?;
    Ok(())
}

fn check_write_size(text: &str) -> Result<(), LocalError> {
    if text.len() > MAX_WRITE_BYTES {
        Err(LocalError::Failed(format!(
            "write exceeds the {MAX_WRITE_BYTES}-byte limit"
        )))
    } else {
        Ok(())
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    /// File path.
    path: String,
    /// Complete replacement file contents.
    content: String,
    /// Create missing parent directories recursively before writing.
    #[serde(default)]
    create_parents: bool,
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
    kind: RemoveKind,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum RemoveKind {
    File,
    Directory,
    Symlink,
}

const fn default_one() -> usize {
    1
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use std::sync::Arc;

    use super::*;
    use crate::tool::ToolRegistryBuilder;
    use crate::{
        tests::{RecordingPolicy, TestRuntime},
        tool::{
            executor::{ExecutionError, ExecutionResult, ToolExecutor},
            policy::{PolicyDecision, ResourceId},
        },
    };

    fn builder() -> ToolRegistryBuilder {
        let mut builder = ToolRegistryBuilder::default();
        builder.register_local(register).unwrap();
        builder
    }

    async fn write(
        executor: &ToolExecutor,
        runtime: &TestRuntime,
        path: impl serde::Serialize,
        content: &str,
        create_parents: bool,
    ) -> Result<ExecutionResult, ExecutionError> {
        let arguments = json!({"path": path, "content": content, "create_parents": create_parents});
        executor.run_host(&runtime.agent, "write", arguments).await
    }

    #[tokio::test]
    async fn write_creates_nested_parents_only_when_requested() {
        let runtime = TestRuntime::new().await;
        let executor = runtime.executor(builder());
        for create_parents in [None, Some(false)] {
            let mut args = json!({"path": "missing/nested/file.txt", "content": "text"});
            if let Some(value) = create_parents {
                args["create_parents"] = json!(value);
            }
            let error = executor
                .run_host(&runtime.agent, "write", args)
                .await
                .unwrap_err();
            assert!(
                matches!(error, ExecutionError::Tool(crate::tool::ToolError::Io(ref error)) if error.kind() == std::io::ErrorKind::NotFound),
                "{error:?}"
            );
            assert!(!runtime.root.path().join("missing").exists());
        }
        for relative in ["relative/nested/file.txt", "absolute/nested/file.txt"] {
            let path = runtime.root.path().join(relative);
            let input = if relative.starts_with("absolute") {
                path.to_string_lossy().into_owned()
            } else {
                relative.to_owned()
            };
            let result = write(&executor, &runtime, &input, "hello", true)
                .await
                .unwrap();
            assert_eq!(result.output.value["path"], relative);
            assert_eq!(result.output.value["bytes"], 5);
            assert_eq!(fs::read_to_string(&path).await.unwrap(), "hello");
            // Both modes keep working with existing parents and replace files.
            for create_parents in [false, true] {
                write(&executor, &runtime, &input, "replacement", create_parents)
                    .await
                    .unwrap();
                assert_eq!(fs::read_to_string(&path).await.unwrap(), "replacement");
                let sibling = path.with_file_name(format!("new-{create_parents}.txt"));
                write(&executor, &runtime, sibling, "new", create_parents)
                    .await
                    .unwrap();
            }
            let entries = std::fs::read_dir(path.parent().unwrap()).unwrap();
            let temporary =
                |name: std::ffi::OsString| name.to_string_lossy().starts_with(".skyhook-");
            assert!(
                !entries
                    .into_iter()
                    .any(|entry| temporary(entry.unwrap().file_name()))
            );
        }
    }

    #[tokio::test]
    async fn write_create_parents_normalizes_traversal_and_rejects_file_ancestors_and_oversize() {
        let runtime = TestRuntime::new().await;
        let executor = runtime.executor(builder());
        let root = runtime.root.path();
        for (input, expected) in [
            ("missing/../file.txt", "file.txt"),
            ("missing/../new/nested/file.txt", "new/nested/file.txt"),
        ] {
            let result = write(&executor, &runtime, input, "text", true)
                .await
                .unwrap();
            assert_eq!(result.output.value["path"], expected);
            assert_eq!(
                fs::read_to_string(root.join(expected)).await.unwrap(),
                "text"
            );
            assert!(!root.join("missing").exists());
        }
        fs::write(root.join("file"), "unchanged").await.unwrap();
        for path in [
            "file/child.txt",
            "file/nested/child.txt",
            "file/../child.txt",
        ] {
            assert!(
                write(&executor, &runtime, path, "text", true)
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            fs::read_to_string(root.join("file")).await.unwrap(),
            "unchanged"
        );
        let oversized = "x".repeat(MAX_WRITE_BYTES + 1);
        let path = "oversized/nested/file.txt";
        assert!(
            write(&executor, &runtime, path, &oversized, true)
                .await
                .is_err()
        );
        assert!(!root.join("oversized").exists());
    }

    /// Denies every path resource, plus every write when `deny_write` is set.
    fn write_policy(deny_write: bool) -> Arc<RecordingPolicy> {
        RecordingPolicy::deciding(move |request| {
            let deny = request.permissions.iter().any(|permission| {
                (deny_write && permission.capability == Capability::Write)
                    || matches!(permission.resource, ResourceId::Path { .. })
            });
            if deny {
                PolicyDecision::Deny {
                    reason: "write fixture denied".to_owned(),
                }
            } else {
                PolicyDecision::allow()
            }
        })
    }

    #[tokio::test]
    async fn write_create_parents_does_not_mutate_denied_paths() {
        let runtime = TestRuntime::new().await;
        let workspace = runtime.root.path().join("workspace");
        fs::create_dir(&workspace).await.unwrap();
        let outside = runtime.root.path().join("outside");
        for deny_write in [false, true] {
            let policy = write_policy(deny_write);
            let executor = ToolExecutor::new(
                builder().build(),
                policy.clone(),
                runtime.jobs.clone(),
                workspace.clone(),
            );
            let mut paths = vec![
                outside
                    .join("nested/file.txt")
                    .to_string_lossy()
                    .into_owned(),
                "../outside/nested/file.txt".to_owned(),
                "missing/../../outside/nested/file.txt".to_owned(),
            ];
            if deny_write {
                paths.push("local/nested/file.txt".to_owned());
            }
            for path in paths {
                let error = write(&executor, &runtime, path, "text", true)
                    .await
                    .unwrap_err();
                assert!(matches!(error, ExecutionError::Denied(_)), "{error:?}");
            }
            assert!(!policy.requests.lock().unwrap().is_empty());
            assert!(!outside.exists());
            assert!(!workspace.join("missing").exists());
            assert!(!workspace.join("local").exists());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_create_parents_resolves_symlinks_before_authorization() {
        use std::os::unix::fs::symlink;

        let runtime = TestRuntime::new().await;
        let workspace = runtime.root.path().join("workspace");
        let outside = runtime.root.path().join("outside");
        fs::create_dir(&workspace).await.unwrap();
        fs::create_dir(&outside).await.unwrap();
        symlink(&outside, workspace.join("link")).unwrap();
        symlink(outside.join("missing"), workspace.join("dangling")).unwrap();
        let policy = write_policy(false);
        let executor = ToolExecutor::new(
            builder().build(),
            policy.clone(),
            runtime.jobs.clone(),
            workspace,
        );
        let error = write(&executor, &runtime, "link/nested/file.txt", "text", true)
            .await
            .unwrap_err();
        assert!(matches!(error, ExecutionError::Denied(_)), "{error:?}");
        let resource = ResourceId::path("root", &outside.join("nested/file.txt"));
        let requests = policy.requests.lock().unwrap().clone();
        assert!(
            requests
                .iter()
                .flat_map(|r| &r.permissions)
                .any(|p| p.resource == resource)
        );
        assert!(
            write(
                &executor,
                &runtime,
                "dangling/nested/file.txt",
                "text",
                true
            )
            .await
            .is_err()
        );
        assert!(!outside.join("nested").exists());
        assert!(!outside.join("missing").exists());
    }

    #[tokio::test]
    async fn remove_requires_recursion_and_refuses_the_workspace_root() {
        let runtime = TestRuntime::new().await;
        let executor = runtime.executor(builder());
        let remove = async |arguments| executor.run_host(&runtime.agent, "remove", arguments).await;
        let workspace = runtime.root.path();
        fs::create_dir(workspace.join("directory")).await.unwrap();
        fs::write(workspace.join("directory/file"), "data")
            .await
            .unwrap();
        let error = remove(json!({"path":"directory"})).await.unwrap_err();
        assert!(error.to_string().contains("not empty"));
        remove(json!({"path":"directory", "recursive":true}))
            .await
            .unwrap();
        assert!(!fs::try_exists(workspace.join("directory")).await.unwrap());
        #[cfg(unix)]
        {
            std::fs::write(workspace.join("target"), "safe").unwrap();
            std::os::unix::fs::symlink("target", workspace.join("link")).unwrap();
            let result = remove(json!({"path":"link"})).await.unwrap();
            assert_eq!(result.output.value["kind"], "symlink");
            assert!(fs::try_exists(workspace.join("target")).await.unwrap());
            assert!(!fs::try_exists(workspace.join("link")).await.unwrap());
        }
        let error = remove(json!({"path":"."})).await.unwrap_err();
        assert!(error.to_string().contains("workspace root"));
    }
}
