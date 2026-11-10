//! Atomic writes, exact replacements, and removals.
use crate::diagnostic::PartialContext;
use crate::output::ProducedOutput;
use crate::spec::SOURCE_ARGUMENT;
use crate::spec::TargetTool;
use crate::{
    error::{AdmissionError, CallError},
    spec::{Call, ended},
};
use skyhook_worker::diagnostic::{Effects, Operation, Subject};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::path::{PathAccess, PathArgument};
use skyhook_base::limits::MODEL_CONTENT_BYTES;
use skyhook_worker::{
    op,
    op::{PathKind, RemoveKind},
};

/// Atomically create or replace a file.
pub struct WriteFile;

impl TargetTool for WriteFile {
    const NAME: &'static str = "write";
    const DESCRIPTION: &'static str = "Atomically create or replace a workspace file with UTF-8 `content`, or copy the bytes of a `source` file";
    type Input = WriteArgs;
    type Admitted = Write;
    type Op = op::Write;
    type Rendering = ();

    fn result_schema() -> Option<serde_json::Value> {
        Some(crate::schema::result_schema::<WriteOutput>())
    }

    fn admit(input: WriteArgs) -> Result<Write, AdmissionError> {
        input.admit()
    }

    fn paths(write: &mut Write) -> Vec<PathArgument<'_>> {
        vec![PathArgument::new(
            &mut write.path,
            PathAccess::Write,
            PathKind::Writable,
        )]
    }

    fn op(write: &mut Write, call: &Call) -> Result<op::Write, CallError> {
        let content = match &mut write.data {
            WriteData::Content(content) => op::Content::Inline(std::mem::take(content).into()),
            WriteData::Source(_) => call.source()?.content()?,
        };
        let path = write.path.clone();
        Ok(op::Write { path, content })
    }

    fn rendering(_: Write) {}

    async fn render<U: Into<CallError> + Send + 'static>(
        (): (),
        running: op::Running<op::Write, U>,
        _: &Call,
    ) -> Result<ProducedOutput, CallError> {
        // The target names every failure but its source's, which only this tool names.
        let source = PartialContext::new(Operation::Read, Subject::argument([SOURCE_ARGUMENT]));
        let written = ended(running).await?;
        let op::Written { bytes } =
            written.map_err(|failure| CallError::from(failure).or(source))?;
        Ok(ProducedOutput::serialized(WriteOutput { bytes })?)
    }
}

/// Replace exact text in a file.
pub struct ReplaceText;

impl TargetTool for ReplaceText {
    const NAME: &'static str = "replace";
    const DESCRIPTION: &'static str = "Replace exact text in a UTF-8 workspace file";
    type Input = op::Replace;
    type Admitted = op::Replace;
    type Op = op::Replace;
    type Rendering = ();

    fn result_schema() -> Option<serde_json::Value> {
        None
    }

    /// Reject input over the content limit.
    fn admit(replace: op::Replace) -> Result<op::Replace, AdmissionError> {
        let input = replace.old.len().saturating_add(replace.new.len());
        if input <= MODEL_CONTENT_BYTES {
            return Ok(replace);
        }
        Err(AdmissionError::invalid_arguments(format!(
            "replace input is {input} bytes, exceeding the {MODEL_CONTENT_BYTES}-byte limit"
        ))
        .context(unchanged(Operation::Validate, &replace.path)))
    }

    fn paths(args: &mut op::Replace) -> Vec<PathArgument<'_>> {
        vec![PathArgument::new(
            &mut args.path,
            PathAccess::Write,
            PathKind::Existing,
        )]
    }

    fn op(replace: &mut op::Replace, _: &Call) -> Result<op::Replace, CallError> {
        Ok(op::Replace {
            path: replace.path.clone(),
            old: std::mem::take(&mut replace.old),
            new: std::mem::take(&mut replace.new),
            count: replace.count,
        })
    }

    fn rendering(_: op::Replace) {}

    async fn render<U: Into<CallError> + Send + 'static>(
        (): (),
        running: op::Running<op::Replace, U>,
        _: &Call,
    ) -> Result<ProducedOutput, CallError> {
        ended(running).await??;
        Ok(ProducedOutput::new(serde_json::Value::Null))
    }
}

/// Remove a file, symlink or directory.
pub struct RemoveEntry;

impl TargetTool for RemoveEntry {
    const NAME: &'static str = "remove";
    const DESCRIPTION: &'static str = "Remove a workspace file, symlink, or directory";
    type Input = op::Remove;
    type Admitted = op::Remove;
    type Op = op::Remove;
    type Rendering = ();

    fn result_schema() -> Option<serde_json::Value> {
        Some(crate::schema::result_schema::<RemoveOutput>())
    }

    fn admit(remove: op::Remove) -> Result<op::Remove, AdmissionError> {
        Ok(remove)
    }

    fn paths(args: &mut op::Remove) -> Vec<PathArgument<'_>> {
        vec![PathArgument::new(
            &mut args.path,
            PathAccess::Write,
            PathKind::Removable,
        )]
    }

    fn op(remove: &mut op::Remove, _: &Call) -> Result<op::Remove, CallError> {
        let (path, recursive) = (remove.path.clone(), remove.recursive);
        Ok(op::Remove { path, recursive })
    }

    fn rendering(_: op::Remove) {}

    async fn render<U: Into<CallError> + Send + 'static>(
        (): (),
        running: op::Running<op::Remove, U>,
        _: &Call,
    ) -> Result<ProducedOutput, CallError> {
        let kind = ended(running).await??;
        Ok(ProducedOutput::serialized(RemoveOutput { kind })?)
    }
}

fn unchanged(operation: Operation, path: &op::TargetPath) -> PartialContext {
    PartialContext::new(operation, Subject::path(path)).effects(Effects::Unchanged)
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteArgs {
    /// File path
    path: op::TargetPath,
    /// Complete replacement file contents
    content: Option<String>,
    /// File whose bytes to copy instead of `content`
    source: Option<crate::target::TargetedPath>,
}

/// A write admitted with exactly one of content or a source.
pub struct Write {
    path: op::TargetPath,
    data: WriteData,
}

enum WriteData {
    Content(String),
    /// The executor loads the source's bytes into the call's context.
    Source(crate::target::TargetedPath),
}

impl Write {
    /// The file whose bytes the write copies, when it names one.
    pub fn source(&self) -> Option<&crate::target::TargetedPath> {
        match &self.data {
            WriteData::Source(source) => Some(source),
            WriteData::Content(_) => None,
        }
    }
}

impl WriteArgs {
    fn admit(self) -> Result<Write, AdmissionError> {
        let data = match (self.content, self.source) {
            (Some(content), None) if content.len() > MODEL_CONTENT_BYTES => {
                let message =
                    crate::diagnostic::write_too_large(content.len(), MODEL_CONTENT_BYTES);
                return Err(AdmissionError::failed(message)
                    .context(unchanged(Operation::Validate, &self.path)));
            }
            (Some(content), None) => WriteData::Content(content),
            (None, Some(source)) => WriteData::Source(source),
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
        })
    }
}

#[derive(Serialize, JsonSchema)]
struct WriteOutput {
    bytes: u64,
}

#[derive(Serialize, JsonSchema)]
struct RemoveOutput {
    kind: RemoveKind,
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use serde_json::json;
    use tokio::fs;

    use super::*;
    use crate::diagnostic::Cause;
    use crate::spec::tests::Workspace;
    use skyhook_worker::op::IoKind;

    #[tokio::test]
    async fn replacement_mismatch_reports_counts_and_lines_not_source_text() {
        let workspace = Workspace::new();
        let original = "private-search private-search\nother\nprivate-search\n";
        let path = workspace.path().join("mismatch.txt");
        fs::write(&path, original).await.unwrap();
        let arguments = json!({"path":"mismatch.txt", "old":"private-search",
            "new":"private-replacement", "count":2});
        let error = workspace.run("replace", arguments).await.unwrap_err();
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
        let workspace = Workspace::new();
        let oversized = "z".repeat(MODEL_CONTENT_BYTES + 1);
        for (tool, arguments) in [
            ("write", json!({"path":"missing/file", "content":oversized})),
            (
                "replace",
                json!({"path":"missing/file", "old":"x", "new":oversized}),
            ),
        ] {
            let diagnostic = workspace
                .run(tool, arguments)
                .await
                .unwrap_err()
                .diagnostic();
            assert_eq!(diagnostic.context.subject, Subject::path("missing/file"));
            assert_matches!(&diagnostic.cause,
                Cause::InvalidArguments(text) | Cause::Message(text) if text.contains("limit"),
                "{diagnostic:?}"
            );
        }
        assert!(!workspace.path().join("missing").exists());
    }

    /// A write creates the resolved path's missing parents, never below a file, and
    /// replaces an existing file without leftovers.
    #[tokio::test]
    async fn write_creates_missing_parents() {
        let workspace = Workspace::new();
        let root = workspace.path();
        let write = async |path: &str, content: &str| {
            let arguments = json!({"path": path, "content": content});
            workspace.run("write", arguments).await
        };
        fs::write(root.join("file"), "unchanged").await.unwrap();
        let absolute = root.join("absolute/nested/file.txt");
        for (path, written) in [
            ("relative/nested/file.txt", Some("relative/nested/file.txt")),
            (absolute.to_str().unwrap(), Some("absolute/nested/file.txt")),
            ("missing/../new/nested/file.txt", None),
            ("file/child.txt", None),
            ("file/nested/child.txt", None),
            ("file/../child.txt", None),
        ] {
            let result = write(path, "text").await;
            assert_eq!(result.is_ok(), written.is_some(), "{path}");
            if let Some(written) = written {
                let text = fs::read_to_string(root.join(written)).await.unwrap();
                assert_eq!(text, "text");
            }
        }
        for untouched in ["missing", "new"] {
            assert!(!root.join(untouched).exists(), "{untouched}");
        }
        let file = fs::read_to_string(root.join("file")).await.unwrap();
        assert_eq!(file, "unchanged");
        let path = "relative/nested/file.txt";
        write(path, "replacement").await.unwrap();
        let replaced = fs::read_to_string(root.join(path)).await.unwrap();
        assert_eq!(replaced, "replacement");
        let entries = std::fs::read_dir(root.join("relative/nested")).unwrap();
        let temporary = |name: std::ffi::OsString| name.to_string_lossy().starts_with(".skyhook-");
        assert!(
            !entries
                .into_iter()
                .any(|entry| temporary(entry.unwrap().file_name()))
        );
    }

    #[tokio::test]
    async fn remove_requires_recursion_and_refuses_the_workspace_root() {
        let workspace = Workspace::new();
        let remove = async |arguments| workspace.run("remove", arguments).await;
        let root = workspace.path();
        fs::create_dir(root.join("directory")).await.unwrap();
        fs::write(root.join("directory/file"), "data")
            .await
            .unwrap();
        let error = remove(json!({"path":"directory"})).await.unwrap_err();
        let Cause::Io(op::OsError {
            kind: IoKind::DirectoryNotEmpty,
            detail: Some(detail),
            ..
        }) = error.diagnostic().cause
        else {
            panic!("{error:?}")
        };
        assert!(detail.contains("recursive"), "{detail}");
        remove(json!({"path":"directory", "recursive":true}))
            .await
            .unwrap();
        assert!(!fs::try_exists(root.join("directory")).await.unwrap());
        std::fs::write(root.join("target"), "safe").unwrap();
        std::os::unix::fs::symlink("target", root.join("link")).unwrap();
        let result = remove(json!({"path":"link"})).await.unwrap();
        assert_eq!(result["kind"], "symlink");
        assert!(fs::try_exists(root.join("target")).await.unwrap());
        assert!(!fs::try_exists(root.join("link")).await.unwrap());
        let error = remove(json!({"path":"."})).await.unwrap_err();
        assert!(error.to_string().contains("authorization root"));
        // A tree holding a live database is refused unchanged, with nothing deleted.
        std::fs::create_dir(root.join("store")).unwrap();
        let live = root.join("store/live.db");
        std::fs::write(&live, b"").unwrap();
        let held = skyhook_worker::fs::live::Held::new(&live, std::time::Duration::ZERO).unwrap();
        let error = remove(json!({"path":"store", "recursive":true}))
            .await
            .unwrap_err();
        let effects = error.diagnostic().context.effects;
        assert_eq!(effects, Effects::Unchanged, "{error:?}");
        assert!(fs::try_exists(&live).await.unwrap());
        drop(held);
    }
}
