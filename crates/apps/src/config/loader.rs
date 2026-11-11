//! Layer selection, schema-directed merging, and side-effect-free admission.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use skyhook::config::{Refused, user_config_directories, workspace_directory};
use skyhook_base::decode::{Decode, Diagnostics};
use skyhook_tools::diagnostic::escape_controls;

use super::layers::{Merged, read_layer};

/// A typed schema loaded by [`resolve_layers`]. The loader merges only the values files
/// write, never decoding defaults, then decodes this type and checks it.
///
/// Each user candidate is decoded and checked before it is selected. Workspace layers
/// may be partial: they are decoded only as part of the effective document.
/// Implementations must not read credentials, run commands, or otherwise perform
/// external work while decoding or checking.
pub trait LayeredConfig: Decode {
    /// What the host knows that checking the configuration needs.
    type Context<'a>: Copy;

    /// File name within each user configuration directory and `.skyhook`.
    const FILE_NAME: &'static str;

    /// Top-level maps merged by name. A later entry replaces the same-named entry
    /// whole; unrelated names and their declaration order are retained. All other
    /// top-level values, including maps, are replaced whole.
    const NAMED_ENTRIES: &'static [&'static str] = &[];

    /// Whether no discovered file configures the schema's defaults. Otherwise a file is
    /// required, and an empty effective document or user candidate configures nothing.
    /// This never hides rejected candidates or a missing explicit file.
    const OPTIONAL: bool = false;

    /// What the configuration's parts refuse of each other, given `context`. A user
    /// candidate is not `complete`: a workspace layer may define what it names.
    fn check(&self, _context: Self::Context<'_>, _complete: bool) -> Vec<Refused> {
        Vec::new()
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum LayerError {
    #[error("file not found")]
    Missing,
    #[error("could not read configuration: {0}")]
    Read(std::io::Error),
    #[error(transparent)]
    Yaml(skyhook::yaml::YamlError),
    #[error("the file contains no settings")]
    Blank,
    #[error("configuration must be a mapping of settings, not {0}")]
    NotMapping(&'static str),
    #[error("invalid configuration: configuration is empty")]
    Empty,
    #[error("{0}")]
    Invalid(Diagnostics),
}

impl LayerError {
    /// The error as diagnostics: one per refused value, at the file that wrote it, else
    /// one of `path`.
    fn diagnostics(self, path: &Path) -> Vec<ConfigDiagnostic> {
        match self {
            Self::Invalid(diagnostics) => (diagnostics.0.into_iter())
                .map(|diagnostic| {
                    let file = diagnostic.at.file.as_deref().unwrap_or(path);
                    ConfigDiagnostic {
                        path: file.to_path_buf(),
                        position: Some((diagnostic.at.line, diagnostic.at.column)),
                        message: diagnostic.located(),
                    }
                })
                .collect(),
            error => vec![diagnostic(path, error.to_string())],
        }
    }
}

/// Where resolution stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolutionStage {
    Explicit,
    Workspace,
    WorkspaceLayer,
    NoUsableLayer,
    Effective,
}

impl ResolutionStage {
    /// What failed, as the line above the report's diagnostics.
    fn headline(self, file_name: &str, report: &ConfigReport) -> String {
        match self {
            Self::Explicit => "could not load the selected configuration file".into(),
            Self::Workspace => "could not resolve the workspace".into(),
            Self::WorkspaceLayer => format!("could not load the workspace {file_name}"),
            Self::NoUsableLayer => format!("no usable {file_name}; every file found was refused"),
            Self::Effective if report.sources.is_empty() => {
                format!("the default {file_name} is invalid")
            }
            Self::Effective => {
                let sources = report.sources.iter().map(|path| path.display().to_string());
                let sources = sources.collect::<Vec<_>>().join(", ");
                format!("the {file_name} merged from {sources} is invalid")
            }
        }
    }
}

/// A rejected candidate or fatal layer error. Never includes YAML source excerpts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigDiagnostic {
    pub path: PathBuf,
    /// The line and column of the refused value, when the refusal names one.
    pub position: Option<(u64, u64)>,
    pub message: String,
}

/// One line: controls in the path or message, newlines included, are escaped so that
/// only the report's own line breaks separate diagnostics.
impl fmt::Display for ConfigDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", escape_controls(self.path.display()))?;
        if let Some((line, column)) = self.position {
            write!(f, ":{line}:{column}")?;
        }
        write!(f, ": {}", escape_controls(&self.message))
    }
}

/// Selected layers in merge order, plus rejected candidate diagnostics.
#[derive(Clone, Debug, Default)]
pub struct ConfigReport {
    pub sources: Vec<PathBuf>,
    pub diagnostics: Vec<ConfigDiagnostic>,
}

impl fmt::Display for ConfigReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for diagnostic in &self.diagnostics {
            write!(f, "\n  {diagnostic}")?;
        }
        Ok(())
    }
}

/// A typed configuration and the selected and rejected files that produced it.
#[derive(Clone, Debug)]
pub struct ResolvedConfig<T> {
    pub config: T,
    pub report: ConfigReport,
}

/// Failure to discover or admit a schema's layered configuration.
#[derive(Debug, thiserror::Error)]
pub enum LayerResolutionError {
    #[error("{}{report}", .stage.headline(.file_name, .report))]
    Resolution {
        file_name: &'static str,
        stage: ResolutionStage,
        report: ConfigReport,
    },
    #[error(
        "no {file_name} found; pass an explicit file or create ~/.config/skyhook/{file_name}{report}"
    )]
    Missing {
        file_name: &'static str,
        report: ConfigReport,
    },
}

impl LayerResolutionError {
    /// Sources selected before failure and rejected-candidate diagnostics.
    pub fn report(&self) -> &ConfigReport {
        match self {
            Self::Resolution { report, .. } | Self::Missing { report, .. } => report,
        }
    }
}

/// Resolve a schema using Skyhook's shared configuration discovery rules.
///
/// An explicit file disables discovery, including workspace resolution. Otherwise
/// the first usable user file wins: `$XDG_CONFIG_HOME/skyhook/<FILE_NAME>`, then
/// `$HOME/.config/skyhook/<FILE_NAME>`. Missing or invalid candidates are reported,
/// and the workspace's `.skyhook/<FILE_NAME>` overlays the selected user file.
/// Invalid workspace files are fatal. A schema may explicitly allow defaults
/// when all discovered files are absent; explicit files are always required.
///
/// YAML must be a mapping, with string keys and no duplicates. Checking and
/// whole-entry map policy belong to `T`; no provider-specific work runs here.
pub async fn resolve_layers<T: LayeredConfig>(
    workspace: &Path,
    explicit: Option<&Path>,
    context: T::Context<'_>,
) -> Result<ResolvedConfig<T>, LayerResolutionError> {
    // Do not even inspect environment roots or workspace for explicit selection.
    let candidates = if explicit.is_some() {
        Vec::new()
    } else {
        user_config_directories()
            .into_iter()
            .map(|directory| directory.join(T::FILE_NAME))
            .collect()
    };
    resolve_paths(workspace, explicit, candidates, context).await
}

pub(super) async fn resolve_paths<T: LayeredConfig>(
    workspace: &Path,
    explicit: Option<&Path>,
    candidates: Vec<PathBuf>,
    context: T::Context<'_>,
) -> Result<ResolvedConfig<T>, LayerResolutionError> {
    let mut report = ConfigReport::default();
    let mut merged = Merged::default();
    if let Some(path) = explicit {
        let explicit = |report: &mut ConfigReport, error: LayerError| {
            failure::<T>(report, ResolutionStage::Explicit, error.diagnostics(path))
        };
        let layer = read_layer(path)
            .await
            .map_err(|error| explicit(&mut report, error))?;
        merged.overlay::<T>(layer);
        let config = merged
            .decode::<T>(context, true)
            .map_err(|error| explicit(&mut report, error))?;
        report.sources.push(path.to_path_buf());
        return Ok(ResolvedConfig { config, report });
    }

    let mut seen = Vec::new();
    for path in candidates {
        // Relative/absolute identical candidates must not duplicate diagnostics.
        let identity = std::path::absolute(&path).unwrap_or_else(|_| path.clone());
        if seen.contains(&identity) {
            continue;
        }
        seen.push(identity);
        let loaded = match read_layer(&path).await {
            Ok(layer) => {
                let mut candidate = Merged::default();
                candidate.overlay::<T>(layer);
                candidate
                    .decode::<T>(context, false)
                    .map(|_| candidate)
                    .map_err(|error| error.diagnostics(&path))
            }
            // An absent candidate is the ordinary case, not a diagnostic.
            Err(LayerError::Missing) => continue,
            Err(error) => Err(error.diagnostics(&path)),
        };
        match loaded {
            Ok(candidate) => {
                merged = candidate;
                report.sources.push(path);
                break;
            }
            Err(diagnostics) => report.diagnostics.extend(diagnostics),
        }
    }

    let workspace = tokio::fs::canonicalize(workspace).await.map_err(|error| {
        let diagnostics = vec![diagnostic(workspace, error.to_string())];
        failure::<T>(&mut report, ResolutionStage::Workspace, diagnostics)
    })?;
    let workspace_path = workspace_directory(&workspace).join(T::FILE_NAME);
    match read_layer(&workspace_path).await {
        Ok(layer) => {
            merged.overlay::<T>(layer);
            report.sources.push(workspace_path.clone());
        }
        Err(LayerError::Missing) => {}
        Err(error) => {
            let diagnostics = error.diagnostics(&workspace_path);
            return Err(failure::<T>(
                &mut report,
                ResolutionStage::WorkspaceLayer,
                diagnostics,
            ));
        }
    }
    if report.sources.is_empty() {
        if !report.diagnostics.is_empty() {
            return Err(LayerResolutionError::Resolution {
                file_name: T::FILE_NAME,
                stage: ResolutionStage::NoUsableLayer,
                report,
            });
        }
        if !T::OPTIONAL {
            return Err(LayerResolutionError::Missing {
                file_name: T::FILE_NAME,
                report,
            });
        }
    }
    let config = merged.decode::<T>(context, true).map_err(|error| {
        let path = report.sources.last().unwrap_or(&workspace_path);
        let diagnostics = error.diagnostics(path);
        failure::<T>(&mut report, ResolutionStage::Effective, diagnostics)
    })?;
    Ok(ResolvedConfig { config, report })
}

fn failure<T: LayeredConfig>(
    report: &mut ConfigReport,
    stage: ResolutionStage,
    diagnostics: Vec<ConfigDiagnostic>,
) -> LayerResolutionError {
    report.diagnostics.extend(diagnostics);
    LayerResolutionError::Resolution {
        file_name: T::FILE_NAME,
        stage,
        report: report.clone(),
    }
}

fn diagnostic(path: &Path, message: impl Into<String>) -> ConfigDiagnostic {
    ConfigDiagnostic {
        path: path.to_path_buf(),
        position: None,
        message: message.into(),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use indexmap::IndexMap;
    use skyhook::launch::config::HostConfig;
    use skyhook_base::decode::{Segment, Source};
    use std::assert_matches;

    #[derive(Debug, Decode)]
    #[serde(default)]
    struct ExampleConfig {
        models: IndexMap<String, Entry>,
        roles: IndexMap<String, Entry>,
        mcp: IndexMap<String, Entry>,
        providers: IndexMap<String, Entry>,
        limit: usize,
    }

    impl Default for ExampleConfig {
        fn default() -> Self {
            Self {
                models: IndexMap::new(),
                roles: IndexMap::new(),
                mcp: IndexMap::new(),
                providers: IndexMap::new(),
                limit: 1,
            }
        }
    }

    #[derive(Debug, Decode)]
    struct Entry {
        value: usize,
        note: Option<String>,
        cwd: Option<PathBuf>,
        #[decode(source)]
        source: Source,
    }

    impl ExampleConfig {
        fn limited(&self) -> Vec<Refused> {
            match self.models.len() > self.limit {
                true => vec![Refused::new(
                    vec![Segment::Key("models".into())],
                    "model count exceeds limit",
                )],
                false => Vec::new(),
            }
        }
    }

    impl LayeredConfig for ExampleConfig {
        type Context<'a> = ();
        const FILE_NAME: &'static str = "example.yaml";
        const NAMED_ENTRIES: &'static [&'static str] = &["models", "roles", "mcp"];
        const OPTIONAL: bool = true;

        fn check(&self, (): (), _complete: bool) -> Vec<Refused> {
            self.limited()
        }
    }

    fn written(entry: &Entry) -> Option<&Path> {
        entry.source.file.as_deref()
    }

    /// A workspace with `C`'s user candidates (XDG, then home) and its workspace file,
    /// their directories created and the files left absent.
    pub(in crate::config) struct Fixture<C> {
        _root: tempfile::TempDir,
        pub(in crate::config) workspace: PathBuf,
        pub(in crate::config) xdg: PathBuf,
        pub(in crate::config) home: PathBuf,
        pub(in crate::config) local: PathBuf,
        config: std::marker::PhantomData<C>,
    }

    impl<C: LayeredConfig> Fixture<C> {
        pub(in crate::config) fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let workspace = root.path().join("project");
            let xdg = root.path().join("xdg/skyhook").join(C::FILE_NAME);
            let home = root.path().join("home/.config/skyhook").join(C::FILE_NAME);
            let local = workspace.join(".skyhook").join(C::FILE_NAME);
            for path in [&xdg, &home, &local] {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            }
            Self {
                _root: root,
                workspace,
                xdg,
                home,
                local,
                config: std::marker::PhantomData,
            }
        }

        pub(in crate::config) async fn resolve(
            &self,
            context: C::Context<'_>,
        ) -> Result<ResolvedConfig<C>, LayerResolutionError> {
            resolve_paths(
                &self.workspace,
                None,
                vec![self.xdg.clone(), self.home.clone()],
                context,
            )
            .await
        }
    }

    pub(in crate::config) fn write(path: &Path, contents: impl AsRef<[u8]>) {
        std::fs::write(path, contents).unwrap();
    }

    #[tokio::test]
    async fn schema_controls_whole_entry_maps_admission_and_provenance() {
        let f = Fixture::<ExampleConfig>::new();
        write(
            &f.xdg,
            r#"
limit: 4
models:
  retained: {value: 1}
  changed: {value: 2, note: user}
roles:
  retained: {value: 3}
  changed: {value: 4, note: user}
mcp:
  retained: {value: 5, cwd: relative-user}
  changed: {value: 6, note: user}
providers:
  discarded: {value: 7}
"#,
        );
        std::fs::create_dir(&f.home).unwrap(); // A successful XDG candidate prevents this read error.
        write(
            &f.local,
            r#"
models:
  changed: {value: 20}
  added: {value: 21}
roles:
  changed: {value: 40}
mcp:
  changed: {value: 60, cwd: relative-workspace}
providers:
  replacement: {value: 70}
"#,
        );
        let resolved = f.resolve(()).await.unwrap();
        assert_eq!(resolved.report.sources, [f.xdg.clone(), f.local.clone()]);
        assert!(resolved.report.diagnostics.is_empty());
        let config = resolved.config;
        // Neither candidate defaults nor the partial workspace's defaults enter the merge.
        assert_eq!(config.limit, 4);
        assert_eq!(
            config.models.keys().map(String::as_str).collect::<Vec<_>>(),
            ["retained", "changed", "added"]
        );
        for (entries, expected) in [(&config.models, 20), (&config.roles, 40), (&config.mcp, 60)] {
            assert_eq!(entries["changed"].value, expected);
            assert!(entries["changed"].note.is_none());
            assert_eq!(written(&entries["changed"]), Some(f.local.as_path()));
            assert_eq!(written(&entries["retained"]), Some(f.xdg.as_path()));
        }
        assert_eq!(
            config
                .providers
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["replacement"]
        );
        assert_eq!(
            written(&config.providers["replacement"]),
            Some(f.local.as_path())
        );
        assert_eq!(
            config.mcp["retained"].cwd.as_deref(),
            Some(Path::new("relative-user"))
        );
        assert_eq!(
            config.mcp["changed"].cwd.as_deref(),
            Some(Path::new("relative-workspace"))
        );
    }

    /// Absent files yield the schema's admitted defaults, which an explicit file
    /// or a required schema never fall back to.
    #[tokio::test]
    async fn missing_files_use_admitted_defaults() {
        #[derive(Debug, Decode)]
        #[serde(default)]
        struct LimitedDefaults {
            models: IndexMap<String, usize>,
        }

        impl Default for LimitedDefaults {
            fn default() -> Self {
                Self {
                    models: [("model".into(), 1)].into(),
                }
            }
        }

        impl LayeredConfig for LimitedDefaults {
            type Context<'a> = ();
            const FILE_NAME: &'static str = "defaults.yaml";
            const OPTIONAL: bool = true;

            fn check(&self, (): (), _complete: bool) -> Vec<Refused> {
                let path = vec![Segment::Key("models".into())];
                vec![Refused::new(
                    path,
                    format!("{} models exceed the limit", self.models.len()),
                )]
            }
        }

        let f = Fixture::<ExampleConfig>::new();
        let resolved = f.resolve(()).await.unwrap();
        assert!(resolved.report.sources.is_empty());
        // Absent candidates are the ordinary case and raise no diagnostic.
        assert!(resolved.report.diagnostics.is_empty());
        assert_eq!(resolved.config.limit, 1);
        write(&f.local, "{}");
        assert_eq!(f.resolve(()).await.unwrap().config.limit, 1);

        let missing = resolve_paths::<HostConfig>(&f.workspace, None, Vec::new(), None)
            .await
            .unwrap_err();
        assert_matches!(
            missing,
            LayerResolutionError::Missing {
                file_name: "config.yaml",
                ..
            }
        );
        let error = resolve_paths::<LimitedDefaults>(&f.workspace, None, Vec::new(), ())
            .await
            .unwrap_err();
        assert_matches!(
            error,
            LayerResolutionError::Resolution {
                file_name: "defaults.yaml",
                stage: ResolutionStage::Effective,
                ..
            }
        );
        assert_eq!(
            error.report().diagnostics[0].path,
            f.local.with_file_name("defaults.yaml")
        );
        assert!(error.to_string().contains("1 models exceed the limit"));

        let explicit = f.home.with_file_name("override.yaml");
        let error = resolve_layers::<ExampleConfig>(&f.workspace, Some(&explicit), ())
            .await
            .unwrap_err();
        assert_matches!(
            error,
            LayerResolutionError::Resolution {
                stage: ResolutionStage::Explicit,
                ..
            }
        );
        assert_eq!(error.report().diagnostics[0].path, explicit);
    }

    /// A refusal reads as a headline over one line per diagnostic, each naming the file
    /// read; a refused root says what it was, and controls inside a diagnostic, its
    /// newlines among them, cannot start a line of their own.
    #[tokio::test]
    async fn refusals_render_one_line_per_diagnostic() {
        let f = Fixture::<ExampleConfig>::new();
        for (text, message) in [
            ("", "the file contains no settings"),
            ("# only a comment\n", "the file contains no settings"),
            (
                "[a, b]",
                "configuration must be a mapping of settings, not a list",
            ),
            (
                "plain",
                "configuration must be a mapping of settings, not a string",
            ),
            (
                "\"bad\\nkey\\u001b\": 1",
                "unknown field `bad\\nkey\\u{1b}`",
            ),
        ] {
            write(&f.xdg, text);
            let error = resolve_layers::<ExampleConfig>(&f.workspace, Some(&f.xdg), ())
                .await
                .unwrap_err();
            let rendered = error.to_string();
            let lines = rendered.lines().collect::<Vec<_>>();
            assert_eq!(lines.len(), 2, "{rendered}");
            assert_eq!(lines[0], "could not load the selected configuration file");
            let diagnostic = lines[1].strip_prefix("  ").unwrap();
            let diagnostic = diagnostic.strip_prefix(f.xdg.to_str().unwrap()).unwrap();
            assert!(diagnostic.contains(message), "{text:?}: {diagnostic}");
        }
    }

    /// A refused value is reported at the line and column of the file that wrote it,
    /// a merged entry at its own file's, and the report quotes no text around it.
    #[tokio::test]
    async fn refusals_name_where_the_refused_value_is_written() {
        let f = Fixture::<ExampleConfig>::new();
        write(
            &f.xdg,
            "limit: 4\nmodels:\n  user: {value: 1, note: do-not-leak}\n",
        );
        let refused = async |text: &str| {
            write(&f.local, text);
            let error = f.resolve(()).await.unwrap_err();
            assert!(!error.to_string().contains("do-not-leak"), "{error}");
            let diagnostic = error.report().diagnostics.last().unwrap().clone();
            (diagnostic.path, diagnostic.position)
        };
        let nested = "roles:\n  r:\n    value: 1\n    cwd: [x]\n";
        assert_eq!(refused(nested).await, (f.local.clone(), Some((4, 10))));
        let entry = "models:\n  workspace: {value: one}\n";
        assert_eq!(refused(entry).await, (f.local.clone(), Some((2, 22))));
        let top = "# leading comment\nlimit: [1]\n";
        assert_eq!(refused(top).await, (f.local.clone(), Some((2, 8))));
        write(&f.xdg, "\n\nmodels:\n  user: {value: 1, notes: x}\n");
        let error = resolve_layers::<ExampleConfig>(&f.workspace, Some(&f.xdg), ())
            .await
            .unwrap_err();
        let diagnostic = &error.report().diagnostics[0];
        assert_eq!(diagnostic.position, Some((4, 20)));
        assert!(
            (diagnostic.to_string()).starts_with(&format!("{}:4:20: ", f.xdg.display())),
            "{diagnostic}"
        );
    }

    /// Discovery takes the first usable candidate: an absent one is silent, a
    /// refused one is reported once, and with none usable the workspace file alone
    /// configures.
    #[tokio::test]
    async fn candidates_fall_back_past_refusals_once_each() {
        let f = Fixture::<ExampleConfig>::new();
        write(&f.home, "models:\n  kept: {value: 1}\n");
        let candidates = vec![f.xdg.clone(), f.xdg.clone(), f.home.clone()];
        let resolve = || resolve_paths::<ExampleConfig>(&f.workspace, None, candidates.clone(), ());
        let refusals: [Option<&[u8]>; 8] = [
            None,
            Some(b"DIRECTORY"), // read failure, works even when tests run as root
            Some(b"\xff\xfe"),  // invalid UTF-8
            Some(b""),          // not a mapping
            Some(b"malformed: ["), // YAML
            Some(b"limit: bad"), // type
            Some(b"unrecognized: 1"), // unknown field
            Some(b"limit: 0\nmodels:\n  a: {value: 1}"), // check
        ];
        for refused in refusals {
            if f.xdg.is_dir() {
                std::fs::remove_dir(&f.xdg).unwrap();
            }
            match refused {
                Some(b"DIRECTORY") => std::fs::create_dir(&f.xdg).unwrap(),
                Some(bytes) => write(&f.xdg, bytes),
                None => {}
            }
            let resolved = resolve()
                .await
                .unwrap_or_else(|error| panic!("{refused:?}: {error}"));
            assert_eq!(resolved.report.sources, std::slice::from_ref(&f.home));
            let diagnostics = &resolved.report.diagnostics;
            assert_eq!(
                diagnostics.len(),
                usize::from(refused.is_some()),
                "{refused:?}"
            );
            assert!(
                diagnostics
                    .iter()
                    .all(|diagnostic| diagnostic.path == f.xdg)
            );
            let config = resolved.config;
            assert_eq!(config.models.keys().collect::<Vec<_>>(), ["kept"]);
        }
        std::fs::remove_file(&f.home).unwrap();
        write(&f.local, "models:\n  local: {value: 2}\n");
        for (candidates, diagnostics) in [(candidates.clone(), 1), (Vec::new(), 0)] {
            let resolved = resolve_paths::<ExampleConfig>(&f.workspace, None, candidates, ())
                .await
                .unwrap();
            assert_eq!(resolved.report.sources, std::slice::from_ref(&f.local));
            assert_eq!(resolved.report.diagnostics.len(), diagnostics);
            assert_eq!(resolved.config.models["local"].value, 2);
        }
    }
}
