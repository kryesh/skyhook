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

/// CLI policy capabilities replace the configured allowlist.
pub async fn load_config(args: &Args) -> Result<Config, Box<dyn std::error::Error>> {
    let mut config = Config::load(args.config.as_deref()).await?;
    if let Some(capabilities) = &args.capabilities {
        config.capabilities.clone_from(&capabilities.0);
    }
    Ok(config)
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
        let sessions = config
            .session_root
            .clone()
            .unwrap_or_else(|| workspace.join(".skyhook/sessions"));
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
