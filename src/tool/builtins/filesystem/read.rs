//! File snapshots, directory listings, images, and expected read failures.
use crate::tool::ToolOptions;
use crate::tool::diagnostic::{Cause, Diagnostic, Effects, IoKind, Operation, Subject};
use crate::tool::invocation::{LocalCatalogBuilder, LocalContext, LocalError};
use crate::tool::output::ProducedOutput;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::fs;

use super::super::workspace::relative_path;
use crate::{
    bounded_io::{BoundedReadError, read_bounded},
    media::{ImageRef, MAX_IMAGE_BYTES},
    tool::output::{FinishedOutput, TextCaptureField},
    tool::{
        PathKind, RegistryError,
        policy::{Capability, PathAccess},
    },
};

pub(super) fn register(builder: &mut LocalCatalogBuilder) -> Result<(), RegistryError> {
    builder.register_product::<ReadArgs, ReadOutput, _, _>(
        "read",
        "Capture a complete UTF-8 file or directory listing, or attach a supported image. Use job_output to page or search the saved snapshot. Missing or OS-inaccessible paths return {kind:\"error\",path,error:{code:\"not_found\"|\"permission_denied\",message}} as a completed result; policy/approval denials remain denials.",
        ToolOptions::new(vec![Capability::Read])
            .read_error_output(read_error_output)
            .placement(crate::tool::ToolPlacement::TargetedWorkspace)
            .path_argument("path", PathAccess::Read, PathKind::Existing),
        read,
    )?;
    Ok(())
}

async fn read(context: LocalContext, args: ReadArgs) -> Result<ProducedOutput, LocalError> {
    let path = std::path::PathBuf::from(&args.path);
    if fs::metadata(&path)
        .await
        .map_err(source(Operation::Inspect, &path))?
        .is_dir()
    {
        let mut directory = fs::read_dir(&path)
            .await
            .map_err(source(Operation::ReadDirectory, &path))?;
        let mut entries = Vec::new();
        while let Some(entry) = directory
            .next_entry()
            .await
            .map_err(source(Operation::ReadDirectory, &path))?
        {
            let metadata = fs::symlink_metadata(entry.path()).await.map_err(|error| {
                LocalError::source_filesystem_io(error)
                    .operation(Operation::Inspect, Subject::DirectoryEntry(entry.path()))
            })?;
            entries.push(DirectoryEntry::from_metadata(
                entry.file_name().to_string_lossy().into_owned(),
                &metadata,
            ));
        }
        entries.sort_by(|left, right| left.name().cmp(right.name()));
        let directory_path = relative_path(&context.execution_location().workspace, &path);
        return Ok(ProducedOutput::new(serde_json::to_value(
            ReadOutput::Directory {
                path: directory_path,
                entries: if args.details {
                    DirectoryEntries::Detailed(entries)
                } else {
                    DirectoryEntries::Grouped(DirectoryGroups::from(entries))
                },
            },
        )?));
    }

    let output_path = relative_path(&context.execution_location().workspace, &path);
    match read_text(&path, &context).await {
        Ok(TextReadOutcome::Captured(capture)) => {
            return Ok(ProducedOutput::new(serde_json::to_value(ReadOutput::File {
                path: output_path,
                content: String::new(),
            })?)
            .with_captures(vec![capture]));
        }
        Ok(TextReadOutcome::NotUtf8) => {}
        Err(error) => return Err(error),
    }

    let metadata = fs::metadata(&path)
        .await
        .map_err(source(Operation::Inspect, &path))?;
    if metadata.len() > MAX_IMAGE_BYTES {
        return Err(LocalError::Failed(format!(
            "non-UTF-8 file is {} bytes, exceeding the {MAX_IMAGE_BYTES}-byte image limit",
            metadata.len()
        ))
        .operation(Operation::Read, Subject::path(&path)));
    }
    let mut input = fs::File::open(&path)
        .await
        .map_err(source(Operation::Read, &path))?;
    let bytes = read_bounded(&mut input, MAX_IMAGE_BYTES as usize)
        .await
        .map_err(|error| match error {
            BoundedReadError::Io(error) => source(Operation::Read, &path)(error),
            error => LocalError::Failed(error.to_string())
                .operation(Operation::Read, Subject::path(&path)),
        })?;
    let image = crate::media::Image::new(bytes).map_err(|_| {
        LocalError::Failed("file is neither UTF-8 nor a supported image".to_owned())
            .operation(Operation::Deserialize, Subject::path(&path))
    })?;
    let reference = context
        .store_image(Some(output_path.clone()), &image)
        .await
        .map_err(|error| error.operation(Operation::StoreImage, Subject::path(&path)))?;
    Ok(ProducedOutput::new(serde_json::to_value(ReadOutput::Image {
        path: output_path,
        image: reference.clone(),
    })?)
    .with_images(vec![reference]))
}

/// Only failures of the requested source may complete as read-error results.
fn source(
    operation: Operation,
    path: &std::path::Path,
) -> impl FnOnce(std::io::Error) -> LocalError + use<> {
    let subject = Subject::path(path);
    move |error| LocalError::source_filesystem_io(error).operation(operation, subject)
}

enum TextReadOutcome {
    Captured(FinishedOutput),
    NotUtf8,
}

async fn read_text(
    path: &std::path::Path,
    context: &LocalContext,
) -> Result<TextReadOutcome, LocalError> {
    let mut capture = context
        .text_capture(TextCaptureField::Content)
        .await
        .map_err(|error| error.operation(Operation::CreateCapture, Subject::path(path)))?
        .open();
    let subject = Subject::path(path);
    let path = path.to_owned();
    let cancellation = context.cancellation_token().child_token();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    // Reading, capture writes and finishing run in one blocking owner; an abandoned
    // capture discards itself.
    let completed = tokio::task::spawn_blocking(move || -> Result<_, LocalError> {
        let mut input = std::fs::File::open(&path).map_err(source(Operation::Read, &path))?;
        match copy_utf8(
            &path,
            &mut input,
            |text| capture.write_text(text),
            &cancellation,
        )? {
            Utf8Read::Complete => Ok(Some(capture.finish().map_err(|error| {
                LocalError::Io(error).operation(Operation::FinishCapture, Subject::path(&path))
            })?)),
            Utf8Read::NotUtf8 => Ok(None),
        }
    })
    .await
    .map_err(|error| {
        LocalError::failed(error)
            .operation(Operation::Wait, subject)
            .effects(Effects::OutputIncomplete)
    })??;
    if let Some(completed) = completed {
        Ok(TextReadOutcome::Captured(completed))
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
    path: &std::path::Path,
    input: &mut impl std::io::Read,
    mut output: impl FnMut(&str) -> std::io::Result<()>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<Utf8Read, LocalError> {
    let mut buffer = [0; 64 * 1024];
    let mut pending = Vec::new();
    let cancelled = || {
        LocalError::Cancelled
            .operation(Operation::Read, Subject::path(path))
            .effects(Effects::OutputIncomplete)
    };
    let capture =
        |error| LocalError::Io(error).operation(Operation::WriteCapture, Subject::path(path));
    loop {
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        let size = input
            .read(&mut buffer)
            .map_err(source(Operation::Read, path))?;
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        pending.extend_from_slice(&buffer[..size]);
        match std::str::from_utf8(&pending) {
            Ok(text) => {
                output(text).map_err(capture)?;
                pending.clear();
            }
            Err(error) if error.error_len().is_none() && size != 0 => {
                let valid = error.valid_up_to();
                output(std::str::from_utf8(&pending[..valid]).expect("validated UTF-8 prefix"))
                    .map_err(capture)?;
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

fn read_error_output(path: &str, diagnostic: &Diagnostic) -> Option<serde_json::Value> {
    let code = match diagnostic.cause {
        Cause::Io {
            kind: IoKind::NotFound,
            ..
        } => ReadErrorCode::NotFound,
        Cause::Io {
            kind: IoKind::PermissionDenied,
            ..
        } => ReadErrorCode::PermissionDenied,
        _ => return None,
    };
    Some(
        serde_json::to_value(ReadOutput::Error {
            path: path.to_owned(),
            error: ReadError {
                code,
                // The output owns the diagnostic; presentation fills this field
                // with the caller-capability-aware common renderer.
                message: String::new(),
            },
        })
        .expect("read errors serialize"),
    )
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
    files: Vec<DirectoryFile>,
    directories: Vec<String>,
    symlinks: Vec<String>,
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
    use crate::tool::ToolRegistryBuilder;
    use crate::tool::executor::ToolExecutor;

    fn read_executor(
        runtime: &crate::tests::TestRuntime,
    ) -> (ToolExecutor, Arc<std::sync::OnceLock<ToolExecutor>>) {
        let mut builder = ToolRegistryBuilder::default();
        builder.register_local(register).unwrap();
        let slot = Arc::new(std::sync::OnceLock::new());
        crate::tool::builtins::install_script_tool(&mut builder, Arc::downgrade(&slot)).unwrap();
        let executor = runtime.executor(builder);
        assert!(slot.set(executor.clone()).is_ok());
        (executor, slot)
    }

    #[test]
    fn grouped_directory_entries_keep_empty_groups() {
        let groups = DirectoryGroups::from(vec![DirectoryEntry::File {
            name: "one.txt".into(),
            bytes: 3,
        }]);
        assert_eq!(
            serde_json::to_value(groups).unwrap(),
            json!({
                "files":[{"name":"one.txt", "bytes":3}],
                "directories":[],
                "symlinks":[],
                "other":[]
            })
        );
    }

    /// Schema-valid input rejected by typed admission still owns a job, which
    /// fails with the admission error before any IO or script evaluation.
    #[tokio::test]
    async fn typed_read_and_script_admission_fails_the_job_before_io_or_evaluation() {
        let runtime = crate::tests::TestRuntime::new().await;
        let (executor, _slot) = read_executor(&runtime);
        for (index, (tool, arguments, argument)) in [
            ("read", json!({"path":".", "details":"invalid"}), "/details"),
            ("script", json!({"source":42}), "/source"),
        ]
        .into_iter()
        .enumerate()
        {
            let error = executor.run_host(&runtime.agent, tool, arguments).await;
            let error =
                error.expect_err("typed arguments must reject before IO or script evaluation");
            let diagnostic = error.diagnostic();
            assert_eq!(diagnostic.context.operation, Operation::Deserialize);
            assert_eq!(
                diagnostic.context.subject,
                Subject::Argument(argument.into())
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
        let copy = |mut reader: &mut dyn std::io::Read| {
            copy_utf8(
                std::path::Path::new("input.txt"),
                &mut reader,
                |_| Ok(()),
                &cancellation,
            )
        };
        assert_eq!(copy(&mut &b"valid\xff"[..]).unwrap(), Utf8Read::NotUtf8);
        assert!(matches!(
            copy(&mut Reader(None)).unwrap_err().diagnostic().cause,
            Cause::Io {
                kind: IoKind::InvalidData,
                ..
            }
        ));
        // The first reader cancels during copying; the second starts cancelled.
        for reader in [
            &mut Reader(Some(cancellation.clone())) as &mut dyn std::io::Read,
            &mut &b"data"[..],
        ] {
            assert_eq!(
                copy(reader).unwrap_err().diagnostic().cause,
                Cause::Cancelled
            );
        }
    }

    #[tokio::test]
    async fn capture_permission_failures_are_not_completed_source_read_errors() {
        use crate::execution::ExecutionLocation;
        use crate::tool::invocation::{
            LocalCatalog,
            tests::{Authorizations, CapturedOutput},
        };
        use crate::tool::output::OutputContext;
        use crate::tool::output::tests::{CaptureStage, FailingCapture};

        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("source.txt"), "captured text")
            .await
            .unwrap();
        let catalog = LocalCatalog::builtins().unwrap();
        for (stage, operation) in [
            (CaptureStage::Open, Operation::CreateCapture),
            (CaptureStage::Write, Operation::WriteCapture),
            (CaptureStage::Finish, Operation::FinishCapture),
        ] {
            let output = OutputContext::new(Arc::new(FailingCapture::new(
                Arc::new(CapturedOutput::default()),
                stage,
                0,
                || std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            )));
            let arguments = json!({"path":"source.txt"});
            let context = LocalContext::new(
                ExecutionLocation::root(root.path().to_owned()),
                [Capability::Read].into_iter().collect(),
                Default::default(),
                CancellationToken::new(),
                output.clone(),
                Arc::new(Authorizations::default()),
                arguments.clone(),
            );
            let error = catalog
                .run("read", arguments, context, root.path())
                .await
                .unwrap_err();
            assert_eq!(error.diagnostic().context.operation, operation);
            assert!(matches!(
                error.diagnostic().cause,
                Cause::Io {
                    kind: IoKind::PermissionDenied,
                    ..
                }
            ));
            output.settle().await.unwrap();
        }
    }

    #[tokio::test]
    async fn missing_reads_complete_direct_and_script_jobs() {
        let runtime = crate::tests::TestRuntime::new().await;
        let (executor, _slot) = read_executor(&runtime);
        let direct = executor
            .run_host(
                &runtime.agent,
                "read",
                json!({"path":"missing/nested/file.txt"}),
            )
            .await
            .unwrap();
        let value = &direct.output.value;
        assert_eq!(value["kind"], "error");
        assert_eq!(value["path"], "missing/nested/file.txt");
        assert_eq!(value["error"]["code"], "not_found");
        assert!(!value["error"]["message"].as_str().unwrap().is_empty());
        assert_eq!(
            runtime.jobs.snapshot(direct.job).await.unwrap().state,
            JobState::Completed
        );

        // Script unwrap must resolve the completed read error, not throw it.
        let script = executor
            .run_host(
                &runtime.agent,
                "script",
                json!({"source":
                    "return (await tool.read({path:'missing'})).unwrap();"
                }),
            )
            .await
            .unwrap();
        assert_eq!(script.output.value["value"]["error"]["code"], "not_found");
        assert!(script.output.value["failure"].is_null());
        assert_eq!(
            runtime.jobs.snapshot(script.job).await.unwrap().state,
            JobState::Completed
        );
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
        assert_eq!(page["presentation"]["preview"]["lines"][0], "two");
    }
}
