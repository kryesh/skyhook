//! Session launch settings shared by terminal and headless hosts.
use super::{
    Args,
    interaction::{HostApprovalPolicy, UiInteraction},
};
use skyhook::{
    agent::SessionHandle,
    config::Config,
    identity::SessionId,
    remote::EmbeddedShimCatalog,
    session::SessionStore,
    tool::policy::{AllowAll, Capability, CapabilitySet},
};
use std::{path::PathBuf, sync::Arc};

#[derive(Clone)]
pub struct Launch {
    pub(crate) config: Arc<Config>,
    pub model: String,
    pub workspace: PathBuf,
    pub sessions: PathBuf,
    pub(crate) catalog: EmbeddedShimCatalog,
    pub(crate) interaction: Option<Arc<UiInteraction>>,
    pub(crate) approve_all: bool,
}
impl Launch {
    pub async fn create(&self, resume: Option<SessionId>) -> Result<SessionHandle, String> {
        let mut model = self.model.clone();
        if let Some(id) = resume {
            let records = SessionStore::read_records(&self.sessions, id)
                .await
                .map_err(|e| e.to_string())?;
            if let Some(m) =
                skyhook::session::agent_selection(&records, &skyhook::identity::AgentId::root(id))
            {
                model = m;
            }
        }
        if !self.config.models.contains_key(&model) {
            return Err(format!(
                "Model profile {model} is missing. Restore it in the configuration before resuming."
            ));
        }
        let capabilities = session_capabilities(&self.config, self.interaction.is_some());
        let builder = self
            .config
            .harness_builder(&self.workspace, &model)
            .map_err(|e| e.to_string())?
            // Keep creation/resume aligned with the workspace-local history menu,
            // even when inherited configuration supplies a library storage override.
            .session_root(self.sessions.clone())
            .shim_catalog(self.catalog.clone())
            .capabilities(capabilities.clone());
        let builder = if self.approve_all {
            builder.policy(Arc::new(AllowAll))
        } else {
            builder.policy(Arc::new(HostApprovalPolicy::new(
                capabilities,
                self.interaction.as_deref().cloned(),
            )))
        };
        let builder = if let Some(interaction) = &self.interaction {
            builder
                .question_handler(interaction.clone())
                .sensitive_prompt_handler(interaction.clone())
        } else {
            builder
        };
        let harness = builder.build().await.map_err(|e| e.to_string())?;
        match resume {
            Some(id) => harness.resume_session(id).await,
            None => harness.new_session().await,
        }
        .map_err(|e| e.to_string())
    }
}

/// Human interaction is a runtime fact, separate from the configured permissions.
fn session_capabilities(config: &Config, interactive: bool) -> CapabilitySet {
    let mut capabilities: CapabilitySet = config.capabilities.iter().copied().collect();
    capabilities.remove(Capability::Interactive);
    if interactive {
        capabilities.insert(Capability::Interactive);
    }
    capabilities
}

/// Resolve exactly the configuration shared by startup and inspection.
pub async fn resolve_config(
    args: &Args,
) -> Result<skyhook::config::ResolvedConfig, Box<dyn std::error::Error>> {
    // The core resolver owns ordering and workspace resolution, including its
    // diagnostics. Explicit files bypass workspace probing there entirely.
    let mut resolved = Config::resolve(&args.workspace, args.config.as_deref()).await?;
    let config = &mut resolved.config;
    if let Some(capabilities) = &args.capabilities {
        // Interaction is runtime-controlled, never part of the TOML allowlist.
        config.capabilities = capabilities
            .0
            .iter()
            .copied()
            .filter(|capability| *capability != Capability::Interactive)
            .collect();
    }
    config.approve_all |= args.approve_all;

    Ok(resolved)
}

/// Validate the effective CLI configuration without opening provider contexts.
pub fn validate_config(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    if config.models.is_empty() {
        return Err(
            "No model profiles configured. Add a [models.<name>] entry to your config.".into(),
        );
    }
    for (name, model) in &config.models {
        if !config.providers.contains_key(&model.provider) {
            return Err(format!(
                "Model profile {name} references unknown provider {}.",
                model.provider
            )
            .into());
        }
    }
    Ok(())
}

/// CLI policy capabilities replace the configured allowlist.
pub async fn load_config(args: &Args) -> Result<Config, Box<dyn std::error::Error>> {
    let resolved = resolve_config(args).await?;
    if !args.non_interactive {
        for diagnostic in &resolved.report.diagnostics {
            eprintln!(
                "skyhook config: {}",
                super::dump::diagnostic_text(diagnostic)
            );
        }
    }
    validate_config(&resolved.config)?;
    Ok(resolved.config)
}

/// Select the explicit model, remembered model, or first configured model for both hosts.
pub fn select_model(
    config: &Config,
    explicit: Option<&str>,
    saved: Option<&str>,
) -> Result<String, String> {
    if let Some(name) = explicit {
        return config
            .models
            .contains_key(name)
            .then(|| name.to_owned())
            .ok_or_else(|| format!("Unknown model profile: {name}"));
    }
    saved
        .filter(|name| config.models.contains_key(*name))
        .map(str::to_owned)
        .or_else(|| config.models.first().map(|(name, _)| name.clone()))
        .ok_or_else(|| {
            "No model profiles configured. Add a [models.<name>] entry to your config.".into()
        })
}

impl Launch {
    pub async fn from_args(
        args: &Args,
        config: Arc<Config>,
        model: String,
        interaction: Option<Arc<UiInteraction>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let workspace = tokio::fs::canonicalize(&args.workspace).await?;
        // CLI history belongs only to the selected workspace, never to an
        // inherited/global session_root or an ancestor workspace's history.
        let sessions = workspace.join(".skyhook/sessions");
        Ok(Self {
            model,
            workspace,
            sessions,
            approve_all: args.approve_all || config.approve_all,
            config,
            catalog: super::embedded_shims::catalog()?,
            interaction,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn history_config(root: Option<PathBuf>) -> Arc<Config> {
        let mut config: Config = toml::from_str(
            r#"
[providers.test]
kind = 'openai'
api = 'chat_completions'
base_url = 'http://127.0.0.1:1/v1'
[models.test]
provider = 'test'
model = 'fixture'
max_context = 128000
max_output = 4096
[targets]
import_ssh_config = false
"#,
        )
        .unwrap();
        config.session_root = root;
        Arc::new(config)
    }

    #[tokio::test]
    async fn history_root_is_selected_workspace_even_with_storage_override() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("project/nested");
        std::fs::create_dir_all(&workspace).unwrap();
        let args = Args::parse_from([
            "skyhook",
            "--workspace",
            workspace.join("..").to_str().unwrap(),
        ]);
        let expected = std::fs::canonicalize(workspace.parent().unwrap())
            .unwrap()
            .join(".skyhook/sessions");
        for configured in [
            None,
            Some(root.path().join("shared-sessions")),
            Some(PathBuf::from("relative-sessions")),
        ] {
            let launch = Launch::from_args(&args, history_config(configured), "test".into(), None)
                .await
                .unwrap();
            // This is the same root passed to the TUI history loader.
            assert_eq!(launch.sessions, expected);
            assert!(!launch.sessions.exists());
        }
    }

    #[tokio::test]
    async fn history_creation_and_resume_are_workspace_local_without_fallback() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let configured = root.path().join("shared-sessions");
        let ancestor = root.path().join(".skyhook/sessions");
        let sibling = root.path().join("other/.skyhook/sessions");
        let mut outside_ids = vec![];
        for outside in [&configured, &ancestor, &sibling] {
            // Seed valid, resumable history, not merely empty session directories.
            let harness = history_config(None)
                .harness_builder(&workspace, "test")
                .unwrap()
                .session_root(outside)
                .build()
                .await
                .unwrap();
            let session = harness.new_session().await.unwrap();
            outside_ids.push(session.id());
            session.shutdown().await.unwrap();
        }
        let args = Args::parse_from(["skyhook", "--workspace", workspace.to_str().unwrap()]);
        let launch = Launch::from_args(
            &args,
            history_config(Some(configured.clone())),
            "test".into(),
            None,
        )
        .await
        .unwrap();
        for id in outside_ids {
            assert!(launch.create(Some(id)).await.is_err());
        }
        assert!(!launch.sessions.exists());

        let session = launch.create(None).await.unwrap();
        let id = session.id();
        assert!(
            launch
                .sessions
                .join(id.to_string())
                .join("events.jsonl")
                .is_file()
        );
        assert!(!configured.join(id.to_string()).exists());
        session.shutdown().await.unwrap();
        drop(session);

        let resumed = launch.create(Some(id)).await.unwrap();
        assert_eq!(resumed.id(), id);
        resumed.shutdown().await.unwrap();
    }

    #[test]
    fn interaction_follows_the_host_even_with_an_empty_allowlist() {
        for text in ["capabilities = []", "capabilities = ['read']"] {
            let config: Config = toml::from_str(text).unwrap();
            for interactive in [false, true] {
                let capabilities = session_capabilities(&config, interactive);
                assert_eq!(capabilities.contains(Capability::Interactive), interactive);
                assert_eq!(
                    capabilities.contains(Capability::Read),
                    config.capabilities.contains(&Capability::Read)
                );
                assert!(!capabilities.contains(Capability::Exec));
            }
        }
    }
}
