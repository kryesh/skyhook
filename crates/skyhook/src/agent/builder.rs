//! Harness configuration and initialization.

use super::HarnessInner;
use super::{Harness, HarnessError, HostState, QuestionHandler, ResumeHook, inbox};
use crate::mcp::McpServerConfig;
use crate::provider::Provider;
use crate::provider::model::{ModelEntry, ModelName, ModelProfile, ModelRef, ProviderName};
use crate::remote::EmbeddedShimCatalog;
use crate::target::TargetRegistry;
use crate::target::TargetsConfig;
use crate::tool::ToolRegistry;
use crate::tool::builtins::HostSkills;
use crate::{
    config::mode::{Mode, ModeName},
    tool::policy::{
        AllowAll, Capability, CapabilitySet, HostCapability, HostCapabilityDeclaration, Policy,
    },
};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs;

pub struct HarnessBuilder {
    workspace: PathBuf,
    host: crate::identity::HostName,
    session_root: Option<PathBuf>,
    catalog: Assembled,
    policy: Arc<dyn Policy>,
    questions: Option<Arc<dyn QuestionHandler>>,
    host_state: Option<Arc<dyn HostState>>,
    resume_hook: Option<Arc<dyn ResumeHook>>,
    session_schema: crate::session::Schema,
    extra_tools: ToolRegistry,
    prompts: crate::prompts::Prompts,
    skills: HostSkills,
    mcp: BTreeMap<String, McpServerConfig>,
    max_child_depth: usize,
    coalescing: inbox::Coalescing,
    foreground: super::foreground::Limits,
    capabilities: CapabilitySet,
    host_capabilities: BTreeSet<HostCapability>,
    targets: TargetsConfig,
    shim_catalog: EmbeddedShimCatalog,
}

/// The models and modes a harness serves, with what a new session starts from.
/// The defaults are members.
struct Catalog {
    models: indexmap::IndexMap<ModelRef, ModelEntry>,
    default_model: ModelRef,
    modes: Modes,
}

/// The modes a harness serves: none, when the root holds the ceiling itself and no
/// mode applies, or some, with the one a new session starts in.
#[derive(Clone)]
pub(super) enum Modes {
    Unconfigured,
    Configured {
        modes: indexmap::IndexMap<ModeName, Mode>,
        default: ModeName,
    },
}

impl Modes {
    /// Every mode, in configured order, and the one a new session starts in.
    pub(super) fn configured(&self) -> Option<(&indexmap::IndexMap<ModeName, Mode>, &ModeName)> {
        match self {
            Self::Unconfigured => None,
            Self::Configured { modes, default } => Some((modes, default)),
        }
    }
}

#[derive(Default)]
struct Assembled {
    models: indexmap::IndexMap<ModelRef, ModelEntry>,
    default_model: Option<ModelRef>,
    modes: indexmap::IndexMap<ModeName, Mode>,
    mode: Option<ModeName>,
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
        let first = self.modes.keys().next().cloned();
        let modes = match (self.mode, first) {
            (_, None) => Modes::Unconfigured,
            (Some(mode), _) if !self.modes.contains_key(&mode) => {
                return Err(HarnessError::UnknownMode(mode));
            }
            (default, Some(first)) => Modes::Configured {
                default: default.unwrap_or(first),
                modes: self.modes,
            },
        };
        Ok(Catalog {
            models: self.models,
            default_model,
            modes,
        })
    }
}

impl HarnessBuilder {
    #[must_use]
    /// `host` names the application creating sessions; another host's sessions
    /// open view-only.
    pub fn new(workspace: impl Into<PathBuf>, host: crate::identity::HostName) -> Self {
        Self {
            workspace: workspace.into(),
            host,
            session_root: None,
            catalog: Assembled::default(),
            policy: Arc::new(AllowAll),
            questions: None,
            host_state: None,
            resume_hook: None,
            session_schema: crate::session::Schema::JOURNAL,
            extra_tools: ToolRegistry::default(),
            prompts: Default::default(),
            skills: HostSkills::default(),
            mcp: BTreeMap::new(),
            max_child_depth: crate::config::DEFAULT_MAX_CHILD_DEPTH,
            coalescing: inbox::Coalescing::default(),
            foreground: super::foreground::Limits::default(),
            capabilities: CapabilitySet::default(),
            host_capabilities: BTreeSet::new(),
            targets: TargetsConfig::default(),
            shim_catalog: EmbeddedShimCatalog::default(),
        }
    }

    /// A harness serving every model `model`'s configuration admits, which starts new
    /// sessions on `model`.
    pub fn configured(
        model: &crate::config::ConfiguredModel,
        workspace: impl Into<PathBuf>,
        host: crate::identity::HostName,
    ) -> Result<Self, crate::config::ConfigError> {
        let runtime = model.config();
        let config = runtime.config();
        let models = runtime.catalog(&host)?;
        Ok(Self::new(workspace, host)
            .models(models)
            .default_model(model.name())
            .modes(config.modes.clone())
            .mode(runtime.default_mode().clone())
            .max_child_depth(config.max_child_depth)
            .capabilities(config.ceiling())
            .mcp(config.mcp.clone())
            .targets_config(config.targets.clone()))
    }

    fn assemble(mut self, edit: impl FnOnce(&mut Assembled)) -> Self {
        edit(&mut self.catalog);
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

    /// Built models, added or replacing like [`Self::provider`]'s.
    #[must_use]
    fn models(self, models: indexmap::IndexMap<ModelRef, ModelEntry>) -> Self {
        self.assemble(|catalog| catalog.models.extend(models))
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

    /// The host's section of every agent's runtime state, sent with each request.
    #[must_use]
    pub fn host_state(mut self, state: Arc<dyn HostState>) -> Self {
        self.host_state = Some(state);
        self
    }

    /// What this launch changes about the work of each of its own sessions it
    /// reopens, run before that work begins.
    #[must_use]
    pub fn resume_hook(mut self, hook: Arc<dyn ResumeHook>) -> Self {
        self.resume_hook = Some(hook);
        self
    }

    /// The schema of every session this harness creates, with any tables of the host's
    /// own, checked on every one of the host's it reopens. The journal's by default.
    #[must_use]
    pub fn session_schema(mut self, schema: crate::session::Schema) -> Self {
        self.session_schema = schema;
        self
    }

    /// The host's prompts: its templates, which render its persona and its modes'
    /// instructions for each agent, with its section of that agent's context.
    ///
    /// Resumed agents retain their recorded prompt until a mode change rebuilds it.
    #[must_use]
    pub fn prompts(mut self, prompts: crate::prompts::Prompts) -> Self {
        self.prompts = prompts;
        self
    }

    /// Skills offered through the `skill` tool; the host decides where they come from.
    #[must_use]
    pub fn skills(mut self, skills: HostSkills) -> Self {
        self.skills = skills;
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
    pub const fn max_child_depth(mut self, depth: usize) -> Self {
        self.max_child_depth = depth;
        self
    }

    #[cfg(test)]
    pub(super) fn coalescing(mut self, coalescing: inbox::Coalescing) -> Self {
        self.coalescing = coalescing;
        self
    }

    #[cfg(test)]
    pub(super) fn foreground(mut self, foreground: super::foreground::Limits) -> Self {
        self.foreground = foreground;
        self
    }

    #[must_use]
    pub fn capabilities(mut self, capabilities: CapabilitySet) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Declare host capabilities. Declaring one does not grant it: ceilings, modes,
    /// tool requirements and MCP servers may only use declared host capabilities.
    #[must_use]
    pub fn host_capabilities(
        mut self,
        capabilities: impl IntoIterator<Item = HostCapabilityDeclaration>,
    ) -> Self {
        (self.host_capabilities).extend(capabilities.into_iter().map(|declared| declared.name));
        self
    }

    /// The modes the root agent can run in, and with a hint its descendants; each is
    /// limited by `capabilities`. With none, the root agent holds `capabilities` itself.
    #[must_use]
    pub fn modes(self, modes: indexmap::IndexMap<ModeName, Mode>) -> Self {
        self.assemble(|catalog| catalog.modes = modes)
    }

    /// The mode a new session starts in; by default the first. `build` rejects one
    /// the modes do not declare.
    #[must_use]
    pub fn mode(self, mode: ModeName) -> Self {
        self.assemble(|catalog| catalog.mode = Some(mode))
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

    /// Load user instructions, then instructions from the resolved workspace
    /// (the more local scope).
    pub async fn build(self) -> Result<Harness, HarnessError> {
        let workspace = fs::canonicalize(&self.workspace).await?;
        if workspace.to_str().is_none() {
            return Err(HarnessError::UnspelledWorkspace(workspace));
        }
        let catalog = self.catalog.check()?;
        let modes = catalog.modes.configured();
        let modes = modes.iter().flat_map(|(modes, _)| modes.values());
        let configured = self
            .capabilities
            .iter()
            .chain(modes.flat_map(|mode| mode.capabilities.iter()))
            .chain(self.extra_tools.required_capabilities())
            .chain(
                self.mcp
                    .values()
                    .flat_map(|server| server.capabilities().iter()),
            );
        for capability in configured {
            if let Capability::Host(host) = capability
                && !self.host_capabilities.contains(host)
            {
                return Err(HarnessError::UndeclaredHostCapability(host.clone()));
            }
        }
        let session_root = self
            .session_root
            .unwrap_or_else(|| crate::config::workspace_session_root(&workspace));
        let users = crate::config::user_config_directories();
        let (instructions, mut warnings) = load_agent_instructions(&workspace, &users).await?;
        warnings.extend_from_slice(self.skills.warnings());
        for (name, entry) in &catalog.models {
            let max_context = entry.profile.max_context.get();
            let max_output = entry.profile.max_output.get();
            if max_output >= max_context.div_ceil(2) {
                warnings.push(format!(
                    "Model `{name}`: max_output ({max_output}) is at least 50% of max_context ({max_context}); \
                     most of the context window will not be used for input before compaction. Consider lowering max_output."
                ));
            }
        }
        let targets = self.targets.definitions()?;
        TargetRegistry::from_definitions(targets.clone())?;
        Ok(Harness {
            inner: Arc::new(HarnessInner {
                workspace,
                host: self.host,
                session_root,
                models: catalog.models,
                default_model: catalog.default_model,
                policy: self.policy,
                questions: self.questions,
                host_state: self.host_state,
                resume_hook: self.resume_hook,
                session_schema: self.session_schema,
                extra_tools: self.extra_tools,
                mcp: self.mcp,
                instructions,
                prompts: self.prompts,
                skills: self.skills,
                warnings,
                max_child_depth: self.max_child_depth,
                coalescing: self.coalescing,
                foreground: self.foreground,
                capabilities: self.capabilities,
                modes: catalog.modes,
                targets: crate::target::ConfiguredTargets::of(targets),
                shim_catalog: self.shim_catalog,
            }),
        })
    }
}
const AGENT_INSTRUCTION_NAMES: [&str; 4] = ["AGENTS.md", "agents.md", "Agents.md", "AGENTS.MD"];

struct AgentInstructionFile {
    identity: (u64, u64),
    text: String,
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
            use skyhook_worker::op::FsError;
            let io = |error| match error {
                FsError::Os(error) => std::io::Error::from(&error),
                error => std::io::Error::other(error),
            };
            let limit = skyhook_base::limits::MAX_TEXT_BYTES;
            let file = match skyhook_worker::fs::open_regular(&path, limit).await {
                Ok(file) => file,
                Err(error @ (FsError::NotRegular | FsError::TooLarge { .. })) => {
                    return Ok(Discovered::Skipped(format!(
                        "AGENTS.md instruction file {} skipped: {error}",
                        path.display()
                    )));
                }
                Err(error) => return Err(io(error)),
            };
            // The identity comes from the handle that is read, not a later path lookup.
            let identity = {
                use std::os::unix::fs::MetadataExt as _;
                let metadata = file.metadata()?;
                (metadata.dev(), metadata.ino())
            };
            let bytes = skyhook_worker::fs::read_to_limit(file, limit)
                .await
                .map_err(io)?;
            let text = String::from_utf8(bytes)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
            Ok(Discovered::Loaded(AgentInstructionFile { identity, text }))
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

/// Instruction texts, then warnings for skipped instruction files.
async fn load_agent_instructions(
    workspace: &Path,
    user_directories: &[PathBuf],
) -> Result<(Vec<String>, Vec<String>), std::io::Error> {
    let mut warnings = Vec::new();
    let user = load_user_instructions(user_directories, &mut warnings).await?;
    // The builder supplies the resolved workspace.
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
        if !user
            .as_ref()
            .is_some_and(|user| user.identity == workspace.identity)
        {
            output.push(format!("workspace AGENTS.md:\n{}", workspace.text));
        }
    }
    Ok((output, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::catalog::ToolOptions;
    use crate::{agent::tests::*, provider::model::LimitsError};
    use std::assert_matches;

    #[derive(serde::Deserialize, schemars::JsonSchema)]
    struct HostInput {}

    fn host_tool(options: ToolOptions<HostInput>) -> ToolRegistry {
        let mut tools = ToolRegistryBuilder::default();
        tools
            .register_unit("review", "Record a review.", options, |_, _| async {
                Ok(())
            })
            .unwrap();
        tools.build()
    }

    /// Tools name the workspace as text, so a workspace that is not is refused
    /// before anything starts.
    #[tokio::test]
    async fn a_workspace_that_is_not_text_is_refused() {
        use std::os::unix::ffi::OsStringExt as _;
        let root = tempfile::tempdir().unwrap();
        let name = std::ffi::OsString::from_vec(b"work-\xff".to_vec());
        let workspace = std::fs::canonicalize(root.path()).unwrap().join(name);
        std::fs::create_dir(&workspace).unwrap();
        let sessions = root.path().join("sessions");
        let built = test_builder(&workspace, &sessions, Arc::new(HangingProvider), false);
        let refused = built.build().await.err().unwrap();
        assert_matches!(refused, HarnessError::UnspelledWorkspace(path) if path == workspace);
    }

    #[tokio::test]
    async fn build_rejects_undeclared_host_capabilities() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let base = || test_builder(root.path(), &sessions, Arc::new(HangingProvider), false);
        let host: HostCapability = "host.review".parse().unwrap();
        let capability = Capability::Host(host.clone());
        let mode = |capabilities| Mode {
            capabilities,
            instructions: None,
            hint: None,
        };
        let cases = [
            (
                "ceiling",
                base().capabilities([capability.clone()].into_iter().collect()),
            ),
            (
                "inactive mode outside ceiling",
                base().modes(
                    [
                        (mode_name("read"), mode(listed(&[Capability::Read]))),
                        (
                            mode_name("review"),
                            mode(listed(std::slice::from_ref(&capability))),
                        ),
                    ]
                    .into(),
                ),
            ),
            (
                "tool requirement",
                base().tools(host_tool(
                    ToolOptions::default().requires(capability.clone()),
                )),
            ),
            (
                "tool execution capability",
                base().tools(host_tool(ToolOptions::new(vec![capability.clone()]))),
            ),
            (
                "root requirement",
                base().tools(host_tool(
                    ToolOptions::default().requires_for_root(capability.clone()),
                )),
            ),
            (
                "MCP server requirement",
                base().mcp(
                    [(
                        "research".to_owned(),
                        serde_json::from_value(json!({
                            "transport": "stdio",
                            "start_command": ["never-started"],
                            "capabilities": [capability],
                        }))
                        .unwrap(),
                    )]
                    .into(),
                ),
            ),
        ];
        for (case, builder) in cases {
            assert!(
                matches!(builder.build().await, Err(HarnessError::UndeclaredHostCapability(found)) if found == host),
                "{case} must require a declaration"
            );
        }
        assert!(
            base()
                .capabilities([Capability::Host(host.clone())].into_iter().collect())
                .host_capabilities([HostCapabilityDeclaration {
                    name: host,
                    title: "Host action",
                }])
                .build()
                .await
                .is_ok()
        );
    }

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
        let limit = crate::tests::limit(8);
        let bad = base().provider(
            "test".parse().unwrap(),
            Arc::new(HangingProvider),
            [(
                "bad".parse().unwrap(),
                ModelProfile::new("model".parse().unwrap(), None, limit, limit, false),
            )],
        );
        let cases = [
            (
                "missing default",
                base().default_model("test/absent".parse().unwrap()),
            ),
            ("output limit", bad),
        ];
        for (case, builder) in cases {
            let (model, error) = match builder.build().await {
                Err(HarnessError::UnknownModel(model)) => (model, None),
                Err(HarnessError::InvalidModel { model, error }) => (model, Some(error)),
                other => panic!("{case}: expected a profile error, got {:?}", other.err()),
            };
            let expected = match case {
                "missing default" => ("test/absent", None),
                _ => ("test/bad", Some(LimitsError)),
            };
            assert_eq!((model.to_string().as_str(), error), expected, "{case}");
        }
        assert!(base().build().await.is_ok());
    }

    #[tokio::test]
    async fn models_reserving_half_their_context_for_output_warn_whether_selected_or_not() {
        let root = tempfile::tempdir().unwrap();
        let cases = [
            (100, 49, false),
            (100, 50, true),
            (101, 50, false),
            (101, 51, true),
        ];
        let models = cases
            .iter()
            .enumerate()
            .map(|(index, &(context, output, _))| {
                (
                    format!("case-{index}").parse().unwrap(),
                    ModelProfile {
                        max_context: crate::tests::limit(context),
                        max_output: crate::tests::limit(output),
                        ..crate::tests::profile("test", false)
                    },
                )
            });
        let harness = HarnessBuilder::new(root.path(), "test".parse().unwrap())
            .provider("test".parse().unwrap(), Arc::new(HangingProvider), models)
            .default_model("test/case-0".parse().unwrap())
            .build()
            .await
            .unwrap();
        for (index, &(context, output, warns)) in cases.iter().enumerate() {
            let name = format!("Model `test/case-{index}`:");
            let warned = (harness.inner.warnings.iter()).any(|warning| warning.starts_with(&name));
            assert_eq!(warned, warns, "{context}/{output}");
        }
    }

    #[tokio::test]
    async fn starting_mode_is_checked_at_build_whichever_setter_came_first() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let base = || test_builder(root.path(), &sessions, Arc::new(HangingProvider), false);
        let (look, work, missing) = (mode_name("look"), mode_name("work"), mode_name("missing"));
        let modes = || {
            [
                (look.clone(), mode(&[Capability::Read], None)),
                (
                    work.clone(),
                    mode(&[Capability::Read, Capability::Write], None),
                ),
            ]
            .into()
        };
        let starting = |harness: Harness| Some(harness.inner.modes.configured()?.1.clone());
        for builder in [
            base().mode(work.clone()).modes(modes()),
            base().modes(modes()).mode(work.clone()),
        ] {
            assert_eq!(
                starting(builder.build().await.unwrap()).as_ref(),
                Some(&work)
            );
        }
        let built = base().modes(modes()).build().await.unwrap();
        assert_eq!(starting(built).as_ref(), Some(&look));
        for builder in [
            base().mode(missing.clone()).modes(modes()),
            base().modes(modes()).mode(missing.clone()),
        ] {
            let error = builder.build().await.err().unwrap();
            assert_matches!(error, HarnessError::UnknownMode(mode) if mode == missing);
        }
        // Without modes the root holds the ceiling itself; a selection does not apply.
        let built = base().mode(work).build().await.unwrap();
        assert!(starting(built).is_none());
    }

    #[tokio::test]
    async fn instructions_are_labeled_in_order_without_duplicate_files() {
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
            let loaded = load_agent_instructions(&workspace, &users).await.unwrap();
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
            let user = load_agent_instructions(&empty, users).await.unwrap();
            assert_eq!(user.0, vec![format!("user AGENTS.md:\n{name}")]);
            let workspace = load_agent_instructions(&instructions, &[]).await.unwrap();
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
                    std::os::unix::fs::symlink("missing", &broken).unwrap();
                }
            }
            if user.is_dir() && std::fs::symlink_metadata(user.join("agents.md")).is_err() {
                instruction_file(&user, "agents.md", "must not load");
            }
            // Fatal for a workspace, and for users unless another location loads.
            let error = load_agent_instructions(&user, &[]).await.unwrap_err();
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
                    vec![b'x'; skyhook_base::limits::MAX_TEXT_BYTES as usize + 1],
                ),
            }
            instruction_file(&fallback, "AGENTS.md", "fallback");
            let (loaded, warnings) = load_agent_instructions(&user, &[user.clone(), fallback])
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
            let loaded = load_agent_instructions(&workspace, &[user]).await.unwrap();
            assert_eq!(loaded.0, vec!["user AGENTS.md:\nshared"]);
        }
    }
}
