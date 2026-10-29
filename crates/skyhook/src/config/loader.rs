//! Layer selection, schema-directed merging, and side-effect-free admission.

use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
};

use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use tokio::fs;

use super::Config;

/// A typed schema loaded by [`resolve_layers`]. The loader merges only source
/// values, not deserialization defaults, then extracts and admits this type.
///
/// Admission also runs on each user candidate before selection. Workspace layers
/// may be partial: they are admitted only as part of the effective document.
/// Implementations must not read credentials, run commands, or otherwise perform
/// external work during admission.
pub trait LayeredConfig: DeserializeOwned {
    type Error: std::error::Error + Send + Sync + 'static;

    /// File name within each user configuration directory and `.skyhook`.
    const FILE_NAME: &'static str;

    /// Top-level maps merged by name. A later entry replaces the same-named entry
    /// whole; unrelated names and their declaration order are retained. All other
    /// top-level values, including maps, are replaced whole.
    const NAMED_ENTRIES: &'static [&'static str] = &[];

    /// Policy when no discovered file exists. `None` requires a file, and refuses
    /// an empty effective document or user candidate as configuring nothing; `Some`
    /// supplies defaults, which are admitted like a loaded configuration. This
    /// never hides rejected candidates or a missing explicit file.
    fn missing() -> Option<Self> {
        None
    }

    /// Validate the typed configuration and resolve any source-relative values.
    /// Changes made here never become input to merging another layer.
    fn admit_layer(&mut self, _sources: &LayerSources) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Absolute source files for the named entries of an effective document.
/// Only maps declared in [`LayeredConfig::NAMED_ENTRIES`] have entry provenance.
#[derive(Clone, Debug, Default)]
pub struct LayerSources(HashMap<String, HashMap<String, PathBuf>>);

impl LayerSources {
    /// The file supplying this whole entry, or `None` for an entry supplied by
    /// schema defaults rather than a file. Paths are absolute, not canonicalized.
    pub fn entry(&self, field: &str, name: &str) -> Option<&Path> {
        self.0.get(field)?.get(name).map(PathBuf::as_path)
    }
}

#[derive(Debug, thiserror::Error)]
enum LayerError {
    #[error("file not found")]
    Missing,
    #[error("could not read configuration: {0}")]
    Read(std::io::Error),
    #[error(transparent)]
    Yaml(crate::yaml::YamlError),
    #[error("configuration must be a mapping")]
    NotMapping,
    #[error("invalid configuration: configuration is empty")]
    Empty,
    #[error("invalid configuration: {}", super::located(.0))]
    Structure(serde_path_to_error::Error<serde_json::Error>),
    #[error(transparent)]
    Admission(Box<dyn std::error::Error + Send + Sync>),
}

/// Where resolution stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResolutionStage {
    #[error("explicit configuration failed")]
    Explicit,
    #[error("could not resolve workspace")]
    Workspace,
    #[error("workspace configuration failed")]
    WorkspaceLayer,
    #[error("no usable Skyhook config found")]
    NoUsableLayer,
    #[error("effective configuration failed")]
    Effective,
}

/// A rejected candidate or fatal layer error. Never includes YAML source excerpts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigDiagnostic {
    pub path: PathBuf,
    pub message: String,
}

impl fmt::Display for ConfigDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
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
pub struct ResolvedConfig<T = Config> {
    pub config: T,
    pub report: ConfigReport,
}

/// Failure to discover or admit a schema's layered configuration.
#[derive(Debug, thiserror::Error)]
pub enum LayerResolutionError {
    #[error("{file_name}: {stage}{report}")]
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
/// YAML must be a mapping, with string keys and no duplicates. Admission and
/// whole-entry map policy belong to `T`; no provider-specific work runs here.
pub async fn resolve_layers<T: LayeredConfig>(
    workspace: &Path,
    explicit: Option<&Path>,
) -> Result<ResolvedConfig<T>, LayerResolutionError> {
    // Do not even inspect environment roots or workspace for explicit selection.
    let candidates = if explicit.is_some() {
        Vec::new()
    } else {
        super::paths::user_config_directories()
            .into_iter()
            .map(|directory| directory.join(T::FILE_NAME))
            .collect()
    };
    resolve_paths(workspace, explicit, candidates).await
}

pub(super) async fn resolve_paths<T: LayeredConfig>(
    workspace: &Path,
    explicit: Option<&Path>,
    candidates: Vec<PathBuf>,
) -> Result<ResolvedConfig<T>, LayerResolutionError> {
    let mut report = ConfigReport::default();
    let mut merged = Merged::default();
    if let Some(path) = explicit {
        let explicit = |report: &mut ConfigReport, message: String| {
            failure::<T>(report, ResolutionStage::Explicit, diagnostic(path, message))
        };
        let layer = read_layer(path)
            .await
            .map_err(|error| explicit(&mut report, error.to_string()))?;
        merged.overlay::<T>(layer);
        let config = merged
            .deserialize::<T>()
            .map_err(|error| explicit(&mut report, error.to_string()))?;
        report.sources.push(path.to_path_buf());
        return Ok(ResolvedConfig { config, report });
    }

    let mut seen = Vec::new();
    // Whether a candidate exists but was refused, as opposed to none existing.
    let mut rejected = false;
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
                    .deserialize::<T>()
                    .map(|_| candidate)
                    .map_err(|error| diagnostic(&path, error.to_string()))
            }
            // An absent candidate is the ordinary case, not a diagnostic.
            Err(LayerError::Missing) => continue,
            Err(error) => Err(diagnostic(&path, error.to_string())),
        };
        match loaded {
            Ok(candidate) => {
                // Only actual source values participate in the layer merge.
                merged = candidate;
                report.sources.push(path);
                break;
            }
            Err(diagnostic) => {
                rejected = true;
                report.diagnostics.push(diagnostic);
            }
        }
    }

    let workspace = fs::canonicalize(workspace).await.map_err(|error| {
        let diagnostic = diagnostic(workspace, error.to_string());
        failure::<T>(&mut report, ResolutionStage::Workspace, diagnostic)
    })?;
    let workspace_path = super::paths::workspace_directory(&workspace).join(T::FILE_NAME);
    match read_layer(&workspace_path).await {
        Ok(layer) => {
            merged.overlay::<T>(layer);
            report.sources.push(workspace_path.clone());
        }
        Err(LayerError::Missing) => {}
        Err(error) => {
            let diagnostic = diagnostic(&workspace_path, error.to_string());
            return Err(failure::<T>(
                &mut report,
                ResolutionStage::WorkspaceLayer,
                diagnostic,
            ));
        }
    }
    let config = if report.sources.is_empty() {
        if rejected {
            return Err(LayerResolutionError::Resolution {
                file_name: T::FILE_NAME,
                stage: ResolutionStage::NoUsableLayer,
                report,
            });
        }
        let Some(mut config) = T::missing() else {
            return Err(LayerResolutionError::Missing {
                file_name: T::FILE_NAME,
                report,
            });
        };
        config
            .admit_layer(&merged.sources)
            .map(|()| config)
            .map_err(|error| LayerError::Admission(Box::new(error)))
    } else {
        merged.deserialize::<T>()
    }
    .map_err(|error| {
        let path = report.sources.last().unwrap_or(&workspace_path);
        let diagnostic = diagnostic(path, error.to_string());
        failure::<T>(&mut report, ResolutionStage::Effective, diagnostic)
    })?;
    Ok(ResolvedConfig { config, report })
}

fn failure<T: LayeredConfig>(
    report: &mut ConfigReport,
    stage: ResolutionStage,
    diagnostic: ConfigDiagnostic,
) -> LayerResolutionError {
    report.diagnostics.push(diagnostic);
    LayerResolutionError::Resolution {
        file_name: T::FILE_NAME,
        stage,
        report: report.clone(),
    }
}

fn diagnostic(path: &Path, message: impl Into<String>) -> ConfigDiagnostic {
    ConfigDiagnostic {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

async fn read_layer(path: &Path) -> Result<Layer, LayerError> {
    let text = fs::read_to_string(path)
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => LayerError::Missing,
            _ => LayerError::Read(error),
        })?;
    let Value::Object(value) = crate::yaml::parse(&text).map_err(LayerError::Yaml)? else {
        return Err(LayerError::NotMapping);
    };
    let file = std::path::absolute(path).map_err(LayerError::Read)?;
    Ok(Layer { value, file })
}

struct Layer {
    value: Map<String, Value>,
    file: PathBuf,
}

#[derive(Default)]
struct Merged {
    value: Map<String, Value>,
    sources: LayerSources,
}

impl Merged {
    fn overlay<T: LayeredConfig>(&mut self, layer: Layer) {
        for (key, value) in layer.value {
            let named = T::NAMED_ENTRIES.contains(&key.as_str());
            if named {
                match &value {
                    Value::Object(entries) => {
                        let sources = self.sources.0.entry(key.clone()).or_default();
                        for name in entries.keys() {
                            sources.insert(name.clone(), layer.file.clone());
                        }
                    }
                    _ => {
                        self.sources.0.remove(&key);
                    }
                }
            }
            match (self.value.get_mut(&key), value) {
                (Some(Value::Object(entries)), Value::Object(overlay)) if named => {
                    entries.extend(overlay);
                }
                (_, value) => {
                    self.value.insert(key, value);
                }
            }
        }
    }

    /// An empty document configures nothing, so a schema that requires a file
    /// refuses it as it would no file at all; one with defaults admits them.
    fn deserialize<T: LayeredConfig>(&self) -> Result<T, LayerError> {
        let mut config: T = match self.value.is_empty() {
            true => T::missing().ok_or(LayerError::Empty)?,
            false => {
                serde_path_to_error::deserialize(&self.value).map_err(LayerError::Structure)?
            }
        };
        config
            .admit_layer(&self.sources)
            .map_err(|error| LayerError::Admission(Box::new(error)))?;
        Ok(config)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use indexmap::IndexMap;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    #[serde(default, deny_unknown_fields)]
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

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Entry {
        value: usize,
        note: Option<String>,
        cwd: Option<PathBuf>,
        #[serde(skip)]
        source: Option<PathBuf>,
    }

    #[derive(Debug, thiserror::Error)]
    #[error("model count exceeds limit")]
    struct TooManyModels;

    impl LayeredConfig for ExampleConfig {
        type Error = TooManyModels;
        const FILE_NAME: &'static str = "example.yaml";
        const NAMED_ENTRIES: &'static [&'static str] = &["models", "roles", "mcp"];

        fn missing() -> Option<Self> {
            Some(Self::default())
        }

        fn admit_layer(&mut self, sources: &LayerSources) -> Result<(), Self::Error> {
            if self.models.len() > self.limit {
                return Err(TooManyModels);
            }
            for (field, entries) in [
                ("models", &mut self.models),
                ("roles", &mut self.roles),
                ("mcp", &mut self.mcp),
            ] {
                for (name, entry) in entries {
                    entry.source = sources.entry(field, name).map(Path::to_path_buf);
                }
            }
            Ok(())
        }
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
        ) -> Result<ResolvedConfig<C>, LayerResolutionError> {
            resolve_paths(
                &self.workspace,
                None,
                vec![self.xdg.clone(), self.home.clone()],
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
        let resolved = f.resolve().await.unwrap();
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
            assert_eq!(
                entries["changed"].source.as_deref(),
                Some(f.local.as_path())
            );
            assert_eq!(entries["retained"].source.as_deref(), Some(f.xdg.as_path()));
        }
        // These field names do not trigger main-Config processing in the generic loader.
        assert_eq!(
            config
                .providers
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["replacement"]
        );
        assert!(config.providers["replacement"].source.is_none());
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
        #[derive(Debug, Deserialize)]
        struct LimitedDefaults(ExampleConfig);

        impl LayeredConfig for LimitedDefaults {
            type Error = TooManyModels;
            const FILE_NAME: &'static str = "defaults.yaml";

            fn missing() -> Option<Self> {
                Some(Self(ExampleConfig {
                    models: [(
                        "model".into(),
                        Entry {
                            value: 1,
                            note: None,
                            cwd: None,
                            source: None,
                        },
                    )]
                    .into(),
                    limit: 0,
                    ..ExampleConfig::default()
                }))
            }

            fn admit_layer(&mut self, sources: &LayerSources) -> Result<(), Self::Error> {
                self.0.admit_layer(sources)
            }
        }

        let f = Fixture::<ExampleConfig>::new();
        let resolved = f.resolve().await.unwrap();
        assert!(resolved.report.sources.is_empty());
        // Absent candidates are the ordinary case and raise no diagnostic.
        assert!(resolved.report.diagnostics.is_empty());
        assert_eq!(resolved.config.limit, 1);
        write(&f.local, "{}");
        assert_eq!(f.resolve().await.unwrap().config.limit, 1);

        let missing = resolve_paths::<Config>(&f.workspace, None, Vec::new())
            .await
            .unwrap_err();
        assert!(matches!(
            missing,
            LayerResolutionError::Missing {
                file_name: "config.yaml",
                ..
            }
        ));
        let error = resolve_paths::<LimitedDefaults>(&f.workspace, None, Vec::new())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            LayerResolutionError::Resolution {
                file_name: "defaults.yaml",
                stage: ResolutionStage::Effective,
                ..
            }
        ));
        assert_eq!(
            error.report().diagnostics[0].path,
            f.local.with_file_name("defaults.yaml")
        );
        assert!(error.to_string().contains("model count exceeds limit"));

        let explicit = f.home.with_file_name("override.yaml");
        let error = resolve_layers::<ExampleConfig>(&f.workspace, Some(&explicit))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            LayerResolutionError::Resolution {
                stage: ResolutionStage::Explicit,
                ..
            }
        ));
        assert_eq!(error.report().diagnostics[0].path, explicit);
    }
}
