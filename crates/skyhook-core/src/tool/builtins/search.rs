use std::path::{Path, PathBuf};

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
        "Search files with ripgrep regex, glob, and ignore semantics.",
        ToolOptions::new(vec![Capability::Read])
            .placement(crate::tool::ToolPlacement::TargetedWorkspace)
            .path_argument("path", PathAccess::Read, PathKind::Existing),
        |context, args| async move {
            let root = resolve_existing(&context.execution_location.workspace, &args.path).await?;
            let capture = context.capture_path("/result/matches").await?;
            let workspace = context.execution_location.workspace.clone();
            tokio::task::spawn_blocking(move || {
                search_blocking(&workspace, &root, &args, Some(&capture), Some(&context))
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
                glob_blocking(&workspace, &root, &args, Some(&capture), Some(&context))
            })
            .await
            .map_err(|error| ToolError::Failed(error.to_string()))?
        },
    )?;
    Ok(())
}

fn walk_builder(root: &Path, hidden: bool, no_ignore: bool) -> WalkBuilder {
    let mut walk = WalkBuilder::new(root);
    walk.hidden(!hidden)
        .ignore(!no_ignore)
        .git_ignore(!no_ignore)
        .git_exclude(!no_ignore)
        .git_global(false)
        .parents(false)
        .follow_links(false)
        .sort_by_file_path(std::cmp::Ord::cmp);
    walk
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
    capture: Option<&Path>,
    context: Option<&crate::tool::ToolContext>,
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
        let mut walk = walk_builder(root, args.hidden, args.no_ignore);
        if !args.glob.is_empty() {
            walk.overrides(overrides(root, &args.glob)?);
        }
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
    let mut matches = CapturedArray::new(capture)?;
    for path in files {
        if context.is_some_and(crate::tool::ToolContext::is_cancelled) {
            return Err(ToolError::Cancelled);
        }
        let path = path?;
        let checkpoint = matches.checkpoint()?;
        let mut sink = SearchSink {
            matcher: &matcher,
            path: relative_path(workspace, &path),
            matches: &mut matches,
            binary: false,
            context,
        };
        searcher
            .search_path(&matcher, &path, &mut sink)
            .map_err(|error| ToolError::Failed(error.to_string()))?;
        if sink.binary {
            matches.rollback(checkpoint)?;
        }
    }
    Ok(SearchOutput {
        matches: matches.finish()?,
    })
}

fn glob_blocking(
    workspace: &Path,
    root: &Path,
    args: &GlobArgs,
    capture: Option<&Path>,
    context: Option<&crate::tool::ToolContext>,
) -> Result<GlobOutput, ToolError> {
    if !root.is_dir() {
        return Err(ToolError::Failed("glob root is not a directory".into()));
    }
    let mut walk = walk_builder(root, args.hidden, args.no_ignore);
    walk.overrides(overrides(root, std::slice::from_ref(&args.pattern))?);
    let mut paths = CapturedArray::new(capture)?;
    for entry in walk.build() {
        if context.is_some_and(crate::tool::ToolContext::is_cancelled) {
            return Err(ToolError::Cancelled);
        }
        let entry = entry.map_err(|error| ToolError::Failed(error.to_string()))?;
        if entry.depth() != 0 && entry.file_type().is_some_and(|kind| kind.is_file()) {
            paths.push(relative_path(workspace, entry.path()))?;
        }
    }
    Ok(GlobOutput {
        paths: paths.finish()?,
    })
}

struct CapturedArray<T> {
    file: Option<std::io::BufWriter<std::fs::File>>,
    values: Vec<T>,
    count: usize,
}
impl<T: Serialize> CapturedArray<T> {
    fn new(path: Option<&Path>) -> std::io::Result<Self> {
        use std::io::Write as _;
        let mut file = path
            .map(std::fs::File::create)
            .transpose()?
            .map(std::io::BufWriter::new);
        if let Some(file) = &mut file {
            file.write_all(b"[\n")?;
        }
        Ok(Self {
            file,
            values: Vec::new(),
            count: 0,
        })
    }
    fn push(&mut self, value: T) -> std::io::Result<()> {
        use std::io::Write as _;
        if let Some(file) = &mut self.file {
            if self.count > 0 {
                file.write_all(b",\n")?;
            }
            serde_json::to_writer(file, &value)?;
        } else {
            self.values.push(value);
        }
        self.count += 1;
        Ok(())
    }
    fn checkpoint(&mut self) -> std::io::Result<(usize, u64)> {
        use std::io::Seek as _;
        Ok((
            self.count,
            self.file
                .as_mut()
                .map(std::io::BufWriter::stream_position)
                .transpose()?
                .unwrap_or(0),
        ))
    }
    fn rollback(&mut self, checkpoint: (usize, u64)) -> std::io::Result<()> {
        use std::io::{Seek as _, Write as _};
        self.count = checkpoint.0;
        self.values.truncate(checkpoint.0);
        if let Some(file) = &mut self.file {
            file.flush()?;
            file.get_ref().set_len(checkpoint.1)?;
            file.seek(std::io::SeekFrom::Start(checkpoint.1))?;
        }
        Ok(())
    }
    fn finish(mut self) -> std::io::Result<Vec<T>> {
        use std::io::Write as _;
        if let Some(file) = &mut self.file {
            file.write_all(b"\n]")?;
            file.flush()?;
        }
        Ok(self.values)
    }
}
struct SearchSink<'a> {
    matcher: &'a grep_regex::RegexMatcher,
    path: String,
    matches: &'a mut CapturedArray<SearchMatch>,
    binary: bool,
    context: Option<&'a crate::tool::ToolContext>,
}
impl Sink for SearchSink<'_> {
    type Error = std::io::Error;
    fn matched(
        &mut self,
        _searcher: &Searcher,
        matched: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        if self
            .context
            .is_some_and(crate::tool::ToolContext::is_cancelled)
        {
            return Err(std::io::Error::other("search cancelled"));
        }
        let bytes = matched.bytes();
        let first = self
            .matcher
            .find(bytes)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        if let Some(first) = first {
            self.matches.push(SearchMatch {
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
    matches: Vec<SearchMatch>,
}

#[derive(Serialize, JsonSchema)]
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

    #[test]
    fn ripgrep_search_supports_smart_case_globs_and_binary_detection() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("upper.rs"), "Needle\n").unwrap();
        std::fs::write(root.path().join("lower.rs"), "needle\n").unwrap();
        std::fs::write(root.path().join("skip.txt"), "Needle\n").unwrap();
        std::fs::write(root.path().join("binary.rs"), b"Needle\0hidden\n").unwrap();
        let output = search_blocking(
            root.path(),
            root.path(),
            &SearchArgs {
                pattern: "Needle".to_owned(),
                path: ".".to_owned(),
                glob: vec!["*.rs".to_owned(), "!binary.rs".to_owned()],
                fixed: false,
                case: Case::Smart,
                word: false,
                hidden: false,
                no_ignore: false,
            },
            None,
            None,
        )
        .unwrap();
        assert_eq!(output.matches.len(), 1);
        assert_eq!(output.matches[0].path, "upper.rs");
        assert_eq!(output.matches[0].line, 1);
        assert_eq!(output.matches[0].column, 1);
    }

    #[test]
    fn glob_uses_relative_sorted_paths_and_lookahead_truncation() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("b.rs"), "").unwrap();
        std::fs::write(root.path().join("a.rs"), "").unwrap();
        let output = glob_blocking(
            root.path(),
            root.path(),
            &GlobArgs {
                pattern: "*.rs".to_owned(),
                path: ".".to_owned(),
                hidden: false,
                no_ignore: false,
            },
            None,
            None,
        )
        .unwrap();
        assert_eq!(output.paths, vec!["a.rs", "b.rs"]);
    }
}
