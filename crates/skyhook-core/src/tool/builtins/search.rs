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
    policy::{PathAccess, ToolEffect},
};

const MAX_RESULTS: usize = 1_000;
const MAX_LINE_BYTES: usize = 32 * 1024;
const MAX_OUTPUT_BYTES: usize = 512 * 1024;

pub(super) fn register(builder: &mut ToolRegistryBuilder) -> Result<(), RegistryError> {
    builder.register::<SearchArgs, SearchOutput, _, _>(
        "search",
        "Search files with ripgrep regex, glob, and ignore semantics.",
        ToolOptions::new(vec![ToolEffect::ReadWorkspace])
            .workspace_bound()
            .path_argument("path", PathAccess::Read, PathKind::Existing),
        |context, args| async move {
            validate_limit(args.limit)?;
            let root = resolve_existing(&context.workspace, &args.path).await?;
            let workspace = context.workspace;
            tokio::task::spawn_blocking(move || search_blocking(&workspace, &root, &args))
                .await
                .map_err(|error| ToolError::Failed(error.to_string()))?
        },
    )?;
    builder.register::<GlobArgs, GlobOutput, _, _>(
        "glob",
        "Find files with ripgrep glob and ignore semantics.",
        ToolOptions::new(vec![ToolEffect::ReadWorkspace])
            .workspace_bound()
            .path_argument("path", PathAccess::Read, PathKind::Existing),
        |context, args| async move {
            validate_limit(args.limit)?;
            let root = resolve_existing(&context.workspace, &args.path).await?;
            let workspace = context.workspace;
            tokio::task::spawn_blocking(move || glob_blocking(&workspace, &root, &args))
                .await
                .map_err(|error| ToolError::Failed(error.to_string()))?
        },
    )?;
    Ok(())
}

fn validate_limit(limit: usize) -> Result<(), ToolError> {
    if (1..=MAX_RESULTS).contains(&limit) {
        Ok(())
    } else {
        Err(ToolError::InvalidArguments(format!(
            "limit must be 1 through {MAX_RESULTS}"
        )))
    }
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
) -> Result<SearchOutput, ToolError> {
    let mut matcher_builder = RegexMatcherBuilder::new();
    matcher_builder
        .fixed_strings(args.fixed)
        .word(args.word)
        .line_terminator(Some(b'\n'))
        .ban_byte(Some(b'\0'))
        .size_limit(10 * 1024 * 1024)
        .dfa_size_limit(10 * 1024 * 1024);
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

    let mut files = Vec::<PathBuf>::new();
    if root.is_file() {
        files.push(root.to_owned());
    } else if root.is_dir() {
        let mut walk = walk_builder(root, args.hidden, args.no_ignore);
        if !args.glob.is_empty() {
            walk.overrides(overrides(root, &args.glob)?);
        }
        for entry in walk.build() {
            let entry = entry.map_err(|error| ToolError::Failed(error.to_string()))?;
            if entry.file_type().is_some_and(|kind| kind.is_file()) {
                files.push(entry.into_path());
            }
        }
    } else {
        return Err(ToolError::Failed(
            "search root is not a file or directory".to_owned(),
        ));
    }

    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(b'\0'))
        .heap_limit(Some(MAX_LINE_BYTES * 4))
        .build();
    let mut matches = Vec::new();
    let mut retained = 0_usize;
    let mut truncated = false;
    for path in files {
        if matches.len() > args.limit || truncated {
            break;
        }
        let relative = relative_path(workspace, &path)?;
        let mut found = Vec::new();
        let mut sink = SearchSink {
            matcher: &matcher,
            path: &relative,
            matches: &mut found,
            binary: false,
            limit: args.limit.saturating_add(1).saturating_sub(matches.len()),
        };
        searcher
            .search_path(&matcher, &path, &mut sink)
            .map_err(|error| ToolError::Failed(error.to_string()))?;
        if sink.binary {
            continue;
        }
        for item in found {
            let bytes = item
                .text
                .len()
                .saturating_add(item.path.len())
                .saturating_add(64);
            if retained.saturating_add(bytes) > MAX_OUTPUT_BYTES {
                truncated = true;
                break;
            }
            retained += bytes;
            matches.push(item);
            if matches.len() > args.limit {
                break;
            }
        }
    }
    truncated |= matches.len() > args.limit;
    matches.truncate(args.limit);
    Ok(SearchOutput { matches, truncated })
}

fn glob_blocking(workspace: &Path, root: &Path, args: &GlobArgs) -> Result<GlobOutput, ToolError> {
    if !root.is_dir() {
        return Err(ToolError::Failed("glob root is not a directory".to_owned()));
    }
    let mut walk = walk_builder(root, args.hidden, args.no_ignore);
    walk.overrides(overrides(root, std::slice::from_ref(&args.pattern))?);
    let mut paths = Vec::new();
    for entry in walk.build() {
        let entry = entry.map_err(|error| ToolError::Failed(error.to_string()))?;
        if entry.depth() == 0 || !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        paths.push(relative_path(workspace, entry.path())?);
        if paths.len() > args.limit {
            break;
        }
    }
    let truncated = paths.len() > args.limit;
    paths.truncate(args.limit);
    Ok(GlobOutput { paths, truncated })
}

struct SearchSink<'a> {
    matcher: &'a grep_regex::RegexMatcher,
    path: &'a str,
    matches: &'a mut Vec<SearchMatch>,
    binary: bool,
    limit: usize,
}

impl Sink for SearchSink<'_> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        matched: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        let bytes = matched.bytes();
        let first = self
            .matcher
            .find(bytes)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let Some(first) = first else {
            return Ok(true);
        };
        let mut line = bytes;
        if line.ends_with(b"\n") {
            line = &line[..line.len() - 1];
            if line.ends_with(b"\r") {
                line = &line[..line.len() - 1];
            }
        }
        let line_truncated = line.len() > MAX_LINE_BYTES;
        if line_truncated {
            line = &line[..MAX_LINE_BYTES];
        }
        self.matches.push(SearchMatch {
            path: self.path.to_owned(),
            line: matched.line_number().unwrap_or(1),
            column: first.start().saturating_add(1),
            text: String::from_utf8_lossy(line).into_owned(),
            truncated: line_truncated,
        });
        Ok(self.matches.len() < self.limit)
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
    /// File or directory to search, relative to the selected workspace by default.
    #[serde(default = "default_dot")]
    path: String,
    /// Ripgrep-style include/exclude globs. Prefix exclusions with `!`; later globs override earlier ones.
    #[serde(default)]
    glob: Vec<String>,
    /// Treat `pattern` as literal text instead of a regular expression (`rg -F`).
    #[serde(default)]
    fixed: bool,
    /// Case mode: `smart`, `sensitive`, or `insensitive`.
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
    /// Maximum number of matches to return.
    #[serde(default = "default_search_limit")]
    #[schemars(range(min = 1, max = 1000))]
    limit: usize,
}

#[derive(Serialize, JsonSchema)]
struct SearchOutput {
    matches: Vec<SearchMatch>,
    truncated: bool,
}

#[derive(Serialize, JsonSchema)]
struct SearchMatch {
    path: String,
    line: u64,
    column: usize,
    text: String,
    truncated: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GlobArgs {
    /// Ripgrep-style file glob, such as `src/**/*.rs`.
    pattern: String,
    /// Directory to enumerate, relative to the selected workspace by default.
    #[serde(default = "default_dot")]
    path: String,
    /// Include hidden files and directories.
    #[serde(default)]
    hidden: bool,
    /// Ignore .gitignore and related ignore files.
    #[serde(default)]
    no_ignore: bool,
    /// Maximum number of paths to return.
    #[serde(default = "default_glob_limit")]
    #[schemars(range(min = 1, max = 1000))]
    limit: usize,
}

#[derive(Serialize, JsonSchema)]
struct GlobOutput {
    paths: Vec<String>,
    truncated: bool,
}

fn default_dot() -> String {
    ".".to_owned()
}
const fn default_search_limit() -> usize {
    100
}
const fn default_glob_limit() -> usize {
    200
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
                limit: 100,
            },
        )
        .unwrap();
        assert_eq!(output.matches.len(), 1);
        assert_eq!(output.matches[0].path, "upper.rs");
        assert_eq!(output.matches[0].line, 1);
        assert_eq!(output.matches[0].column, 1);
        assert!(!output.truncated);
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
                limit: 1,
            },
        )
        .unwrap();
        assert_eq!(output.paths, vec!["a.rs"]);
        assert!(output.truncated);
    }
}
