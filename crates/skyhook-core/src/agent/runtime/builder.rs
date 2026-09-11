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

async fn load_agent_instructions(workspace: &Path) -> Result<Vec<String>, std::io::Error> {
    let mut directories = workspace.ancestors().collect::<Vec<_>>();
    directories.reverse();
    let mut output = Vec::new();
    for directory in directories {
        let path = directory.join("AGENTS.md");
        match fs::read_to_string(path).await {
            Ok(text) if !text.trim().is_empty() => output.push(text),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::runtime::tests::*;

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
