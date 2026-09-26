//! Harness configuration and initialization.

use super::*;

pub struct HarnessBuilder {
    workspace: PathBuf,
    session_root: Option<PathBuf>,
    catalog: CatalogSource,
    policy: Arc<dyn Policy>,
    questions: Option<Arc<dyn QuestionHandler>>,
    extra_tools: ToolRegistry,
    mcp: BTreeMap<String, McpServerConfig>,
    instructions: Vec<String>,
    max_child_depth: usize,
    capabilities: CapabilitySet,
    targets: TargetsConfig,
    shim_catalog: EmbeddedShimCatalog,
    sensitive_prompts: Arc<dyn SensitivePromptHandler>,
}

/// A configured model: its profile and the provider that serves it.
#[derive(Clone)]
pub(crate) struct ModelEntry {
    pub profile: ModelProfile,
    pub provider: Arc<dyn Provider>,
}

/// The models and modes a harness serves, with what a new session starts from.
/// The defaults are members.
pub(crate) struct Catalog {
    pub(crate) models: indexmap::IndexMap<ModelRef, ModelEntry>,
    pub(crate) default_model: ModelRef,
    pub(crate) modes: indexmap::IndexMap<String, Mode>,
    /// The mode a new session starts in; none without modes.
    pub(crate) mode: Option<String>,
}

/// A catalog admitted with the configuration is proven; one assembled through
/// the setters is checked by `build`. Any catalog setter makes it the latter.
enum CatalogSource {
    Admitted(Catalog),
    Assembled(Assembled),
}

impl Default for CatalogSource {
    fn default() -> Self {
        Self::Assembled(Assembled::default())
    }
}

impl CatalogSource {
    fn assembled(self) -> Assembled {
        match self {
            Self::Admitted(catalog) => Assembled {
                models: catalog.models,
                default_model: Some(catalog.default_model),
                modes: catalog.modes,
                mode: catalog.mode,
            },
            Self::Assembled(assembled) => assembled,
        }
    }
}

#[derive(Default)]
struct Assembled {
    models: indexmap::IndexMap<ModelRef, ModelEntry>,
    default_model: Option<ModelRef>,
    modes: indexmap::IndexMap<String, Mode>,
    mode: Option<String>,
}

impl Assembled {
    fn check(self) -> Result<Catalog, HarnessError> {
        let default_model = self
            .default_model
            .ok_or(HarnessError::MissingDefaultModel)?;
        if !self.models.contains_key(&default_model) {
            return Err(HarnessError::UnknownModel(default_model));
        }
        for (name, entry) in &self.models {
            entry
                .profile
                .validate_limits()
                .map_err(|error| HarnessError::InvalidModel {
                    model: name.clone(),
                    error,
                })?;
        }
        // Without modes the root agent holds the ceiling itself and no mode applies.
        let mode = match self.mode {
            _ if self.modes.is_empty() => None,
            Some(mode) if !self.modes.contains_key(&mode) => {
                return Err(HarnessError::UnknownMode(mode));
            }
            Some(mode) => Some(mode),
            None => self.modes.keys().next().cloned(),
        };
        Ok(Catalog {
            models: self.models,
            default_model,
            modes: self.modes,
            mode,
        })
    }
}

/// A mode is a set: the journal pins it sorted and without repeats. Interaction
/// follows the host, so a mode never lists it.
fn normalize_modes(modes: &mut indexmap::IndexMap<String, Mode>) {
    for mode in modes.values_mut() {
        (mode.capabilities).retain(|capability| *capability != Capability::Interactive);
        mode.capabilities.sort();
        mode.capabilities.dedup();
    }
}

impl HarnessBuilder {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self::with_catalog(workspace, CatalogSource::default())
    }

    /// A builder over a catalog that configuration admission has already proven.
    #[must_use]
    pub(crate) fn admitted(workspace: impl Into<PathBuf>, mut catalog: Catalog) -> Self {
        normalize_modes(&mut catalog.modes);
        Self::with_catalog(workspace, CatalogSource::Admitted(catalog))
    }

    fn with_catalog(workspace: impl Into<PathBuf>, catalog: CatalogSource) -> Self {
        Self {
            workspace: workspace.into(),
            session_root: None,
            catalog,
            policy: Arc::new(AllowAll),
            questions: None,
            extra_tools: ToolRegistry::default(),
            mcp: BTreeMap::new(),
            instructions: Vec::new(),
            max_child_depth: 4,
            capabilities: CapabilitySet::default(),
            targets: TargetsConfig::default(),
            shim_catalog: EmbeddedShimCatalog::default(),
            sensitive_prompts: Arc::new(RejectSensitivePrompts),
        }
    }

    fn assemble(mut self, edit: impl FnOnce(&mut Assembled)) -> Self {
        let mut assembled = std::mem::take(&mut self.catalog).assembled();
        edit(&mut assembled);
        self.catalog = CatalogSource::Assembled(assembled);
        self
    }

    #[must_use]
    pub fn session_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.session_root = Some(path.into());
        self
    }

    /// A provider and the models it serves, each named `provider/model`. A later
    /// call for the same provider name adds or replaces models.
    #[must_use]
    pub fn provider(
        self,
        name: ProviderName,
        provider: Arc<dyn Provider>,
        models: impl IntoIterator<Item = (ModelName, ModelProfile)>,
    ) -> Self {
        self.assemble(|catalog| {
            for (model, profile) in models {
                let entry = ModelEntry {
                    profile,
                    provider: provider.clone(),
                };
                catalog
                    .models
                    .insert(ModelRef::new(name.clone(), model), entry);
            }
        })
    }

    #[must_use]
    pub fn default_model(self, model: ModelRef) -> Self {
        self.assemble(|catalog| catalog.default_model = Some(model))
    }

    #[must_use]
    pub fn policy(mut self, policy: Arc<dyn Policy>) -> Self {
        self.policy = policy;
        self
    }

    #[must_use]
    pub fn question_handler(mut self, handler: Arc<dyn QuestionHandler>) -> Self {
        self.questions = Some(handler);
        self
    }

    #[must_use]
    pub fn tools(mut self, tools: ToolRegistry) -> Self {
        self.extra_tools = tools;
        self
    }

    /// Configure root-owned MCP servers. Discovery occurs at session startup.
    #[must_use]
    pub fn mcp(mut self, servers: BTreeMap<String, McpServerConfig>) -> Self {
        self.mcp = servers;
        self
    }

    #[must_use]
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions.push(instructions.into());
        self
    }

    #[must_use]
    pub const fn max_child_depth(mut self, depth: usize) -> Self {
        self.max_child_depth = depth;
        self
    }

    #[must_use]
    pub fn capabilities(mut self, capabilities: CapabilitySet) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// The modes the root agent can run in, and with a hint its descendants; each is
    /// limited by `capabilities`. With none, the root agent holds `capabilities` itself.
    #[must_use]
    pub fn modes(self, mut modes: indexmap::IndexMap<String, Mode>) -> Self {
        normalize_modes(&mut modes);
        self.assemble(|catalog| catalog.modes = modes)
    }

    /// The mode a new session starts in; by default the first. `build` rejects one
    /// the modes do not declare.
    #[must_use]
    pub fn mode(self, mode: impl Into<String>) -> Self {
        self.assemble(|catalog| catalog.mode = Some(mode.into()))
    }

    #[must_use]
    pub fn targets_config(mut self, targets: TargetsConfig) -> Self {
        self.targets = targets;
        self
    }

    /// Supplies the platform shims used for SSH-backed tools and agents.
    #[must_use]
    pub fn shim_catalog(mut self, catalog: EmbeddedShimCatalog) -> Self {
        self.shim_catalog = catalog;
        self
    }

    #[must_use]
    pub fn sensitive_prompt_handler(mut self, handler: Arc<dyn SensitivePromptHandler>) -> Self {
        self.sensitive_prompts = handler;
        self
    }

    /// Load user instructions, then instructions from the resolved workspace
    /// (the more local scope), then append library-supplied instructions.
    /// Instruction discovery is independent of the YAML configuration path.
    pub async fn build(self) -> Result<Harness, HarnessError> {
        let workspace = fs::canonicalize(&self.workspace).await?;
        let catalog = match self.catalog {
            CatalogSource::Admitted(catalog) => catalog,
            CatalogSource::Assembled(assembled) => assembled.check()?,
        };
        let session_root = self
            .session_root
            .unwrap_or_else(|| crate::config::workspace_session_root(&workspace));
        let (mut instructions, mut discovery_warnings) =
            load_agent_instructions(&workspace).await?;
        let skills = HostSkills::discover(&workspace).await;
        discovery_warnings.extend_from_slice(skills.warnings());
        let target_definitions = self.targets.definitions()?;
        TargetRegistry::from_definitions(target_definitions.clone())?;
        instructions.extend(self.instructions);
        // A host handler must not restore a revoked interaction capability,
        // including authentication forwarded from remote workers.
        let sensitive_prompts = if self.capabilities.contains(Capability::Interactive) {
            self.sensitive_prompts
        } else {
            Arc::new(RejectSensitivePrompts) as Arc<dyn SensitivePromptHandler>
        };
        Ok(Harness {
            inner: Arc::new(HarnessInner {
                workspace,
                session_root,
                models: catalog.models,
                default_model: catalog.default_model,
                policy: self.policy,
                questions: self.questions,
                extra_tools: self.extra_tools,
                mcp: self.mcp,
                instructions,
                skills,
                discovery_warnings,
                max_child_depth: self.max_child_depth,
                capabilities: self.capabilities,
                modes: catalog.modes,
                mode: catalog.mode,
                target_definitions,
                shim_catalog: self.shim_catalog,
                sensitive_prompts,
            }),
        })
    }
}
const AGENT_INSTRUCTION_NAMES: [&str; 4] = ["AGENTS.md", "agents.md", "Agents.md", "AGENTS.MD"];

struct AgentInstructionFile {
    canonical_path: PathBuf,
    #[cfg(unix)]
    identity: (u64, u64),
    text: String,
}

impl AgentInstructionFile {
    fn same_file(&self, other: &Self) -> bool {
        #[cfg(unix)]
        if self.identity == other.identity {
            return true;
        }
        self.canonical_path == other.canonical_path
    }
}

fn instruction_error(path: &Path, error: std::io::Error) -> std::io::Error {
    std::io::Error::new(
        error.kind(),
        format!("AGENTS.md instruction file {}: {error}", path.display()),
    )
}

/// An instruction file that is too large or not a regular file is skipped with a
/// warning; other failures stop startup.
enum Discovered {
    Loaded(AgentInstructionFile),
    Skipped(String),
}

async fn read_instruction_directory(
    directory: &Path,
) -> Result<Option<Discovered>, std::io::Error> {
    for name in AGENT_INSTRUCTION_NAMES {
        let path = directory.join(name);
        // A dangling symlink is an existing candidate, not permission to try the
        // next spelling. Other discovery errors also fail this entire location.
        match fs::symlink_metadata(&path).await {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(instruction_error(&path, error)),
        }
        let result = async {
            let io = |error| match error {
                crate::fs::RegularFileError::Io(error) => error,
                error => std::io::Error::other(error),
            };
            let limit = crate::media::MAX_TEXT_BYTES;
            let file = match crate::fs::open_regular(&path, limit).await {
                Ok(file) => file,
                Err(
                    error @ (crate::fs::RegularFileError::NotRegular
                    | crate::fs::RegularFileError::TooLarge { .. }),
                ) => {
                    return Ok(Discovered::Skipped(format!(
                        "AGENTS.md instruction file {} skipped: {error}",
                        path.display()
                    )));
                }
                Err(error) => return Err(io(error)),
            };
            // The identity comes from the handle that is read, not a later path lookup.
            #[cfg(unix)]
            let identity = {
                use std::os::unix::fs::MetadataExt as _;
                let metadata = file.metadata()?;
                (metadata.dev(), metadata.ino())
            };
            let bytes = crate::fs::read_to_limit(file, limit).await.map_err(io)?;
            let text = String::from_utf8(bytes)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
            Ok(Discovered::Loaded(AgentInstructionFile {
                canonical_path: fs::canonicalize(&path).await?,
                #[cfg(unix)]
                identity,
                text,
            }))
        }
        .await;
        return result
            .map(Some)
            .map_err(|error| instruction_error(&path, error));
    }
    Ok(None)
}

async fn load_user_instructions(
    directories: &[PathBuf],
    warnings: &mut Vec<String>,
) -> Result<Option<AgentInstructionFile>, std::io::Error> {
    let mut failures = Vec::new();
    for directory in directories {
        match read_instruction_directory(directory).await {
            Ok(Some(Discovered::Loaded(file))) => return Ok(Some(file)),
            Ok(Some(Discovered::Skipped(warning))) => warnings.push(warning),
            Ok(None) => {}
            Err(error) => failures.push(error),
        }
    }
    // Missing locations are harmless, but do not hide a failure at another
    // location. Preserve the last failure's kind and report all failing paths.
    if let Some(last) = failures.last() {
        return Err(std::io::Error::new(
            last.kind(),
            failures
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }
    Ok(None)
}

// Instruction discovery deliberately does not depend on the selected YAML config.
/// Instruction texts, then warnings for skipped instruction files.
async fn load_agent_instructions(
    workspace: &Path,
) -> Result<(Vec<String>, Vec<String>), std::io::Error> {
    load_agent_instructions_from(workspace, &crate::config::user_config_directories()).await
}

async fn load_agent_instructions_from(
    workspace: &Path,
    user_directories: &[PathBuf],
) -> Result<(Vec<String>, Vec<String>), std::io::Error> {
    let mut warnings = Vec::new();
    let user = load_user_instructions(user_directories, &mut warnings).await?;
    // The builder supplies the resolved workspace. Never walk its ancestors.
    let workspace = match read_instruction_directory(workspace).await? {
        Some(Discovered::Loaded(file)) => Some(file),
        Some(Discovered::Skipped(warning)) => {
            warnings.push(warning);
            None
        }
        None => None,
    };
    let mut output = Vec::new();
    if let Some(user) = &user {
        output.push(format!("user AGENTS.md:\n{}", user.text));
    }
    if let Some(workspace) = workspace {
        // First occurrence wins when both locations identify the same file.
        if !user.as_ref().is_some_and(|user| user.same_file(&workspace)) {
            output.push(format!("workspace AGENTS.md:\n{}", workspace.text));
        }
    }
    Ok((output, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agent::runtime::tests::*, provider::profile::LimitsError};

    fn instruction_file(directory: &Path, name: &str, content: impl AsRef<[u8]>) {
        std::fs::create_dir_all(directory).unwrap();
        std::fs::write(directory.join(name), content).unwrap();
    }

    fn contains_path(error: &std::io::Error, path: PathBuf) -> bool {
        error.to_string().contains(&path.display().to_string())
    }

    #[tokio::test]
    async fn build_rejects_invalid_model_profiles() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let base = || test_builder(root.path(), &sessions, Arc::new(HangingProvider), false);
        let profile = |context, output| ModelProfile::new("model", None, context, output, false);
        let bad = |profile| {
            base().provider(
                "test".parse().unwrap(),
                Arc::new(HangingProvider),
                [("bad".parse().unwrap(), profile)],
            )
        };
        let cases = [
            (
                "missing default",
                base().default_model("test/absent".parse().unwrap()),
            ),
            ("zero context", bad(profile(0, 1))),
            ("output limit", bad(profile(8, 8))),
        ];
        for (case, builder) in cases {
            let (model, error) = match builder.build().await {
                Err(HarnessError::UnknownModel(model)) => (model, None),
                Err(HarnessError::InvalidModel { model, error }) => (model, Some(error)),
                other => panic!("{case}: expected a profile error, got {:?}", other.err()),
            };
            let expected = match case {
                "missing default" => ("test/absent", None),
                "zero context" => ("test/bad", Some(LimitsError::Context)),
                _ => ("test/bad", Some(LimitsError::OutputExceedsContext)),
            };
            assert_eq!((model.to_string().as_str(), error), expected, "{case}");
        }
        assert!(base().build().await.is_ok());
    }

    #[tokio::test]
    async fn starting_mode_is_checked_at_build_whichever_setter_came_first() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let base = || test_builder(root.path(), &sessions, Arc::new(HangingProvider), false);
        let mode = |capabilities: &[Capability]| Mode {
            capabilities: capabilities.to_vec(),
            instructions: None,
            hint: None,
        };
        let modes = || {
            [
                ("look".to_owned(), mode(&[Capability::Read])),
                (
                    "work".to_owned(),
                    mode(&[Capability::Read, Capability::Write]),
                ),
            ]
            .into()
        };
        let starting = |harness: Harness| harness.inner.mode.clone();
        let built = base().mode("work").modes(modes()).build().await.unwrap();
        assert_eq!(starting(built).as_deref(), Some("work"));
        let built = base().modes(modes()).mode("work").build().await.unwrap();
        assert_eq!(starting(built).as_deref(), Some("work"));
        let built = base().modes(modes()).build().await.unwrap();
        assert_eq!(starting(built).as_deref(), Some("look"));
        for builder in [
            base().mode("missing").modes(modes()),
            base().modes(modes()).mode("missing"),
        ] {
            let error = builder.build().await.err().unwrap();
            assert!(matches!(error, HarnessError::UnknownMode(mode) if mode == "missing"));
        }
        // Without modes the root holds the ceiling itself; a selection does not apply.
        let built = base().mode("work").build().await.unwrap();
        assert!(starting(built).is_none());
    }

    #[tokio::test]
    async fn instructions_are_labeled_in_order_without_ancestors_or_duplicate_files() {
        // (files as (directory, name, content), user directories, expected)
        type Case<'a> = (
            &'a [(&'a str, &'a str, &'a str)],
            &'a [&'a str],
            &'a [&'a str],
        );
        let cases: &[Case] = &[
            // User then workspace; whitespace is preserved.
            (
                &[
                    ("user", "AGENTS.md", "user rules\n"),
                    ("workspace", "AGENTS.md", " \n\t"),
                ],
                &["user"],
                &[
                    "user AGENTS.md:\nuser rules\n",
                    "workspace AGENTS.md:\n \n\t",
                ],
            ),
            // An empty user file selects its location and casing without fallback.
            (
                &[
                    ("user", "agents.md", "alternate"),
                    ("user", "AGENTS.md", ""),
                    ("fallback", "AGENTS.md", "fallback"),
                    ("workspace", "AGENTS.md", ""),
                ],
                &["user", "fallback"],
                &["user AGENTS.md:\n", "workspace AGENTS.md:\n"],
            ),
            // Missing user locations are allowed and ancestors are never loaded.
            (
                &[("", "AGENTS.md", "ancestor must not load")],
                &["missing-xdg", "missing-home"],
                &[],
            ),
            // The same file as user and workspace instructions is included once.
            (
                &[("workspace", "AGENTS.md", "shared")],
                &["workspace"],
                &["user AGENTS.md:\nshared"],
            ),
            // The first user location holding a file wins; later ones are fallbacks.
            (
                &[
                    ("xdg", "AGENTS.md", "preferred"),
                    ("home", "AGENTS.md", "fallback"),
                ],
                &["xdg", "home"],
                &["user AGENTS.md:\npreferred"],
            ),
            (
                &[("home", "AGENTS.md", "fallback")],
                &["xdg", "home"],
                &["user AGENTS.md:\nfallback"],
            ),
            // Distinct files with identical contents are not deduplicated.
            (
                &[
                    ("user", "AGENTS.md", "same text"),
                    ("workspace", "AGENTS.md", "same text"),
                ],
                &["user"],
                &[
                    "user AGENTS.md:\nsame text",
                    "workspace AGENTS.md:\nsame text",
                ],
            ),
        ];
        for (files, users, expected) in cases {
            let root = tempfile::tempdir().unwrap();
            let workspace = root.path().join("workspace");
            std::fs::create_dir_all(&workspace).unwrap();
            for (directory, name, content) in *files {
                instruction_file(&root.path().join(directory), name, content);
            }
            let users = users
                .iter()
                .map(|user| root.path().join(user))
                .collect::<Vec<_>>();
            let loaded = load_agent_instructions_from(&workspace, &users)
                .await
                .unwrap();
            assert_eq!(
                loaded,
                (
                    expected.iter().map(|text| text.to_string()).collect(),
                    Vec::new()
                ),
                "{files:?}"
            );
        }
    }

    #[tokio::test]
    async fn casing_selects_first_existing_for_user_and_workspace() {
        for (index, name) in AGENT_INSTRUCTION_NAMES.iter().enumerate() {
            let root = tempfile::tempdir().unwrap();
            let instructions = root.path().join("instructions");
            let empty = root.path().join("empty");
            std::fs::create_dir(&empty).unwrap();
            // Create in reverse order so creation order cannot choose the winner.
            for candidate in AGENT_INSTRUCTION_NAMES[index..].iter().rev() {
                instruction_file(&instructions, candidate, candidate);
            }
            let users = std::slice::from_ref(&instructions);
            let user = load_agent_instructions_from(&empty, users).await.unwrap();
            assert_eq!(user.0, vec![format!("user AGENTS.md:\n{name}")]);
            let workspace = load_agent_instructions_from(&instructions, &[])
                .await
                .unwrap();
            assert_eq!(workspace.0, vec![format!("workspace AGENTS.md:\n{name}")]);
        }
    }

    #[tokio::test]
    async fn a_broken_candidate_fails_its_location_without_trying_another_casing() {
        use std::io::ErrorKind::{InvalidData, NotFound};
        // ENOTDIR is a discovery error, unlike a missing candidate; a dangling
        // symlink is an existing candidate.
        for (case, kind) in [
            ("not-a-directory", None),
            ("utf8", Some(InvalidData)),
            #[cfg(unix)]
            ("dangling", Some(NotFound)),
        ] {
            let root = tempfile::tempdir().unwrap();
            let (user, fallback) = (root.path().join("user"), root.path().join("fallback"));
            let broken = user.join("AGENTS.md");
            match case {
                "not-a-directory" => std::fs::write(&user, "a file").unwrap(),
                "utf8" => instruction_file(&user, "AGENTS.md", [0xff]),
                _ => {
                    std::fs::create_dir(&user).unwrap();
                    #[cfg(unix)]
                    std::os::unix::fs::symlink("missing", &broken).unwrap();
                }
            }
            if user.is_dir() && std::fs::symlink_metadata(user.join("agents.md")).is_err() {
                instruction_file(&user, "agents.md", "must not load");
            }
            // Fatal for a workspace, and for users unless another location loads.
            let error = load_agent_instructions_from(&user, &[]).await.unwrap_err();
            assert!(contains_path(&error, broken.clone()), "{case}: {error}");
            let users = [user, root.path().join("missing"), fallback.clone()];
            let error = load_user_instructions(&users, &mut Vec::new())
                .await
                .err()
                .unwrap();
            assert!(contains_path(&error, broken.clone()), "{case}: {error}");
            assert!(kind.is_none_or(|kind| kind == error.kind()), "{case}");
            // Every failing location is reported.
            instruction_file(&fallback, "Agents.md", [0xfe]);
            let error = load_user_instructions(&users, &mut Vec::new())
                .await
                .err()
                .unwrap();
            assert!(contains_path(&error, broken), "{case}: {error}");
            assert!(contains_path(&error, fallback.join("Agents.md")));
            instruction_file(&fallback, "AGENTS.md", "fallback");
            let loaded = load_user_instructions(&users, &mut Vec::new())
                .await
                .unwrap();
            assert_eq!(loaded.unwrap().text, "fallback", "{case}");
        }
    }

    /// An instruction file that is too large or not a regular file is skipped
    /// with a warning naming it, and the next user location still loads.
    #[tokio::test]
    async fn unusable_instruction_files_are_skipped_with_warnings() {
        for case in ["directory", "oversized"] {
            let root = tempfile::tempdir().unwrap();
            let (user, fallback) = (root.path().join("user"), root.path().join("fallback"));
            let skipped = user.join("AGENTS.md");
            match case {
                "directory" => std::fs::create_dir_all(&skipped).unwrap(),
                _ => instruction_file(
                    &user,
                    "AGENTS.md",
                    vec![b'x'; crate::media::MAX_TEXT_BYTES as usize + 1],
                ),
            }
            instruction_file(&fallback, "AGENTS.md", "fallback");
            let (loaded, warnings) = load_agent_instructions_from(&user, &[user.clone(), fallback])
                .await
                .unwrap();
            assert_eq!(loaded, vec!["user AGENTS.md:\nfallback"], "{case}");
            assert_eq!(warnings.len(), 2, "{case}: {warnings:?}");
            assert!(
                warnings
                    .iter()
                    .all(|warning| warning.contains(&skipped.display().to_string())),
                "{case}: {warnings:?}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_and_hardlinks_to_the_same_file_are_not_repeated() {
        for hardlink in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let user = root.path().join("user");
            let workspace = root.path().join("workspace");
            instruction_file(&user, "AGENTS.md", "shared");
            std::fs::create_dir(&workspace).unwrap();
            if hardlink {
                std::fs::hard_link(user.join("AGENTS.md"), workspace.join("Agents.md")).unwrap();
            } else {
                std::os::unix::fs::symlink(user.join("AGENTS.md"), workspace.join("agents.md"))
                    .unwrap();
            }
            let loaded = load_agent_instructions_from(&workspace, &[user])
                .await
                .unwrap();
            assert_eq!(loaded.0, vec!["user AGENTS.md:\nshared"]);
        }
    }

    #[tokio::test]
    async fn library_instructions_are_appended_after_workspace_instructions() {
        let root = tempfile::tempdir().unwrap();
        instruction_file(root.path(), "AGENTS.md", "workspace rules");
        let provider = Arc::new(HangingProvider);
        let harness = test_builder(root.path(), &root.path().join("sessions"), provider, false)
            .instructions("library rules")
            .build()
            .await
            .unwrap();
        let expected =
            ["workspace AGENTS.md:\nworkspace rules", "library rules"].map(str::to_owned);
        assert!(harness.inner.instructions.ends_with(&expected));
    }

    #[tokio::test]
    async fn revoked_interactive_rejects_supplied_sensitive_handler() {
        use crate::remote::{
            PromptAnswer, SensitivePrompt, SensitivePromptFuture, SensitivePromptKind,
        };

        struct RecordingSensitive(Arc<AtomicUsize>);
        impl SensitivePromptHandler for RecordingSensitive {
            fn prompt(&self, _prompt: SensitivePrompt) -> SensitivePromptFuture {
                self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(PromptAnswer::Confirmed) })
            }
        }

        let root = tempfile::tempdir().unwrap();
        for interactive in [false, true] {
            let calls = Arc::new(AtomicUsize::new(0));
            let mut capabilities = CapabilitySet::default();
            if !interactive {
                capabilities.remove(Capability::Interactive);
            }
            let provider = Arc::new(HangingProvider);
            let harness = test_builder(root.path(), &root.path().join("sessions"), provider, false)
                .capabilities(capabilities)
                .policy(Arc::new(crate::tool::policy::AllowAll))
                .sensitive_prompt_handler(Arc::new(RecordingSensitive(calls.clone())))
                .build()
                .await
                .unwrap();
            use SensitivePromptKind::*;
            for kind in [
                Password,
                KeyboardInteractive,
                KeyPassphrase,
                HostConfirmation,
                AgentConfirmation,
            ] {
                let message = "authentication requested".to_owned();
                let prompt = SensitivePrompt { kind, message };
                let result = harness.inner.sensitive_prompts.prompt(prompt).await;
                match result {
                    Ok(_) => assert!(interactive),
                    Err(error) => {
                        assert!(!interactive);
                        let found = error.to_string();
                        assert_eq!(found, "interactive authentication is unavailable");
                    }
                }
            }
            let found = calls.load(Ordering::SeqCst);
            assert_eq!(found, if interactive { 5 } else { 0 });
        }
    }
}
