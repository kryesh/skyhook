//! Atomic writes, exact replacements, and removals.
use crate::tool::ToolOptions;
use crate::tool::diagnostic::{Cause, Effects, IoKind, Operation, PartialContext, Subject};
use crate::tool::invocation::{AdmissionError, LocalCatalogBuilder, LocalContext, LocalError};
use crate::tool::registry::{Invocation, SOURCE_ARGUMENT, serialized};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;

use super::super::workspace::atomic_write;
use crate::fs::{Contents, FileKind};
use crate::tool::{
    PathArgument, PathKind, RegistryError,
    policy::{Capability, PathAccess},
};

const MAX_WRITE_BYTES: usize = 4 * 1024 * 1024;

pub(super) fn register(builder: &mut LocalCatalogBuilder) -> Result<(), RegistryError> {
    builder.register_checked(
        crate::tool::builtins::names::WRITE,
        "Atomically create or replace a workspace file with UTF-8 `content`, or copy the bytes of a `source` file. Set create_parents to create missing parent directories recursively.",
        ToolOptions::new(vec![Capability::Write])
            .result::<WriteOutput>()
            .placement(crate::tool::ToolPlacement::InheritWorkspace)
            .reads_source()
            .argument_paths(|write: &mut Write| {
                let kind = if write.create_parents {
                    PathKind::WritableWithParents
                } else {
                    PathKind::Writable
                };
                vec![PathArgument::new(&mut write.path, PathAccess::Write, kind)]
            }),
        WriteArgs::admit,
        |write| Invocation::new(|context| async move { serialized(write.run(context).await?) }),
    )?;
    builder.register_checked(
        crate::tool::builtins::names::REPLACE,
        "Replace exact text in a UTF-8 workspace file with an expected match count.",
        ToolOptions::new(vec![Capability::Write])
            .placement(crate::tool::ToolPlacement::InheritWorkspace)
            .argument_paths(|args: &mut ReplaceArgs| {
                vec![PathArgument::new(
                    &mut args.path,
                    PathAccess::Write,
                    PathKind::Existing,
                )]
            }),
        ReplaceArgs::admit,
        |args| Invocation::unit(|_| args.run()),
    )?;
    builder.register::<RemoveArgs, RemoveOutput, _, _>(
        "remove",
        "Remove a workspace file, symlink, or directory.",
        ToolOptions::new(vec![Capability::Write])
            .placement(crate::tool::ToolPlacement::InheritWorkspace)
            .argument_paths(|args: &mut RemoveArgs| {
                vec![PathArgument::new(&mut args.path, PathAccess::Write, PathKind::Removable)]
            }),
        |_, args| async move {
            let path = PathBuf::from(&args.path);
            let metadata = fs::symlink_metadata(&path)
                .await
                .map_err(AdmissionError::annotated(unchanged(
                    Operation::Inspect,
                    &path,
                )))?;
            let kind = match FileKind::from(metadata.file_type()) {
                FileKind::Symlink => RemoveKind::Symlink,
                FileKind::File => RemoveKind::File,
                FileKind::Directory => RemoveKind::Directory,
                FileKind::Other => {
                    return Err(LocalError::failed("unsupported filesystem entry")
                        .context(unchanged(Operation::Remove, &path)));
                }
            };
            let failed =
                |error| LocalError::io(error).operation(Operation::Remove, Subject::path(&path));
            match kind {
                RemoveKind::Directory if args.recursive => fs::remove_dir_all(&path)
                    .await
                    .map_err(|error| failed(error).effects(Effects::PartialChange))?,
                RemoveKind::Directory => fs::remove_dir(&path).await.map_err(|error| {
                    match error.kind() {
                        std::io::ErrorKind::DirectoryNotEmpty => LocalError::cause(Cause::Io {
                            kind: IoKind::DirectoryNotEmpty,
                            code: error.raw_os_error(),
                            detail: Some(
                                "use recursive removal only if all directory contents should be deleted"
                                    .to_owned(),
                            ),
                        })
                        .operation(Operation::Remove, Subject::path(&path)),
                        _ => failed(error),
                    }
                })?,
                RemoveKind::Symlink | RemoveKind::File => {
                    fs::remove_file(&path).await.map_err(failed)?
                }
            }
            Ok(RemoveOutput { kind })
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
    content: Option<String>,
    /// File whose bytes to copy instead of `content`.
    source: Option<crate::target::TargetPath>,
    /// Create missing parent directories recursively before writing.
    #[serde(default)]
    create_parents: bool,
}

/// A write admitted with exactly one of content or a source.
struct Write {
    path: String,
    data: WriteData,
    create_parents: bool,
}

enum WriteData {
    Content(String),
    /// The executor loads the source's bytes into the call's context.
    Source,
}

impl WriteArgs {
    fn admit(self) -> Result<Write, AdmissionError> {
        let data = match (self.content, self.source) {
            (Some(content), None) => {
                check_write_size(content.len(), &self.path)?;
                WriteData::Content(content)
            }
            (None, Some(_)) => WriteData::Source,
            _ => {
                return Err(AdmissionError::invalid_arguments(
                    "supply exactly one of content or source",
                )
                .context(unchanged(Operation::Validate, &self.path)));
            }
        };
        Ok(Write {
            path: self.path,
            data,
            create_parents: self.create_parents,
        })
    }
}

impl Write {
    async fn run(self, context: LocalContext) -> Result<WriteOutput, LocalError> {
        let path = PathBuf::from(&self.path);
        let contents = match self.data {
            WriteData::Content(content) => Contents::Bytes(content.into_bytes()),
            WriteData::Source => Contents::File(context.source()?.reader().map_err(|error| {
                LocalError::io(error)
                    .operation(Operation::Read, Subject::argument([SOURCE_ARGUMENT]))
            })?),
        };
        if self.create_parents {
            let parent = path.parent().ok_or_else(|| {
                AdmissionError::failed("path has no parent")
                    .context(unchanged(Operation::CreateDirectories, &path))
            })?;
            fs::create_dir_all(parent)
                .await
                .map_err(LocalError::annotated(
                    PartialContext::new(
                        Operation::CreateDirectories,
                        Subject::ParentDirectory(parent.to_owned()),
                    )
                    .effects(Effects::PartialChange),
                ))?;
        }
        let bytes = atomic_write(&path, contents).await.map_err(|error| {
            if self.create_parents
                && error.diagnostic().context.effects != Effects::DestinationReplaced
            {
                error.effects(Effects::PartialChange)
            } else {
                error
            }
        })?;
        Ok(WriteOutput { bytes })
    }
}

#[derive(Serialize, JsonSchema)]
struct WriteOutput {
    bytes: u64,
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

impl ReplaceArgs {
    /// Reject invalid input before authorization, source reads, or allocation.
    fn admit(self) -> Result<Self, AdmissionError> {
        let input = self.old.len().saturating_add(self.new.len());
        let invalid = if input > MAX_WRITE_BYTES {
            format!("replace input is {input} bytes, exceeding the {MAX_WRITE_BYTES}-byte limit")
        } else if self.old.is_empty() {
            "old cannot be empty".to_owned()
        } else {
            return Ok(self);
        };
        Err(AdmissionError::invalid_arguments(invalid)
            .context(unchanged(Operation::Validate, &self.path)))
    }

    async fn run(self) -> Result<(), LocalError> {
        let path = PathBuf::from(&self.path);
        let bytes = crate::fs::read_regular(&path, MAX_WRITE_BYTES as u64)
            .await
            .map_err(AdmissionError::annotated(unchanged(Operation::Read, &path)))?;
        let text = String::from_utf8(bytes).map_err(|error| {
            AdmissionError::failed(error.utf8_error())
                .context(unchanged(Operation::Deserialize, &path))
        })?;
        let found = text.matches(&self.old).count();
        if found != self.count {
            return Err(LocalError::failed(format!(
                "expected {} matches, found {found}{}",
                self.count,
                match_lines(&text, &self.old)
            ))
            .context(unchanged(Operation::Validate, &path)));
        }
        // Bound the result before allocating it: a short search string and
        // a large replacement can otherwise expand a small input enormously.
        check_write_size(
            replacement_size(text.len(), self.old.len(), self.new.len(), found),
            &path,
        )?;
        let output = text.replace(&self.old, &self.new);
        atomic_write(&path, Contents::Bytes(output.into_bytes())).await?;
        Ok(())
    }
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
            ToolError,
            diagnostic::{Cause, IoKind},
            executor::{ExecutionResult, ToolExecutor},
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
    ) -> Result<ExecutionResult, ToolError> {
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
    async fn replace_rejects_oversized_input_and_expansion_without_mutating() {
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
        assert_eq!(fs::read_to_string(&path).await.unwrap(), "xx");
        // The input is bounded even when the replacement would shrink it.
        let oversized = "x".repeat(MAX_WRITE_BYTES + 1);
        fs::write(&path, &oversized).await.unwrap();
        let error = executor
            .run_host(
                &runtime.agent,
                "replace",
                json!({"path":"expand.txt", "old":"x", "new":"", "count":MAX_WRITE_BYTES + 1}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.diagnostic().context.effects, Effects::Unchanged);
        assert_eq!(fs::read_to_string(path).await.unwrap(), oversized);
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

    /// A missing parent fails unless `create_parents` asks for the resolved path's
    /// missing directories, never below a file nor for an oversized write.
    #[tokio::test]
    async fn write_creates_missing_parents_only_when_requested() {
        let runtime = TestRuntime::new().await;
        let executor = runtime.executor(builder());
        let root = runtime.root.path();
        let missing = json!({"path": "missing/nested/file.txt", "content": "text"});
        let error = executor
            .run_host(&runtime.agent, "write", missing)
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
        fs::write(root.join("file"), "unchanged").await.unwrap();
        let absolute = root.join("absolute/nested/file.txt");
        let oversized = "x".repeat(MAX_WRITE_BYTES + 1);
        for (path, create_parents, content, written) in [
            ("missing/nested/file.txt", false, "text", None),
            (
                "relative/nested/file.txt",
                true,
                "text",
                Some("relative/nested/file.txt"),
            ),
            (
                absolute.to_str().unwrap(),
                true,
                "text",
                Some("absolute/nested/file.txt"),
            ),
            ("missing/../new/nested/file.txt", true, "text", None),
            ("file/child.txt", true, "text", None),
            ("file/nested/child.txt", true, "text", None),
            ("file/../child.txt", true, "text", None),
            ("oversized/nested/file.txt", true, &oversized, None),
        ] {
            let result = write(&executor, &runtime, path, content, create_parents).await;
            assert_eq!(result.is_ok(), written.is_some(), "{path}");
            if let Some(written) = written {
                let text = fs::read_to_string(root.join(written)).await.unwrap();
                assert_eq!(text, content);
            }
        }
        for untouched in ["missing", "new", "oversized"] {
            assert!(!root.join(untouched).exists(), "{untouched}");
        }
        let file = fs::read_to_string(root.join("file")).await.unwrap();
        assert_eq!(file, "unchanged");
        // Existing parents work in both modes, replacing without leftovers.
        let path = "relative/nested/file.txt";
        for create_parents in [false, true] {
            let content = format!("replacement {create_parents}");
            write(&executor, &runtime, path, &content, create_parents)
                .await
                .unwrap();
            assert_eq!(fs::read_to_string(root.join(path)).await.unwrap(), content);
        }
        let entries = std::fs::read_dir(root.join("relative/nested")).unwrap();
        let temporary = |name: std::ffi::OsString| name.to_string_lossy().starts_with(".skyhook-");
        assert!(
            !entries
                .into_iter()
                .any(|entry| temporary(entry.unwrap().file_name()))
        );
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

    /// A denied write creates nothing: the policy sees the resolved destination,
    /// through symlinks, before any directory exists, and a path that climbs
    /// out of a missing entry fails without asking.
    #[tokio::test]
    async fn write_create_parents_creates_nothing_before_authorization() {
        use std::os::unix::fs::symlink;

        let runtime = TestRuntime::new().await;
        let workspace = runtime.root.path().join("workspace");
        let outside = runtime.root.path().join("outside");
        fs::create_dir(&workspace).await.unwrap();
        fs::create_dir(&outside).await.unwrap();
        symlink(&outside, workspace.join("link")).unwrap();
        symlink(outside.join("missing"), workspace.join("dangling")).unwrap();
        let resolved = ResourceId::path(
            &crate::target::TargetRef::Root,
            &crate::tool::policy::PathText::new(outside.join("nested/file.txt")).unwrap(),
        );
        for deny_write in [false, true] {
            let policy = write_policy(deny_write);
            let executor = ToolExecutor::new(
                builder().build(),
                policy.clone(),
                runtime.jobs.clone(),
                workspace.clone(),
            );
            let mut paths = vec![
                "link/nested/file.txt".to_owned(),
                outside
                    .join("nested/file.txt")
                    .to_string_lossy()
                    .into_owned(),
                "../outside/nested/file.txt".to_owned(),
            ];
            if deny_write {
                paths.push("local/nested/file.txt".to_owned());
            }
            let dangling = write(
                &executor,
                &runtime,
                "dangling/nested/file.txt",
                "text",
                true,
            );
            assert!(dangling.await.is_err());
            // Nothing resolves under a missing entry, so these fail unasked
            // rather than step back out through a link.
            let asked = policy.requests.lock().unwrap().len();
            for path in [
                "missing/../../outside/nested/file.txt",
                "missing/../link/pwned",
                "missing/../../workspace/link/pwned",
            ] {
                let error = write(&executor, &runtime, path, "text", true)
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
                    "{path}: {error:?}"
                );
            }
            assert_eq!(policy.requests.lock().unwrap().len(), asked);
            for path in paths {
                let error = write(&executor, &runtime, &path, "text", true)
                    .await
                    .unwrap_err();
                assert!(
                    matches!(error.diagnostic().cause, Cause::Denied(_)),
                    "{path}: {error:?}"
                );
                if path.starts_with("link/") {
                    let requests = policy.requests.lock().unwrap();
                    let asked = &requests.last().unwrap().permissions;
                    assert!(asked.iter().any(|p| p.resource == resolved), "{asked:?}");
                }
            }
        }
        for created in [
            outside.join("nested"),
            outside.join("missing"),
            outside.join("pwned"),
            workspace.join("missing"),
            workspace.join("local"),
        ] {
            assert!(!created.exists(), "{}", created.display());
        }
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
        let Cause::Io {
            kind: IoKind::DirectoryNotEmpty,
            detail: Some(detail),
            ..
        } = error.diagnostic().cause
        else {
            panic!("{error:?}")
        };
        assert!(detail.contains("recursive"), "{detail}");
        remove(json!({"path":"directory", "recursive":true}))
            .await
            .unwrap();
        assert!(!fs::try_exists(workspace.join("directory")).await.unwrap());
        std::fs::write(workspace.join("target"), "safe").unwrap();
        std::os::unix::fs::symlink("target", workspace.join("link")).unwrap();
        let result = remove(json!({"path":"link"})).await.unwrap();
        assert_eq!(result.output.value["kind"], "symlink");
        assert!(fs::try_exists(workspace.join("target")).await.unwrap());
        assert!(!fs::try_exists(workspace.join("link")).await.unwrap());
        let error = remove(json!({"path":"."})).await.unwrap_err();
        assert!(error.to_string().contains("authorization root"));
    }
}
