//! Harness configuration and initialization.

use super::*;

pub struct HarnessBuilder {
    workspace: PathBuf,
    session_root: Option<PathBuf>,
    providers: BTreeMap<String, Arc<dyn Provider>>,
    model_profiles: BTreeMap<String, ModelProfile>,
    default_model_profile: Option<String>,
    policy: Arc<dyn Policy>,
    questions: Option<Arc<dyn QuestionHandler>>,
    extra_tools: ToolRegistry,
    mcp: BTreeMap<String, McpServerConfig>,
    instructions: Vec<String>,
    max_child_depth: usize,
    capabilities: CapabilitySet,
    modes: indexmap::IndexMap<String, Mode>,
    mode: Option<String>,
    targets: TargetsConfig,
    shim_catalog: EmbeddedShimCatalog,
    sensitive_prompts: Arc<dyn SensitivePromptHandler>,
}

impl HarnessBuilder {
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            session_root: None,
            providers: BTreeMap::new(),
            model_profiles: BTreeMap::new(),
            default_model_profile: None,
            policy: Arc::new(AllowAll),
            questions: None,
            extra_tools: ToolRegistry::default(),
            mcp: BTreeMap::new(),
            instructions: Vec::new(),
            max_child_depth: 4,
            capabilities: CapabilitySet::default(),
            modes: indexmap::IndexMap::new(),
            mode: None,
            targets: TargetsConfig::default(),
            shim_catalog: EmbeddedShimCatalog::default(),
            sensitive_prompts: Arc::new(RejectSensitivePrompts),
        }
    }

    #[must_use]
    pub fn session_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.session_root = Some(path.into());
        self
    }

    #[must_use]
    pub fn provider(mut self, name: impl Into<String>, provider: Arc<dyn Provider>) -> Self {
        self.providers.insert(name.into(), provider);
        self
    }

    #[must_use]
    pub fn model_profile(mut self, name: impl Into<String>, profile: ModelProfile) -> Self {
        self.model_profiles.insert(name.into(), profile);
        self
    }

    #[must_use]
    pub fn default_model_profile(mut self, name: impl Into<String>) -> Self {
        self.default_model_profile = Some(name.into());
        self
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
    /// Clears the selected mode.
    #[must_use]
    pub fn modes(mut self, mut modes: indexmap::IndexMap<String, Mode>) -> Self {
        // A mode is a set: the journal pins it sorted and without repeats. Interaction
        // follows the host, so a mode never lists it.
        for mode in modes.values_mut() {
            (mode.capabilities).retain(|capability| *capability != Capability::Interactive);
            mode.capabilities.sort();
            mode.capabilities.dedup();
        }
        self.modes = modes;
        self.mode = None;
        self
    }

    /// The mode a new session starts in; by default the first.
    #[must_use]
    pub fn mode(mut self, mode: impl Into<String>) -> Self {
        self.mode = Some(mode.into());
        self
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
        let default_model_profile = self
            .default_model_profile
            .ok_or(HarnessError::MissingDefaultModelProfile)?;
        validate_model_profiles(
            &self.providers,
            &self.model_profiles,
            &default_model_profile,
        )?;
        let session_root = self
            .session_root
            .unwrap_or_else(|| workspace.join(".skyhook/sessions"));
        let mode = self.mode.or_else(|| self.modes.keys().next().cloned());
        if let Some(mode) = &mode
            && !self.modes.contains_key(mode)
        {
            return Err(HarnessError::UnknownMode(mode.clone()));
        }
        let mut instructions = load_agent_instructions(&workspace).await?;
        let skills = HostSkills::discover(&workspace).await;
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
                providers: self.providers,
                model_profiles: self.model_profiles,
                default_model_profile,
                policy: self.policy,
                questions: self.questions,
                extra_tools: self.extra_tools,
                mcp: self.mcp,
                instructions,
                skills,
                max_child_depth: self.max_child_depth,
                capabilities: self.capabilities,
                modes: self.modes,
                mode,
                target_definitions,
                shim_catalog: self.shim_catalog,
                sensitive_prompts,
            }),
        })
    }
}
fn validate_model_profiles(
    providers: &BTreeMap<String, Arc<dyn Provider>>,
    models: &BTreeMap<String, ModelProfile>,
    default_model: &str,
) -> Result<(), HarnessError> {
    if !models.contains_key(default_model) {
        return Err(HarnessError::UnknownModelProfile(default_model.to_owned()));
    }
    for (name, profile) in models {
        profile.validate_limits().map_err(|error| {
            HarnessError::InvalidProfile(format!("model profile `{name}`: {error}"))
        })?;
        if !providers.contains_key(&profile.provider) {
            return Err(HarnessError::InvalidProfile(format!(
                "model profile `{name}` uses unknown provider `{}`",
                profile.provider
            )));
        }
    }
    Ok(())
}

const AGENT_INSTRUCTION_NAMES: [&str; 4] = ["AGENTS.md", "agents.md", "Agents.md", "AGENTS.MD"];

// Instruction discovery deliberately does not depend on the selected YAML config.
fn user_instruction_directories(
    xdg_config_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    if let Some(root) = xdg_config_home.filter(|root| !root.is_empty()) {
        directories.push(PathBuf::from(root).join("skyhook"));
    }
    if let Some(home) = home.filter(|home| !home.is_empty()) {
        let fallback = PathBuf::from(home).join(".config/skyhook");
        if !directories.contains(&fallback) {
            directories.push(fallback);
        }
    }
    directories
}

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

async fn read_instruction_directory(
    directory: &Path,
) -> Result<Option<AgentInstructionFile>, std::io::Error> {
    use tokio::io::AsyncReadExt as _;

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
            let mut file = fs::File::open(&path).await?;
            #[cfg(unix)]
            let identity = {
                use std::os::unix::fs::MetadataExt as _;
                let metadata = file.metadata().await?;
                (metadata.dev(), metadata.ino())
            };
            let mut text = String::new();
            file.read_to_string(&mut text).await?;
            Ok(AgentInstructionFile {
                canonical_path: fs::canonicalize(&path).await?,
                #[cfg(unix)]
                identity,
                text,
            })
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
) -> Result<Option<AgentInstructionFile>, std::io::Error> {
    let mut failures = Vec::new();
    for directory in directories {
        match read_instruction_directory(directory).await {
            Ok(Some(file)) => return Ok(Some(file)),
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

async fn load_agent_instructions(workspace: &Path) -> Result<Vec<String>, std::io::Error> {
    let directories = user_instruction_directories(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    );
    load_agent_instructions_from(workspace, &directories).await
}

async fn load_agent_instructions_from(
    workspace: &Path,
    user_directories: &[PathBuf],
) -> Result<Vec<String>, std::io::Error> {
    let user = load_user_instructions(user_directories).await?;
    // The builder supplies the resolved workspace. Never walk its ancestors.
    let workspace = read_instruction_directory(workspace).await?;
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
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::runtime::tests::*;

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
        let profile = |provider, context, output| {
            ModelProfile::new(provider, "model", None, context, output, false)
        };
        let cases = [
            ("missing default", base().default_model_profile("absent")),
            (
                "zero context",
                base().model_profile("bad", profile("test", 0, 1)),
            ),
            (
                "output limit",
                base().model_profile("bad", profile("test", 8, 8)),
            ),
            (
                "provider",
                base().model_profile("bad", profile("absent", 16, 8)),
            ),
        ];
        for (case, builder) in cases {
            let message = match builder.build().await {
                Err(HarnessError::UnknownModelProfile(name)) => name,
                Err(HarnessError::InvalidProfile(message)) => message,
                other => panic!("{case}: expected a profile error, got {:?}", other.err()),
            };
            let expected = match case {
                "missing default" => "absent",
                "zero context" => "model profile `bad`: max_context must be positive",
                "output limit" => {
                    "model profile `bad`: max_output must be smaller than max_context"
                }
                _ => "model profile `bad` uses unknown provider `absent`",
            };
            assert_eq!(message, expected, "{case}");
        }
        assert!(base().build().await.is_ok());
    }

    #[test]
    fn instruction_roots_are_ordered_and_independent_of_config() {
        let xdg = PathBuf::from("xdg");
        let home = PathBuf::from("home");
        let directories = |xdg: &Path, home: &Path| {
            user_instruction_directories(Some(xdg.into()), Some(home.into()))
        };
        let preferred = vec![xdg.join("skyhook"), home.join(".config/skyhook")];
        assert_eq!(directories(&xdg, &home), preferred);
        let fallback = vec![home.join(".config/skyhook")];
        assert_eq!(directories(Path::new(""), &home), fallback);
        assert_eq!(directories(&home.join(".config"), &home), fallback);
        assert!(user_instruction_directories(None, Some("".into())).is_empty());
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
            assert_eq!(loaded, *expected, "{files:?}");
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
            assert_eq!(user, vec![format!("user AGENTS.md:\n{name}")]);
            let workspace = load_agent_instructions_from(&instructions, &[])
                .await
                .unwrap();
            assert_eq!(workspace, vec![format!("workspace AGENTS.md:\n{name}")]);
        }
    }

    #[tokio::test]
    async fn a_broken_candidate_fails_its_location_without_trying_another_casing() {
        use std::io::ErrorKind::{InvalidData, NotFound};
        // ENOTDIR is a discovery error, unlike a missing candidate; a dangling
        // symlink is an existing candidate.
        for (case, kind) in [
            ("not-a-directory", None),
            ("directory", None),
            ("utf8", Some(InvalidData)),
            #[cfg(unix)]
            ("dangling", Some(NotFound)),
        ] {
            let root = tempfile::tempdir().unwrap();
            let (user, fallback) = (root.path().join("user"), root.path().join("fallback"));
            let broken = user.join("AGENTS.md");
            match case {
                "not-a-directory" => std::fs::write(&user, "a file").unwrap(),
                "directory" => std::fs::create_dir_all(&broken).unwrap(),
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
            let error = load_user_instructions(&users).await.err().unwrap();
            assert!(contains_path(&error, broken.clone()), "{case}: {error}");
            assert!(kind.is_none_or(|kind| kind == error.kind()), "{case}");
            // Every failing location is reported.
            instruction_file(&fallback, "Agents.md", [0xfe]);
            let error = load_user_instructions(&users).await.err().unwrap();
            assert!(contains_path(&error, broken), "{case}: {error}");
            assert!(contains_path(&error, fallback.join("Agents.md")));
            instruction_file(&fallback, "AGENTS.md", "fallback");
            let loaded = load_user_instructions(&users).await.unwrap();
            assert_eq!(loaded.unwrap().text, "fallback", "{case}");
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
            assert_eq!(loaded, vec!["user AGENTS.md:\nshared"]);
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
            SecretValue, SensitivePrompt, SensitivePromptFuture, SensitivePromptKind,
        };

        struct RecordingSensitive(Arc<AtomicUsize>);
        impl SensitivePromptHandler for RecordingSensitive {
            fn prompt(&self, _prompt: SensitivePrompt) -> SensitivePromptFuture {
                self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(SecretValue::new("answer".to_owned())) })
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
