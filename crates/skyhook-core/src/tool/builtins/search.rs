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

use super::workspace::{relative_path, resolve_existing};
use crate::tool::{
    PathKind, RegistryError, ToolError, ToolOptions, ToolRegistryBuilder,
    policy::{Capability, PathAccess},
};

pub(super) fn register(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    builder.register::<SearchArgs, SearchOutput, _, _>(
        "search",
        "Search files with ripgrep regex, glob, and ignore semantics. Matches map file paths to arrays of \"line: text\" strings; details returns structured matches.",
        ToolOptions::new(vec![Capability::Read])
            .placement(crate::tool::ToolPlacement::TargetedWorkspace)
            .path_argument("path", PathAccess::Read, PathKind::Existing),
        |context, args| async move {
            let root = resolve_existing(&context.execution_location.workspace, &args.path).await?;
            let capture = context.capture_path("/result/matches").await?;
            let workspace = context.execution_location.workspace.clone();
            tokio::task::spawn_blocking(move || {
                search_blocking(&workspace, &root, &args, &capture, &context.cancellation_token())
            })
            .await
            .map_err(|error| ToolError::Failed(error.to_string()))?
        },
    )?;
    builder.register::<GlobArgs, GlobOutput, _, _>(
        "glob",
        "Find files with ripgrep glob and ignore semantics.",
        ToolOptions::new(vec![Capability::Read])
            .placement(crate::tool::ToolPlacement::TargetedWorkspace)
            .path_argument("path", PathAccess::Read, PathKind::Existing),
        |context, args| async move {
            let root = resolve_existing(&context.execution_location.workspace, &args.path).await?;
            let capture = context.capture_path("/result/paths").await?;
            let workspace = context.execution_location.workspace.clone();
            tokio::task::spawn_blocking(move || {
                glob_blocking(
                    &workspace,
                    &root,
                    &args,
                    &capture,
                    &context.cancellation_token(),
                )
            })
            .await
            .map_err(|error| ToolError::Failed(error.to_string()))?
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
    capture: &Path,
    cancellation: &crate::job::CancellationToken,
) -> Result<SearchOutput, ToolError> {
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
            .map_err(|error| ToolError::Failed(error.to_string()))?;
        if sink.binary {
            matches.rollback(checkpoint)?;
        }
    }
    matches.finish()?;
    Ok(SearchOutput {
        matches: if args.details {
            SearchMatches::Detailed(Vec::new())
        } else {
            SearchMatches::Grouped(BTreeMap::new())
        },
    })
}

fn glob_blocking(
    workspace: &Path,
    root: &Path,
    args: &GlobArgs,
    capture: &Path,
    cancellation: &crate::job::CancellationToken,
) -> Result<GlobOutput, ToolError> {
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
    paths.finish()?;
    Ok(GlobOutput { paths: Vec::new() })
}

/// Streams the complete result into the owning job's capture. The handler returns
/// only an empty placeholder; job collection hydrates the saved field.
struct CapturedOutput {
    file: std::io::BufWriter<std::fs::File>,
    count: usize,
    grouped: bool,
    group: Option<String>,
}
impl CapturedOutput {
    fn new(path: &Path, grouped: bool) -> std::io::Result<Self> {
        use std::io::Write as _;
        let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
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
        self.file.get_ref().set_len(checkpoint.1)?;
        self.file.seek(std::io::SeekFrom::Start(checkpoint.1))?;
        Ok(())
    }
    fn finish(mut self) -> std::io::Result<()> {
        use std::io::Write as _;
        if self.grouped {
            if self.group.is_some() {
                self.file.write_all(b"\n]")?;
            }
            self.file.write_all(b"\n}")?;
        } else {
            self.file.write_all(b"\n]")?;
        }
        self.file.flush()
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

struct SearchSink<'a> {
    matcher: &'a grep_regex::RegexMatcher,
    path: String,
    matches: &'a mut CapturedOutput,
    binary: bool,
    cancellation: &'a crate::job::CancellationToken,
}
impl Sink for SearchSink<'_> {
    type Error = std::io::Error;
    fn matched(
        &mut self,
        _searcher: &Searcher,
        matched: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        if self.cancellation.is_cancelled() {
            return Err(std::io::Error::other("search cancelled"));
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

    fn captured_search(
        workspace: &Path,
        root: &Path,
        args: &SearchArgs,
    ) -> Result<Vec<SearchMatch>, ToolError> {
        assert!(args.details);
        let capture = tempfile::NamedTempFile::new()?;
        search_blocking(
            workspace,
            root,
            args,
            capture.path(),
            &crate::job::CancellationToken::new(),
        )?;
        Ok(serde_json::from_reader(capture.reopen()?)?)
    }

    fn captured_glob(
        workspace: &Path,
        root: &Path,
        args: &GlobArgs,
    ) -> Result<GlobOutput, ToolError> {
        let capture = tempfile::NamedTempFile::new()?;
        glob_blocking(
            workspace,
            root,
            args,
            capture.path(),
            &crate::job::CancellationToken::new(),
        )?;
        Ok(GlobOutput {
            paths: serde_json::from_reader(capture.reopen()?)?,
        })
    }

    #[tokio::test]
    async fn grouped_search_is_captured_and_preserves_whitespace_and_binary_rollback() {
        use serde_json::json;
        let runtime = crate::tests::TestRuntime::new().await;
        let directory = runtime.root.path().join("files");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("a.txt"), "  needle  \nneedle again\n").unwrap();
        std::fs::write(directory.join("b.bin"), b"needle\n\0hidden").unwrap();
        std::fs::write(directory.join("z.txt"), "needle last\n").unwrap();
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder).unwrap();
        let executor = runtime.executor(builder);
        let captured = executor
            .execute(
                runtime.agent.clone(),
                "search",
                json!({"path":"files","pattern":"needle"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            captured.output.value,
            json!({"matches":{
                "files/a.txt":["1:   needle  ", "2: needle again"], "files/z.txt":["1: needle last"]
            }})
        );
        let empty = executor
            .execute(
                runtime.agent.clone(),
                "search",
                json!({"path":"files","pattern":"absent"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(empty.output.value, json!({"matches":{}}));
        let detailed = executor
            .execute(
                runtime.agent.clone(),
                "search",
                json!({"path":"files","pattern":"needle","details":true}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            detailed.output.value["matches"][0],
            json!({"path":"files/a.txt","line":1,"column":3,"text":"  needle  "})
        );
        std::fs::write(directory.join("a.txt"), "needle λ\n".repeat(500)).unwrap();
        let preview = executor
            .execute_model(
                runtime.agent.clone(),
                "search",
                json!({"path":"files","pattern":"needle"}),
                None,
            )
            .await
            .unwrap();
        assert!(
            serde_json::to_vec(&preview.output.value["result"]["matches"])
                .unwrap()
                .len()
                <= 2048
        );
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
        assert_eq!(page["preview"]["lines"].as_array().unwrap().len(), 1);
        assert!(
            page["preview"]["lines"][0]
                .as_str()
                .unwrap()
                .contains("500: needle λ")
        );
    }

    #[test]
    fn wildcard_filters_respect_hidden_and_ignore_flags() {
        let root = tempfile::tempdir().unwrap();
        for directory in [".git", ".hidden", "build", "src"] {
            std::fs::create_dir(root.path().join(directory)).unwrap();
        }
        std::fs::write(root.path().join(".gitignore"), "build/\n").unwrap();
        for path in [
            ".git/config",
            ".hidden/secret",
            "build/generated",
            "src/visible",
        ] {
            std::fs::write(root.path().join(path), "needle\n").unwrap();
        }
        for hidden in [false, true] {
            for no_ignore in [false, true] {
                for pattern in ["*", "**/*"] {
                    let args = GlobArgs {
                        pattern: pattern.into(),
                        path: ".".into(),
                        hidden,
                        no_ignore,
                    };
                    let paths = captured_glob(root.path(), root.path(), &args)
                        .unwrap()
                        .paths;
                    assert!(paths.iter().any(|p| p == "src/visible"));
                    assert_eq!(paths.iter().any(|p| p == ".git/config"), hidden);
                    assert_eq!(paths.iter().any(|p| p == ".hidden/secret"), hidden);
                    assert_eq!(paths.iter().any(|p| p == "build/generated"), no_ignore);
                    let args: SearchArgs = serde_json::from_value(serde_json::json!({
                        "pattern":"needle", "details":true, "glob":[pattern], "hidden":hidden, "no_ignore":no_ignore
                    }))
                    .unwrap();
                    let matches = captured_search(root.path(), root.path(), &args).unwrap();
                    assert_eq!(matches.iter().any(|m| m.path == ".git/config"), hidden);
                    assert_eq!(
                        matches.iter().any(|m| m.path == "build/generated"),
                        no_ignore
                    );
                    assert!(matches.iter().any(|m| m.path == "src/visible"));
                }
            }
        }
        // An explicitly requested hidden root remains accessible.
        let args: GlobArgs =
            serde_json::from_value(serde_json::json!({"pattern":"*", "path":".git"})).unwrap();
        assert_eq!(
            captured_glob(root.path(), &root.path().join(".git"), &args)
                .unwrap()
                .paths,
            [".git/config"]
        );
    }

    #[test]
    fn subdirectory_roots_inherit_ignores_and_globs_keep_precedence() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::create_dir(root.path().join("src/nested")).unwrap();
        std::fs::write(root.path().join("src/nested/file.rs"), "needle\n").unwrap();
        std::fs::write(root.path().join(".gitignore"), "/src/generated.rs\n").unwrap();
        std::fs::write(root.path().join(".ignore"), "skip.rs\n").unwrap();
        for name in ["a.rs", "b.rs", "generated.rs", "skip.rs"] {
            std::fs::write(root.path().join("src").join(name), "needle\n").unwrap();
        }
        let args: GlobArgs =
            serde_json::from_value(serde_json::json!({"pattern":"*.rs", "path":"src"})).unwrap();
        assert_eq!(
            captured_glob(root.path(), &root.path().join("src"), &args)
                .unwrap()
                .paths,
            ["src/a.rs", "src/b.rs", "src/nested/file.rs"]
        );
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
            let args: SearchArgs = serde_json::from_value(
                serde_json::json!({"pattern":"needle", "details":true, "path":"src", "glob":glob}),
            )
            .unwrap();
            let matches = captured_search(root.path(), &root.path().join("src"), &args).unwrap();
            assert_eq!(
                matches.iter().map(|m| m.path.as_str()).collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn ripgrep_search_supports_smart_case_globs_and_binary_detection() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("upper.rs"), "Needle\n").unwrap();
        std::fs::write(root.path().join("lower.rs"), "needle\n").unwrap();
        std::fs::write(root.path().join("skip.txt"), "Needle\n").unwrap();
        std::fs::write(root.path().join("binary.rs"), b"Needle\0hidden\n").unwrap();
        let output = captured_search(
            root.path(),
            root.path(),
            &SearchArgs {
                pattern: "Needle".to_owned(),
                details: true,
                path: ".".to_owned(),
                glob: vec!["*.rs".to_owned(), "!binary.rs".to_owned()],
                fixed: false,
                case: Case::Smart,
                word: false,
                hidden: false,
                no_ignore: false,
            },
        )
        .unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].path, "upper.rs");
        assert_eq!(output[0].line, 1);
        assert_eq!(output[0].column, 1);
    }

    #[test]
    fn glob_uses_relative_sorted_paths() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("b.rs"), "").unwrap();
        std::fs::write(root.path().join("a.rs"), "").unwrap();
        let output = captured_glob(
            root.path(),
            root.path(),
            &GlobArgs {
                pattern: "*.rs".to_owned(),
                path: ".".to_owned(),
                hidden: false,
                no_ignore: false,
            },
        )
        .unwrap();
        assert_eq!(output.paths, vec!["a.rs", "b.rs"]);
    }
}
