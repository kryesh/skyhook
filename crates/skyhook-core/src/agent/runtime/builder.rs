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
    /// Instruction discovery is independent of the TOML configuration path.
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
        for (name, config) in &self.mcp {
            config.validate().map_err(|error| {
                HarnessError::Initialization(format!("invalid MCP server {name}: {error}"))
            })?;
        }
        let session_root = self
            .session_root
            .unwrap_or_else(|| workspace.join(".skyhook/sessions"));
        let mut instructions = load_agent_instructions(&workspace).await?;
        let skills = HostSkills::discover(&workspace).await;
        let imported = if self.targets.import_ssh_config {
            import_ssh_targets().await?
        } else {
            Vec::new()
        };
        let target_definitions = imported
            .into_iter()
            .chain(self.targets.definitions()?)
            .map(|definition| (definition.name.clone(), definition))
            .collect::<BTreeMap<_, _>>()
            .into_values()
            .collect::<Vec<_>>();
        let target_definitions = crate::target::normalize::normalize(
            target_definitions,
            Vec::new(),
            Arc::new(crate::target::normalize::LocalResolver),
        )
        .await?;
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

// Instruction discovery deliberately does not depend on the selected TOML config.
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

    #[test]
    fn instruction_roots_are_ordered_and_independent_of_config() {
        let xdg = PathBuf::from("xdg");
        let home = PathBuf::from("home");
        assert_eq!(
            user_instruction_directories(Some(xdg.clone().into()), Some(home.clone().into())),
            vec![xdg.join("skyhook"), home.join(".config/skyhook")]
        );
        assert_eq!(
            user_instruction_directories(Some("".into()), Some(home.clone().into())),
            vec![home.join(".config/skyhook")]
        );
        assert_eq!(
            user_instruction_directories(
                Some(home.join(".config").into()),
                Some(home.clone().into())
            ),
            vec![home.join(".config/skyhook")]
        );
        assert!(user_instruction_directories(None, Some("".into())).is_empty());
    }

    #[tokio::test]
    async fn user_only_instructions_select_one_location() {
        let root = tempfile::tempdir().unwrap();
        let xdg = root.path().join("xdg");
        let home = root.path().join("home");
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        instruction_file(&xdg, "AGENTS.md", "preferred");
        instruction_file(&home, "AGENTS.md", "fallback");
        let directories = [xdg.clone(), home];
        assert_eq!(
            load_agent_instructions_from(&workspace, &directories)
                .await
                .unwrap(),
            vec!["user AGENTS.md:\npreferred"]
        );
        std::fs::remove_file(xdg.join("AGENTS.md")).unwrap();
        assert_eq!(
            load_agent_instructions_from(&workspace, &directories)
                .await
                .unwrap(),
            vec!["user AGENTS.md:\nfallback"]
        );
    }

    #[tokio::test]
    async fn user_and_workspace_instructions_are_labeled_in_order() {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("user");
        let workspace = root.path().join("workspace");
        instruction_file(&user, "AGENTS.md", "user rules\n");
        instruction_file(&workspace, "AGENTS.md", "local rules\n");
        assert_eq!(
            load_agent_instructions_from(&workspace, &[user])
                .await
                .unwrap(),
            vec![
                "user AGENTS.md:\nuser rules\n",
                "workspace AGENTS.md:\nlocal rules\n"
            ]
        );
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
            assert_eq!(
                load_agent_instructions_from(&empty, std::slice::from_ref(&instructions))
                    .await
                    .unwrap(),
                vec![format!("user AGENTS.md:\n{name}")]
            );
            assert_eq!(
                load_agent_instructions_from(&instructions, &[])
                    .await
                    .unwrap(),
                vec![format!("workspace AGENTS.md:\n{name}")]
            );
        }
    }

    #[tokio::test]
    async fn empty_user_file_selects_location_and_casing_without_fallback() {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("user");
        let fallback = root.path().join("fallback");
        let workspace = root.path().join("workspace");
        instruction_file(&user, "agents.md", "alternate");
        instruction_file(&user, "AGENTS.md", "");
        instruction_file(&fallback, "AGENTS.md", "fallback");
        instruction_file(&workspace, "AGENTS.md", "");
        assert_eq!(
            load_agent_instructions_from(&workspace, &[user, fallback])
                .await
                .unwrap(),
            vec!["user AGENTS.md:\n", "workspace AGENTS.md:\n"]
        );
    }

    #[tokio::test]
    async fn whitespace_is_preserved() {
        let root = tempfile::tempdir().unwrap();
        instruction_file(root.path(), "AGENTS.md", " \n\t");
        assert_eq!(
            load_agent_instructions_from(root.path(), &[])
                .await
                .unwrap(),
            vec!["workspace AGENTS.md:\n \n\t"]
        );
    }

    #[tokio::test]
    async fn missing_user_locations_and_workspace_file_are_allowed_without_ancestors() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("nested/workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        instruction_file(root.path(), "AGENTS.md", "ancestor must not load");
        instruction_file(
            workspace.parent().unwrap(),
            "agents.md",
            "near ancestor must not load",
        );
        assert!(
            load_agent_instructions_from(
                &workspace,
                &[
                    root.path().join("missing-xdg"),
                    root.path().join("missing-home")
                ]
            )
            .await
            .unwrap()
            .is_empty()
        );
    }

    #[tokio::test]
    async fn user_utf8_error_falls_back_without_trying_another_casing() {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("user");
        let fallback = root.path().join("fallback");
        instruction_file(&user, "agents.md", "must not load");
        instruction_file(&user, "AGENTS.md", [0xff]);
        instruction_file(&fallback, "Agents.md", "fallback");
        let loaded = load_user_instructions(&[user, fallback])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.text, "fallback");
    }

    #[tokio::test]
    async fn user_discovery_and_read_errors_fall_back() {
        for discovery_error in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let user = root.path().join("user");
            let fallback = root.path().join("fallback");
            if discovery_error {
                // ENOTDIR is a discovery error, unlike a missing candidate.
                std::fs::write(&user, "not a directory").unwrap();
            } else {
                std::fs::create_dir_all(user.join("AGENTS.md")).unwrap();
                if !user.join("agents.md").exists() {
                    instruction_file(&user, "agents.md", "must not load");
                }
            }
            instruction_file(&fallback, "AGENTS.MD", "fallback");
            assert_eq!(
                load_user_instructions(&[user, fallback])
                    .await
                    .unwrap()
                    .unwrap()
                    .text,
                "fallback"
            );
        }
    }

    #[tokio::test]
    async fn user_error_survives_missing_fallback_and_reports_path() {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("user");
        instruction_file(&user, "AGENTS.md", [0xff]);
        let error = load_user_instructions(&[user.clone(), root.path().join("missing")])
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            error
                .to_string()
                .contains(&user.join("AGENTS.md").display().to_string())
        );
    }

    #[tokio::test]
    async fn both_user_errors_are_reported() {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("user");
        let fallback = root.path().join("fallback");
        instruction_file(&user, "AGENTS.md", [0xff]);
        instruction_file(&fallback, "Agents.md", [0xfe]);
        let error = load_user_instructions(&[user.clone(), fallback.clone()])
            .await
            .err()
            .unwrap();
        for path in [user.join("AGENTS.md"), fallback.join("Agents.md")] {
            assert!(error.to_string().contains(&path.display().to_string()));
        }
    }

    #[tokio::test]
    async fn workspace_errors_are_fatal_without_trying_another_casing() {
        for invalid_utf8 in [false, true] {
            let root = tempfile::tempdir().unwrap();
            if invalid_utf8 {
                instruction_file(root.path(), "AGENTS.md", [0xff]);
            } else {
                std::fs::create_dir(root.path().join("AGENTS.md")).unwrap();
            }
            if !root.path().join("agents.md").exists() {
                instruction_file(root.path(), "agents.md", "must not load");
            }
            let error = load_agent_instructions_from(root.path(), &[])
                .await
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(&root.path().join("AGENTS.md").display().to_string())
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dangling_candidate_is_an_error_not_an_alternate_spelling() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("user");
        let fallback = root.path().join("fallback");
        std::fs::create_dir(&user).unwrap();
        symlink("missing", user.join("AGENTS.md")).unwrap();
        if std::fs::symlink_metadata(user.join("agents.md")).is_err() {
            instruction_file(&user, "agents.md", "must not load");
        }
        let error = load_agent_instructions_from(&user, &[]).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        instruction_file(&fallback, "AGENTS.md", "fallback");
        assert_eq!(
            load_user_instructions(&[user, fallback])
                .await
                .unwrap()
                .unwrap()
                .text,
            "fallback"
        );
    }

    #[tokio::test]
    async fn same_user_and_workspace_file_is_only_included_once() {
        let root = tempfile::tempdir().unwrap();
        instruction_file(root.path(), "AGENTS.md", "shared");
        assert_eq!(
            load_agent_instructions_from(root.path(), &[root.path().to_owned()])
                .await
                .unwrap(),
            vec!["user AGENTS.md:\nshared"]
        );
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
            assert_eq!(
                load_agent_instructions_from(&workspace, &[user])
                    .await
                    .unwrap(),
                vec!["user AGENTS.md:\nshared"]
            );
        }
    }

    #[tokio::test]
    async fn distinct_files_with_identical_contents_are_not_deduplicated() {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("user");
        let workspace = root.path().join("workspace");
        instruction_file(&user, "AGENTS.md", "same text");
        instruction_file(&workspace, "AGENTS.md", "same text");
        assert_eq!(
            load_agent_instructions_from(&workspace, &[user])
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn library_instructions_are_appended_after_workspace_instructions() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        instruction_file(workspace.path(), "AGENTS.md", "workspace rules");
        let harness = test_builder(workspace.path(), sessions.path(), Arc::new(HangingProvider))
            .instructions("library rules")
            .build()
            .await
            .unwrap();
        assert!(harness.inner.instructions.ends_with(&[
            "workspace AGENTS.md:\nworkspace rules".to_owned(),
            "library rules".to_owned(),
        ]));
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

        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        for interactive in [false, true] {
            let calls = Arc::new(AtomicUsize::new(0));
            let mut capabilities = CapabilitySet::default();
            if !interactive {
                capabilities.remove(Capability::Interactive);
            }
            let harness =
                test_builder(workspace.path(), sessions.path(), Arc::new(HangingProvider))
                    .capabilities(capabilities)
                    .policy(Arc::new(crate::tool::policy::AllowAll))
                    .sensitive_prompt_handler(Arc::new(RecordingSensitive(calls.clone())))
                    .build()
                    .await
                    .unwrap();
            for kind in [
                SensitivePromptKind::Password,
                SensitivePromptKind::KeyboardInteractive,
                SensitivePromptKind::KeyPassphrase,
                SensitivePromptKind::HostConfirmation,
                SensitivePromptKind::AgentConfirmation,
            ] {
                let result = harness
                    .inner
                    .sensitive_prompts
                    .prompt(SensitivePrompt {
                        kind,
                        message: "authentication requested".to_owned(),
                    })
                    .await;
                assert_eq!(result.is_ok(), interactive);
                if !interactive {
                    assert_eq!(
                        result.unwrap_err().to_string(),
                        "interactive authentication is unavailable"
                    );
                }
            }
            assert_eq!(
                calls.load(Ordering::SeqCst),
                if interactive { 5 } else { 0 }
            );
        }
    }
}
