//! Atomic writes, exact replacements, and removals.
use crate::tool::ToolOptions;
use crate::tool::diagnostic::{Effects, Operation, PartialContext, Subject, deserialize_arguments};
use crate::tool::invocation::{AdmissionError, LocalCatalogBuilder, LocalError};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
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
            .argument_validator(|arguments| {
                let args: WriteArgs = deserialize_arguments(arguments.clone())?;
                check_write_size(args.content.len(), &args.path)
            })
            .argument_paths(|arguments| {
                let args: WriteArgs = deserialize_arguments(arguments.clone())?;
                Ok(vec![crate::tool::PathArgument::top_level(
                    "path", None, PathAccess::Write,
                    if args.create_parents { PathKind::WritableWithParents } else { PathKind::Writable },
                )])
            }),
        |context, args| async move {
            let path = std::path::PathBuf::from(&args.path);
            if args.create_parents {
                let parent = path
                    .parent()
                    .ok_or_else(|| AdmissionError::failed("path has no parent").context(unchanged(Operation::CreateDirectories, &path)))?;
                fs::create_dir_all(parent).await.map_err(LocalError::annotated(
                    PartialContext::new(Operation::CreateDirectories, Subject::ParentDirectory(parent.to_owned()))
                        .effects(Effects::PartialChange),
                ))?;
            }
            atomic_write(&path, args.content.as_bytes()).await.map_err(|error| {
                if args.create_parents && error.diagnostic().context.effects != Effects::DestinationReplaced {
                    error.effects(Effects::PartialChange)
                } else { error }
            })?;
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
            .path_argument("path", PathAccess::Write, PathKind::Existing)
            .argument_validator(|arguments| {
                let args: ReplaceArgs = deserialize_arguments(arguments.clone())?;
                // Reject invalid input before authorization, source reads, or allocation.
                let input = args.old.len().saturating_add(args.new.len());
                let invalid = if input > MAX_WRITE_BYTES {
                    format!(
                        "replace input is {input} bytes, exceeding the {MAX_WRITE_BYTES}-byte limit"
                    )
                } else if args.old.is_empty() {
                    "old cannot be empty".to_owned()
                } else {
                    return Ok(());
                };
                Err(AdmissionError::invalid_arguments(invalid)
                    .context(unchanged(Operation::Validate, &args.path)))
            }),
        |context, args| async move {
            let path = std::path::PathBuf::from(&args.path);
            let text = fs::read_to_string(&path)
                .await
                .map_err(AdmissionError::annotated(unchanged(Operation::Read, &path)))?;
            let replacements = text.matches(&args.old).count();
            if replacements != args.count {
                return Err(LocalError::failed(format!(
                    "expected {} matches, found {replacements}{}",
                    args.count,
                    match_lines(&text, &args.old)
                ))
                .context(unchanged(Operation::Validate, &path)));
            }
            // Bound the result before allocating it: a short search string and
            // a large replacement can otherwise expand a small input enormously.
            check_write_size(
                replacement_size(text.len(), args.old.len(), args.new.len(), replacements),
                &path,
            )?;
            let output = text.replace(&args.old, &args.new);
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
            let metadata = fs::symlink_metadata(&path)
                .await
                .map_err(AdmissionError::annotated(unchanged(
                    Operation::Inspect,
                    &path,
                )))?;
            let kind = if metadata.file_type().is_symlink() {
                RemoveKind::Symlink
            } else if metadata.is_file() {
                RemoveKind::File
            } else if metadata.is_dir() {
                RemoveKind::Directory
            } else {
                return Err(LocalError::failed("unsupported filesystem entry")
                    .context(unchanged(Operation::Remove, &path)));
            };
            let failed =
                |error| LocalError::io(error).operation(Operation::Remove, Subject::path(&path));
            match kind {
                RemoveKind::Directory if args.recursive => fs::remove_dir_all(&path)
                    .await
                    .map_err(|error| failed(error).effects(Effects::PartialChange))?,
                RemoveKind::Directory => fs::remove_dir(&path).await.map_err(failed)?,
                RemoveKind::Symlink | RemoveKind::File => {
                    fs::remove_file(&path).await.map_err(failed)?
                }
            }
            Ok(RemoveOutput {
                path: relative_path(&context.execution_location().workspace, &path),
                kind,
            })
        },
    )?;
    Ok(())
}

/// Starting lines of the first few matches.
fn match_lines(text: &str, old: &str) -> String {
    const SHOWN: usize = 10;
    let mut lines = Vec::new();
    let (mut line, mut scanned) = (1, 0);
    for (offset, _) in text.match_indices(old).take(SHOWN + 1) {
        line += text[scanned..offset].matches('\n').count();
        scanned = offset;
        lines.push(line.to_string());
    }
    if lines.is_empty() {
        return String::new();
    }
    let more = if lines.len() > SHOWN { ", ..." } else { "" };
    lines.truncate(SHOWN);
    format!(" at lines {}{more}", lines.join(", "))
}

/// `count` non-overlapping matches of `old` lie within `text`.
fn replacement_size(text: usize, old: usize, new: usize, count: usize) -> usize {
    (text - old * count).saturating_add(new.saturating_mul(count))
}

fn check_write_size(bytes: usize, path: impl AsRef<Path>) -> Result<(), AdmissionError> {
    if bytes > MAX_WRITE_BYTES {
        Err(AdmissionError::failed(format!(
            "write is {bytes} bytes, exceeding the {MAX_WRITE_BYTES}-byte limit"
        ))
        .context(unchanged(Operation::Validate, path)))
    } else {
        Ok(())
    }
}

fn unchanged(operation: Operation, path: impl AsRef<Path>) -> PartialContext {
    PartialContext::new(operation, Subject::path(path)).effects(Effects::Unchanged)
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
            diagnostic::{Cause, IoKind},
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

    #[test]
    fn match_lines_reports_starting_lines_and_caps_the_list() {
        assert_eq!(match_lines("a\nb\n", "x"), "");
        assert_eq!(match_lines("x x\na\nb\nx\n", "x"), " at lines 1, 1, 4");
        assert_eq!(match_lines("a\nb\nc\nb\nc\n", "b\nc"), " at lines 2, 4");
        assert_eq!(
            match_lines(&"x\n".repeat(11), "x"),
            " at lines 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, ..."
        );
    }

    #[test]
    fn replacement_size_checks_expansion_shrinkage_and_overflow() {
        assert_eq!(replacement_size(8, 1, 3, 2), 12);
        assert_eq!(replacement_size(8, 3, 1, 2), 4);
        assert_eq!(replacement_size(8, 1, usize::MAX, 2), usize::MAX);
    }

    #[tokio::test]
    async fn replace_rejects_oversized_expansion_without_mutating() {
        let runtime = TestRuntime::new().await;
        let executor = runtime.executor(builder());
        let path = runtime.root.path().join("expand.txt");
        fs::write(&path, "xx").await.unwrap();
        let result = executor.run_host(
            &runtime.agent,
            "replace",
            json!({"path":"expand.txt", "old":"x", "new":"y".repeat(MAX_WRITE_BYTES / 2 + 1), "count":2}),
        ).await;
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(path).await.unwrap(), "xx");
    }

    #[tokio::test]
    async fn replacement_mismatch_reports_counts_and_lines_not_source_text() {
        let runtime = TestRuntime::new().await;
        let executor = runtime.executor(builder());
        let original = "private-search private-search\nother\nprivate-search\n";
        let path = runtime.root.path().join("mismatch.txt");
        fs::write(&path, original).await.unwrap();
        let error = executor.run_host(
            &runtime.agent,
            "replace",
            json!({"path":"mismatch.txt", "old":"private-search", "new":"private-replacement", "count":2}),
        ).await.unwrap_err();
        let diagnostic = error.diagnostic();
        assert_eq!(diagnostic.context.effects, Effects::Unchanged);
        assert_eq!(
            diagnostic.cause,
            Cause::Message("expected 2 matches, found 3 at lines 1, 1, 3".into())
        );
        assert_eq!(fs::read_to_string(path).await.unwrap(), original);
    }

    #[tokio::test]
    async fn mutation_preflight_validates_limits_before_resolving_paths() {
        let runtime = TestRuntime::new().await;
        let executor = runtime.executor(builder());
        let oversized = "z".repeat(MAX_WRITE_BYTES + 1);
        for (tool, arguments, expected) in [
            (
                "write",
                json!({"path":"missing/file", "content":oversized}),
                "limit",
            ),
            (
                "replace",
                json!({"path":"missing/file", "old":"", "new":"y"}),
                "old cannot be empty",
            ),
            (
                "replace",
                json!({"path":"missing/file", "old":"x", "new":oversized}),
                "limit",
            ),
        ] {
            let error = executor
                .run_host(&runtime.agent, tool, arguments)
                .await
                .unwrap_err();
            let diagnostic = error.diagnostic();
            assert_eq!(diagnostic.context.subject, Subject::path("missing/file"));
            assert!(
                matches!(&diagnostic.cause, Cause::InvalidArguments(text) | Cause::Message(text) if text.contains(expected)),
                "{diagnostic:?}"
            );
        }
        assert!(!runtime.root.path().join("missing").exists());
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
                matches!(
                    error.diagnostic().cause,
                    Cause::Io {
                        kind: IoKind::NotFound,
                        ..
                    }
                ),
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
                assert!(
                    matches!(error.diagnostic().cause, Cause::Denied(_)),
                    "{error:?}"
                );
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
        assert!(
            matches!(error.diagnostic().cause, Cause::Denied(_)),
            "{error:?}"
        );
        let resource = ResourceId::path(
            &crate::target::TargetRef::Root,
            &outside.join("nested/file.txt"),
        );
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
        let message = error.to_string();
        assert!(message.contains("not empty"), "{message}");
        assert!(message.contains("recursive"), "{message}");
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
