use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use grep_matcher::Matcher as _;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::workspace::relative_path;
use crate::job::output::{CaptureWriter, CompletedCapture, PendingCapture};
use crate::tool::{
    PathKind, RegistryError, ToolError, ToolOptions, ToolRegistryBuilder,
    policy::{Capability, PathAccess},
};

pub(super) fn register(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    builder.register_product::<SearchArgs, SearchOutput, _, _>(
        "search",
        "Search files with ripgrep regex, glob, and ignore semantics. Matches map file paths to arrays of \"line: text\" strings; details returns structured matches.",
        ToolOptions::new(vec![Capability::Read])
            .placement(crate::tool::ToolPlacement::TargetedWorkspace)
            .default_path_argument("path", ".", PathAccess::Read, PathKind::Existing),
        |context, args| async move {
            let root = PathBuf::from(&args.path);
            let capture = context.pending_stream_capture("/result/matches", crate::job::output::CaptureKind::Json).await?;
            let workspace = context.execution_location().workspace.clone();
            tokio::task::spawn_blocking(move || {
                search_blocking(&workspace, &root, &args, capture, &context.cancellation_token())
            })
            .await
            .map_err(|error| ToolError::Failed(error.to_string()))?
            .and_then(|capture| {
                let output = SearchOutput { matches: SearchMatches::Grouped(BTreeMap::new()) };
                Ok(crate::tool::ToolOutput::new(serde_json::to_value(output)?).with_captures(vec![capture]))
            })
        },
    )?;
    builder.register_product::<GlobArgs, GlobOutput, _, _>(
        "glob",
        "Find files with ripgrep glob and ignore semantics.",
        ToolOptions::new(vec![Capability::Read])
            .placement(crate::tool::ToolPlacement::TargetedWorkspace)
            .default_path_argument("path", ".", PathAccess::Read, PathKind::Existing),
        |context, args| async move {
            let root = PathBuf::from(&args.path);
            let capture = context
                .pending_stream_capture("/result/paths", crate::job::output::CaptureKind::Json)
                .await?;
            let workspace = context.execution_location().workspace.clone();
            tokio::task::spawn_blocking(move || {
                glob_blocking(
                    &workspace,
                    &root,
                    &args,
                    capture,
                    &context.cancellation_token(),
                )
            })
            .await
            .map_err(|error| ToolError::Failed(error.to_string()))?
            .map(|capture| {
                crate::tool::ToolOutput::new(serde_json::json!({})).with_captures(vec![capture])
            })
        },
    )?;
    Ok(())
}

fn walk_builder(
    root: &Path,
    hidden: bool,
    no_ignore: bool,
    patterns: &[String],
) -> Result<WalkBuilder, ToolError> {
    let patterns = overrides(root, patterns)?;
    let mut walk = WalkBuilder::new(root);
    walk.hidden(!hidden)
        .ignore(!no_ignore)
        .git_ignore(!no_ignore)
        .git_exclude(!no_ignore)
        .git_global(false)
        .parents(!no_ignore)
        .follow_links(false)
        .sort_by_file_path(std::cmp::Ord::cmp)
        // Apply globs after normal ignore eligibility, including directory
        // exclusions so excluded subtrees are never traversed.
        .filter_entry(move |entry| {
            entry.depth() == 0
                || !patterns
                    .matched(
                        entry.path(),
                        entry.file_type().is_some_and(|kind| kind.is_dir()),
                    )
                    .is_ignore()
        });
    Ok(walk)
}

fn overrides(root: &Path, patterns: &[String]) -> Result<ignore::overrides::Override, ToolError> {
    let mut builder = OverrideBuilder::new(root);
    for pattern in patterns {
        builder
            .add(pattern)
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
    }
    builder
        .build()
        .map_err(|error| ToolError::InvalidArguments(error.to_string()))
}

fn search_blocking(
    workspace: &Path,
    root: &Path,
    args: &SearchArgs,
    capture: PendingCapture,
    cancellation: &crate::job::CancellationToken,
) -> Result<CompletedCapture, ToolError> {
    let mut matcher_builder = common_matcher();
    matcher_builder.fixed_strings(args.fixed).word(args.word);
    match args.case {
        Case::Smart => {
            matcher_builder.case_smart(true);
        }
        Case::Sensitive => {
            matcher_builder.case_insensitive(false).case_smart(false);
        }
        Case::Insensitive => {
            matcher_builder.case_insensitive(true).case_smart(false);
        }
    }
    let matcher = matcher_builder
        .build(&args.pattern)
        .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;

    let files: Box<dyn Iterator<Item = Result<PathBuf, ToolError>>> = if root.is_file() {
        Box::new(std::iter::once(Ok(root.to_owned())))
    } else if root.is_dir() {
        let walk = walk_builder(root, args.hidden, args.no_ignore, &args.glob)?;
        Box::new(walk.build().filter_map(|entry| match entry {
            Ok(entry) if entry.file_type().is_some_and(|kind| kind.is_file()) => {
                Some(Ok(entry.into_path()))
            }
            Ok(_) => None,
            Err(error) => Some(Err(ToolError::Failed(error.to_string()))),
        }))
    } else {
        return Err(ToolError::Failed(
            "search root is not a file or directory".into(),
        ));
    };
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(b'\0'))
        .heap_limit(Some(4 * 1024 * 1024))
        .build();
    let mut matches = CapturedOutput::new(capture, !args.details)?;
    for path in files {
        if cancellation.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let path = path?;
        let checkpoint = matches.checkpoint()?;
        let mut sink = SearchSink {
            matcher: &matcher,
            path: relative_path(workspace, &path),
            matches: &mut matches,
            binary: false,
            cancellation,
        };
        searcher
            .search_path(&matcher, &path, &mut sink)
            .map_err(SearchStop::into_tool_error)?;
        if sink.binary {
            matches.rollback(checkpoint)?;
        }
    }
    Ok(matches.finish()?)
}

fn glob_blocking(
    workspace: &Path,
    root: &Path,
    args: &GlobArgs,
    capture: PendingCapture,
    cancellation: &crate::job::CancellationToken,
) -> Result<CompletedCapture, ToolError> {
    if !root.is_dir() {
        return Err(ToolError::Failed("glob root is not a directory".into()));
    }
    let walk = walk_builder(
        root,
        args.hidden,
        args.no_ignore,
        std::slice::from_ref(&args.pattern),
    )?;
    let mut paths = CapturedOutput::new(capture, false)?;
    for entry in walk.build() {
        if cancellation.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let entry = entry.map_err(|error| ToolError::Failed(error.to_string()))?;
        if entry.depth() != 0 && entry.file_type().is_some_and(|kind| kind.is_file()) {
            paths.push(relative_path(workspace, entry.path()))?;
        }
    }
    Ok(paths.finish()?)
}

/// Streams the complete result into the owning job's capture. The handler returns
/// completed ownership evidence; the finalizer publishes the captured field.
struct CapturedOutput {
    file: CaptureWriter,
    count: usize,
    grouped: bool,
    group: Option<String>,
}
impl CapturedOutput {
    fn new(capture: PendingCapture, grouped: bool) -> std::io::Result<Self> {
        use std::io::Write as _;
        let mut file = capture.open();
        file.write_all(if grouped { b"{\n" } else { b"[\n" })?;
        Ok(Self {
            file,
            count: 0,
            grouped,
            group: None,
        })
    }
    fn push(&mut self, value: impl Serialize) -> std::io::Result<()> {
        use std::io::Write as _;
        if self.count > 0 {
            self.file.write_all(b",\n")?;
        }
        serde_json::to_writer(&mut self.file, &value)?;
        self.count += 1;
        Ok(())
    }
    fn checkpoint(&mut self) -> std::io::Result<(usize, u64, Option<String>)> {
        use std::io::Seek as _;
        Ok((self.count, self.file.stream_position()?, self.group.clone()))
    }
    fn rollback(&mut self, checkpoint: (usize, u64, Option<String>)) -> std::io::Result<()> {
        use std::io::{Seek as _, Write as _};
        self.count = checkpoint.0;
        self.group = checkpoint.2;
        self.file.flush()?;
        self.file.truncate(checkpoint.1)?;
        self.file.seek(std::io::SeekFrom::Start(checkpoint.1))?;
        Ok(())
    }
    fn finish(mut self) -> std::io::Result<CompletedCapture> {
        use std::io::Write as _;
        if self.grouped {
            if self.group.is_some() {
                self.file.write_all(b"\n]")?;
            }
            self.file.write_all(b"\n}")?;
        } else {
            self.file.write_all(b"\n]")?;
        }
        self.file.finish()
    }
    fn push_match(&mut self, value: SearchMatch) -> std::io::Result<()> {
        use std::io::Write as _;
        if !self.grouped {
            return self.push(value);
        }
        if self.group.as_deref() == Some(&value.path) {
            self.file.write_all(b",\n")?;
        } else {
            if self.group.is_some() {
                self.file.write_all(b"\n],\n")?;
            }
            serde_json::to_writer(&mut self.file, &value.path)?;
            self.file.write_all(b": [\n")?;
            self.group = Some(value.path);
        }
        serde_json::to_writer(&mut self.file, &format!("{}: {}", value.line, value.text))?;
        self.count += 1;
        Ok(())
    }
}

/// Grep errors retain their cause; cancellation is never inferred from text or
/// from a token observed later than an unrelated IO failure.
#[derive(Debug)]
enum SearchStop {
    Cancelled,
    Io(std::io::Error),
}

impl grep_searcher::SinkError for SearchStop {
    fn error_message<T: std::fmt::Display>(message: T) -> Self {
        Self::Io(std::io::Error::other(message.to_string()))
    }
    fn error_io(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<std::io::Error> for SearchStop {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl SearchStop {
    fn into_tool_error(self) -> ToolError {
        match self {
            Self::Cancelled => ToolError::Cancelled,
            // Preserve the existing grep IO failure presentation.
            Self::Io(error) => ToolError::Failed(error.to_string()),
        }
    }
}

struct SearchSink<'a> {
    matcher: &'a grep_regex::RegexMatcher,
    path: String,
    matches: &'a mut CapturedOutput,
    binary: bool,
    cancellation: &'a crate::job::CancellationToken,
}
impl Sink for SearchSink<'_> {
    type Error = SearchStop;
    fn matched(
        &mut self,
        _searcher: &Searcher,
        matched: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        if self.cancellation.is_cancelled() {
            return Err(SearchStop::Cancelled);
        }
        let bytes = matched.bytes();
        let first = self
            .matcher
            .find(bytes)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        if let Some(first) = first {
            self.matches.push_match(SearchMatch {
                path: self.path.clone(),
                line: matched.line_number().unwrap_or(1),
                column: first.start() + 1,
                text: String::from_utf8_lossy(bytes)
                    .trim_end_matches(['\r', '\n'])
                    .to_owned(),
            })?;
        }
        Ok(true)
    }
    fn binary_data(&mut self, _searcher: &Searcher, _offset: u64) -> Result<bool, Self::Error> {
        self.binary = true;
        Ok(false)
    }
}

#[derive(Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Case {
    #[default]
    Smart,
    Sensitive,
    Insensitive,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    /// Rust-regex pattern, using ripgrep syntax. Set `fixed` to match it literally.
    pattern: String,
    /// Return structured matches including columns instead of grouping by file.
    #[serde(default)]
    details: bool,
    /// File or directory to search.
    #[serde(default = "default_dot")]
    path: String,
    /// Ripgrep-style include/exclude globs. Prefix exclusions with `!`; later globs override earlier ones.
    #[serde(default)]
    glob: Vec<String>,
    /// Treat `pattern` as literal text instead of a regular expression (`rg -F`).
    #[serde(default)]
    fixed: bool,
    /// Case matching mode.
    #[serde(default)]
    case: Case,
    /// Require whole-word matches (`rg -w`).
    #[serde(default)]
    word: bool,
    /// Include hidden files and directories (`rg --hidden`).
    #[serde(default)]
    hidden: bool,
    /// Ignore .gitignore and related ignore files (`rg --no-ignore`).
    #[serde(default)]
    no_ignore: bool,
}

#[derive(Serialize, JsonSchema)]
struct SearchOutput {
    #[schemars(extend("x-skyhook-truncatable" = true))]
    matches: SearchMatches,
}

#[derive(Serialize, JsonSchema)]
#[serde(untagged)]
enum SearchMatches {
    Grouped(BTreeMap<String, Vec<String>>),
    #[expect(
        dead_code,
        reason = "schema-only alternative; completed captures own detailed output"
    )]
    Detailed(Vec<SearchMatch>),
}

#[derive(Serialize, JsonSchema)]
#[cfg_attr(test, derive(Deserialize))]
struct SearchMatch {
    path: String,
    line: u64,
    column: usize,
    text: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GlobArgs {
    /// Ripgrep-style file glob, such as `src/**/*.rs`.
    pattern: String,
    /// Directory to enumerate.
    #[serde(default = "default_dot")]
    path: String,
    /// Include hidden files and directories.
    #[serde(default)]
    hidden: bool,
    /// Ignore .gitignore and related ignore files.
    #[serde(default)]
    no_ignore: bool,
}

#[derive(Serialize, JsonSchema)]
struct GlobOutput {
    #[schemars(extend("x-skyhook-truncatable" = true))]
    paths: Vec<String>,
}

fn default_dot() -> String {
    ".".to_owned()
}
fn common_matcher() -> RegexMatcherBuilder {
    let mut builder = RegexMatcherBuilder::new();
    builder
        .line_terminator(Some(b'\n'))
        .ban_byte(Some(b'\0'))
        .size_limit(10 * 1024 * 1024)
        .dfa_size_limit(10 * 1024 * 1024);
    builder
}

pub(crate) fn output_matcher(pattern: &str) -> Result<grep_regex::RegexMatcher, ToolError> {
    common_matcher()
        .build(pattern)
        .map_err(|error| ToolError::InvalidArguments(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn pending_capture(directory: &Path) -> PendingCapture {
        let job = crate::identity::JobId::new(1).unwrap();
        let kind = crate::job::output::CaptureKind::Json;
        PendingCapture::create(job, directory, "/result/test", kind).unwrap()
    }

    /// Runs a blocking walker into a fresh capture and decodes what it wrote.
    fn captured<T: serde::de::DeserializeOwned, R>(
        walk: impl FnOnce(PendingCapture, &crate::job::CancellationToken) -> Result<R, ToolError>,
    ) -> Result<T, ToolError> {
        let directory = tempfile::tempdir()?;
        walk(
            pending_capture(directory.path()),
            &crate::job::CancellationToken::new(),
        )?;
        let file = crate::job::output::field_file(directory.path(), "/result/test");
        Ok(serde_json::from_reader(std::fs::File::open(file)?)?)
    }

    fn search(root: &Path, path: &Path, args: Value) -> Vec<SearchMatch> {
        let args: SearchArgs = serde_json::from_value(args).unwrap();
        assert!(args.details);
        captured(|capture, cancel| search_blocking(root, path, &args, capture, cancel)).unwrap()
    }

    fn glob(root: &Path, path: &Path, args: Value) -> Vec<String> {
        let args: GlobArgs = serde_json::from_value(args).unwrap();
        captured(|capture, cancel| glob_blocking(root, path, &args, capture, cancel)).unwrap()
    }

    fn write_files(root: &Path, files: &[&str]) {
        for file in files {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "needle\n").unwrap();
        }
    }

    #[test]
    fn cancellation_before_first_file_is_typed() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input");
        std::fs::write(&input, "needle").unwrap();
        let args: SearchArgs = serde_json::from_value(json!({"pattern":"needle"})).unwrap();
        let cancellation = crate::job::CancellationToken::new();
        cancellation.cancel();
        let capture = pending_capture(root.path());
        let result = search_blocking(root.path(), &input, &args, capture, &cancellation);
        assert!(matches!(result, Err(ToolError::Cancelled)));
    }

    #[tokio::test]
    async fn grouped_search_is_captured_and_preserves_whitespace_and_binary_rollback() {
        let runtime = crate::tests::TestRuntime::new().await;
        let directory = runtime.root.path().join("files");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("a.txt"), "  needle  \nneedle again\n").unwrap();
        std::fs::write(directory.join("b.bin"), b"needle\n\0hidden").unwrap();
        std::fs::write(directory.join("z.txt"), "needle last\n").unwrap();
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder).unwrap();
        let executor = runtime.executor(builder);
        let search = async |args| {
            executor
                .run_host(&runtime.agent, "search", args)
                .await
                .unwrap()
        };
        let captured = search(json!({"path":"files","pattern":"needle"})).await;
        let grouped = json!({"matches":{
            "files/a.txt":["1:   needle  ", "2: needle again"], "files/z.txt":["1: needle last"]
        }});
        assert_eq!(captured.output.value, grouped);
        let empty = search(json!({"path":"files","pattern":"absent"})).await;
        assert_eq!(empty.output.value, json!({"matches":{}}));
        let detailed = search(json!({"path":"files","pattern":"needle","details":true})).await;
        let first = json!({"path":"files/a.txt","line":1,"column":3,"text":"  needle  "});
        assert_eq!(detailed.output.value["matches"][0], first);
        std::fs::write(directory.join("a.txt"), "needle λ\n".repeat(500)).unwrap();
        let args = json!({"path":"files","pattern":"needle"});
        let preview = executor
            .run_model(&runtime.agent, "search", args)
            .await
            .unwrap();
        let matches = serde_json::to_vec(&preview.output.value["result"]["matches"]).unwrap();
        assert!(matches.len() <= 2048);
        assert_eq!(
            preview.output.value["truncated"][0]["field"],
            "/result/matches"
        );
        let mut query = crate::job::JobOutputQuery::new(preview.job);
        query.field = Some("/result/matches".into());
        query.pattern = Some("500: needle".into());
        let page = runtime
            .jobs
            .inspect_output(query, &Default::default())
            .await
            .unwrap();
        let lines = page["preview"]["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].as_str().unwrap().contains("500: needle λ"));
    }

    #[test]
    fn wildcard_filters_respect_hidden_and_ignore_flags() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_files(
            root,
            &[
                ".git/config",
                ".hidden/secret",
                "build/generated",
                "src/visible",
            ],
        );
        std::fs::write(root.join(".gitignore"), "build/\n").unwrap();
        for hidden in [false, true] {
            for no_ignore in [false, true] {
                for pattern in ["*", "**/*"] {
                    let args = json!({"pattern":pattern, "hidden":hidden, "no_ignore":no_ignore});
                    let paths = glob(root, root, args);
                    let has = |path: &str| paths.iter().any(|p| p == path);
                    assert!(has("src/visible"));
                    assert_eq!(
                        (has(".git/config"), has(".hidden/secret")),
                        (hidden, hidden)
                    );
                    assert_eq!(has("build/generated"), no_ignore);
                    let args = json!({
                        "pattern":"needle", "details":true, "glob":[pattern], "hidden":hidden, "no_ignore":no_ignore
                    });
                    let matches = search(root, root, args);
                    let has = |path: &str| matches.iter().any(|m| m.path == path);
                    assert!(has("src/visible"));
                    assert_eq!(
                        (has(".git/config"), has("build/generated")),
                        (hidden, no_ignore)
                    );
                }
            }
        }
        // An explicitly requested hidden root remains accessible.
        let paths = glob(
            root,
            &root.join(".git"),
            json!({"pattern":"*", "path":".git"}),
        );
        assert_eq!(paths, [".git/config"]);
    }

    #[test]
    fn subdirectory_roots_inherit_ignores_and_globs_keep_precedence() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        write_files(
            root,
            &[
                "src/nested/file.rs",
                "src/a.rs",
                "src/b.rs",
                "src/generated.rs",
                "src/skip.rs",
            ],
        );
        std::fs::write(root.join(".gitignore"), "/src/generated.rs\n").unwrap();
        std::fs::write(root.join(".ignore"), "skip.rs\n").unwrap();
        let src = root.join("src");
        let paths = glob(root, &src, json!({"pattern":"*.rs", "path":"src"}));
        assert_eq!(paths, ["src/a.rs", "src/b.rs", "src/nested/file.rs"]);
        for (glob, expected) in [
            (
                vec!["*.rs", "!b.rs"],
                vec!["src/a.rs", "src/nested/file.rs"],
            ),
            (
                vec!["*.rs", "!b.rs", "b.rs"],
                vec!["src/a.rs", "src/b.rs", "src/nested/file.rs"],
            ),
            (vec!["!b.rs"], vec!["src/a.rs", "src/nested/file.rs"]),
            (vec!["*.rs", "!nested/"], vec!["src/a.rs", "src/b.rs"]),
        ] {
            let args = json!({"pattern":"needle", "details":true, "path":"src", "glob":glob});
            let matches = search(root, &src, args);
            assert_eq!(
                matches.iter().map(|m| m.path.as_str()).collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn ripgrep_smart_case_binary_detection_and_sorted_relative_globs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join("upper.rs"), "Needle\n").unwrap();
        std::fs::write(root.join("lower.rs"), "needle\n").unwrap();
        std::fs::write(root.join("skip.txt"), "Needle\n").unwrap();
        std::fs::write(root.join("binary.rs"), b"Needle\0hidden\n").unwrap();
        let args = json!({"pattern":"Needle", "details":true, "glob":["*.rs", "!binary.rs"], "case":"smart"});
        let output = search(root, root, args);
        assert_eq!(output.len(), 1);
        assert_eq!(
            (output[0].path.as_str(), output[0].line, output[0].column),
            ("upper.rs", 1, 1)
        );
        let paths = glob(root, root, json!({"pattern":"*.rs"}));
        assert_eq!(paths, ["binary.rs", "lower.rs", "upper.rs"]);
    }
}
