//! Atomic writes, exact replacements, unified patches, and removals.

use diffy::{Patch, apply};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::fs;

use super::super::workspace::{
    atomic_write, relative_path, resolve_existing, resolve_removable, resolve_writable,
    resolve_writable_with_parents,
};
use crate::tool::{
    PathKind, RegistryError, ToolError, ToolOptions, ToolRegistryBuilder,
    policy::{Capability, PathAccess},
};

const MAX_WRITE_BYTES: usize = 4 * 1024 * 1024;

pub(super) fn register(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    builder.register::<WriteArgs, WriteOutput, _, _>(
        "write",
        "Atomically create or replace a UTF-8 workspace file. Set create_parents to create missing parent directories recursively.",
        ToolOptions::new(vec![Capability::Write])
            .placement(crate::tool::ToolPlacement::InheritWorkspace)
            .argument_paths(|arguments| {
                let args: WriteArgs = serde_json::from_value(arguments.clone())
                    .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
                Ok(vec![crate::tool::PathArgument {
                    name: "path".to_owned(),
                    access: PathAccess::Write,
                    kind: if args.create_parents {
                        PathKind::WritableWithParents
                    } else {
                        PathKind::Writable
                    },
                    default: None,
                    pointer: None,
                }])
            }),
        |context, args| async move {
            check_write_size(&args.content)?;
            let path = if args.create_parents {
                resolve_writable_with_parents(
                    &context.execution_location.workspace,
                    &args.path,
                )
                .await?
            } else {
                resolve_writable(&context.execution_location.workspace, &args.path).await?
            };
            if args.create_parents {
                let parent = path
                    .parent()
                    .ok_or_else(|| ToolError::Failed("path has no parent".to_owned()))?;
                fs::create_dir_all(parent).await?;
            }
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

fn check_write_size(text: &str) -> Result<(), ToolError> {
    if text.len() > MAX_WRITE_BYTES {
        Err(ToolError::Failed(format!(
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
    kind: String,
}

const fn default_one() -> usize {
    1
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::tool::{
        executor::{ExecutionError, ToolExecutor},
        policy::{AuthorizationRequest, Policy, PolicyDecision, PolicyFuture},
    };
    use tokio::sync::Mutex;

    fn mutation_executor(runtime: &crate::tests::TestRuntime) -> ToolExecutor {
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder).unwrap();
        runtime.executor(builder)
    }

    #[tokio::test]
    async fn write_creates_nested_parents_only_when_requested() {
        use serde_json::json;

        let runtime = crate::tests::TestRuntime::new().await;
        let executor = mutation_executor(&runtime);
        for create_parents in [None, Some(false)] {
            let mut args = json!({"path": "missing/nested/file.txt", "content": "text"});
            if let Some(value) = create_parents {
                args["create_parents"] = json!(value);
            }
            let error = executor
                .execute(runtime.agent.clone(), "write", args, None)
                .await
                .unwrap_err();
            assert!(
                matches!(error, ExecutionError::Tool(ToolError::Io(ref error)) if error.kind() == std::io::ErrorKind::NotFound),
                "{error:?}"
            );
            assert!(!runtime.root.path().join("missing").exists());
        }
        for absolute in [false, true] {
            let relative = if absolute {
                "absolute/nested/file.txt"
            } else {
                "relative/nested/file.txt"
            };
            let path = runtime.root.path().join(relative);
            let input = if absolute {
                path.to_string_lossy().into_owned()
            } else {
                relative.to_owned()
            };
            let result = executor
                .execute(
                    runtime.agent.clone(),
                    "write",
                    json!({
                        "path": input, "content": "hello", "create_parents": true
                    }),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(result.output.value["path"], relative);
            assert_eq!(result.output.value["bytes"], 5);
            assert_eq!(fs::read_to_string(&path).await.unwrap(), "hello");
            // Both modes keep working with existing parents and replace files.
            for create_parents in [false, true] {
                executor.execute(runtime.agent.clone(), "write", json!({
                    "path": input, "content": "replacement", "create_parents": create_parents
                }), None).await.unwrap();
                assert_eq!(fs::read_to_string(&path).await.unwrap(), "replacement");
                executor
                    .execute(
                        runtime.agent.clone(),
                        "write",
                        json!({
                            "path": path.with_file_name(format!("new-{create_parents}.txt")),
                            "content": "new", "create_parents": create_parents
                        }),
                        None,
                    )
                    .await
                    .unwrap();
            }
            let entries = std::fs::read_dir(path.parent().unwrap()).unwrap();
            assert!(entries.into_iter().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".skyhook-")
            }));
        }
    }

    #[tokio::test]
    async fn write_create_parents_normalizes_missing_parent_traversal() {
        use serde_json::json;

        let runtime = crate::tests::TestRuntime::new().await;
        let executor = mutation_executor(&runtime);
        for (input, expected) in [
            ("missing/../file.txt", "file.txt"),
            ("missing/../new/nested/file.txt", "new/nested/file.txt"),
        ] {
            let result = executor
                .execute(
                    runtime.agent.clone(),
                    "write",
                    json!({
                        "path": input, "content": "text", "create_parents": true
                    }),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(result.output.value["path"], expected);
            assert_eq!(
                fs::read_to_string(runtime.root.path().join(expected))
                    .await
                    .unwrap(),
                "text"
            );
            assert!(!runtime.root.path().join("missing").exists());
        }
    }

    #[tokio::test]
    async fn write_create_parents_rejects_file_ancestors_and_oversized_content() {
        use serde_json::json;

        let runtime = crate::tests::TestRuntime::new().await;
        let executor = mutation_executor(&runtime);
        fs::write(runtime.root.path().join("file"), "unchanged")
            .await
            .unwrap();
        for path in [
            "file/child.txt",
            "file/nested/child.txt",
            "file/../child.txt",
        ] {
            assert!(
                executor
                    .execute(
                        runtime.agent.clone(),
                        "write",
                        json!({
                            "path": path, "content": "text", "create_parents": true
                        }),
                        None
                    )
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            fs::read_to_string(runtime.root.path().join("file"))
                .await
                .unwrap(),
            "unchanged"
        );
        assert!(executor.execute(runtime.agent.clone(), "write", json!({
            "path": "oversized/nested/file.txt", "content": "x".repeat(MAX_WRITE_BYTES + 1), "create_parents": true
        }), None).await.is_err());
        assert!(!runtime.root.path().join("oversized").exists());
    }

    #[derive(Default)]
    struct WritePolicy {
        deny_write: bool,
        requests: Mutex<Vec<AuthorizationRequest>>,
    }

    impl Policy for WritePolicy {
        fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_> {
            Box::pin(async move {
                let deny = request.permissions.iter().any(|permission| {
                    (self.deny_write && permission.capability == Capability::Write)
                        || permission.resource.namespace == "path"
                });
                self.requests.lock().await.push(request);
                if deny {
                    PolicyDecision::Deny {
                        reason: "write fixture denied".to_owned(),
                    }
                } else {
                    PolicyDecision::allow()
                }
            })
        }
    }

    #[tokio::test]
    async fn write_create_parents_does_not_mutate_denied_paths() {
        use serde_json::json;

        let runtime = crate::tests::TestRuntime::new().await;
        let workspace = runtime.root.path().join("workspace");
        fs::create_dir(&workspace).await.unwrap();
        let outside = runtime.root.path().join("outside");
        for deny_write in [false, true] {
            let mut builder = ToolRegistryBuilder::default();
            register(&mut builder).unwrap();
            let policy = Arc::new(WritePolicy {
                deny_write,
                ..Default::default()
            });
            let executor = ToolExecutor::new(
                builder.build(),
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
                let error = executor
                    .execute(
                        runtime.agent.clone(),
                        "write",
                        json!({
                            "path": path, "content": "text", "create_parents": true
                        }),
                        None,
                    )
                    .await
                    .unwrap_err();
                assert!(matches!(error, ExecutionError::Denied(_)), "{error:?}");
            }
            assert!(!policy.requests.lock().await.is_empty());
            assert!(!outside.exists());
            assert!(!workspace.join("missing").exists());
            assert!(!workspace.join("local").exists());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_create_parents_resolves_symlinks_before_authorization() {
        use serde_json::json;
        use std::os::unix::fs::symlink;

        let runtime = crate::tests::TestRuntime::new().await;
        let workspace = runtime.root.path().join("workspace");
        let outside = runtime.root.path().join("outside");
        fs::create_dir(&workspace).await.unwrap();
        fs::create_dir(&outside).await.unwrap();
        symlink(&outside, workspace.join("link")).unwrap();
        symlink(outside.join("missing"), workspace.join("dangling")).unwrap();
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder).unwrap();
        let policy = Arc::new(WritePolicy::default());
        let executor = ToolExecutor::new(
            builder.build(),
            policy.clone(),
            runtime.jobs.clone(),
            workspace,
        );
        let error = executor
            .execute(
                runtime.agent.clone(),
                "write",
                json!({
                    "path": "link/nested/file.txt", "content": "text", "create_parents": true
                }),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ExecutionError::Denied(_)), "{error:?}");
        assert!(
            policy
                .requests
                .lock()
                .await
                .iter()
                .flat_map(|request| &request.permissions)
                .any(|permission| permission.resource
                    == crate::tool::policy::ResourceId::path(
                        "root",
                        &outside.join("nested/file.txt")
                    ))
        );
        assert!(executor.execute(runtime.agent.clone(), "write", json!({
            "path": "dangling/nested/file.txt", "content": "text", "create_parents": true
        }), None).await.is_err());
        assert!(!outside.join("nested").exists());
        assert!(!outside.join("missing").exists());
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
    async fn remove_requires_recursion_and_refuses_the_workspace_root() {
        let runtime = crate::tests::TestRuntime::new().await;
        let executor = mutation_executor(&runtime);
        let workspace = runtime.root.path();
        fs::create_dir(workspace.join("directory")).await.unwrap();
        fs::write(workspace.join("directory/file"), "data")
            .await
            .unwrap();
        let error = executor
            .execute(
                runtime.agent.clone(),
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
                runtime.agent.clone(),
                "remove",
                serde_json::json!({"path":"directory", "recursive":true}),
                None,
            )
            .await
            .unwrap();
        assert!(!fs::try_exists(workspace.join("directory")).await.unwrap());
        #[cfg(unix)]
        {
            std::fs::write(workspace.join("target"), "safe").unwrap();
            std::os::unix::fs::symlink("target", workspace.join("link")).unwrap();
            executor
                .execute(
                    runtime.agent.clone(),
                    "remove",
                    serde_json::json!({"path":"link"}),
                    None,
                )
                .await
                .unwrap();
            assert!(fs::try_exists(workspace.join("target")).await.unwrap());
            assert!(!fs::try_exists(workspace.join("link")).await.unwrap());
        }
        let error = executor
            .execute(
                runtime.agent,
                "remove",
                serde_json::json!({"path":"."}),
                None,
            )
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("workspace root"));
    }
}
