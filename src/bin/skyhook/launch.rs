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
    tool::policy::{AllowAll, Capability},
};
use std::{path::PathBuf, sync::Arc};

#[derive(Clone)]
pub struct Launch {
    pub(crate) config: Arc<Config>,
    pub model: String,
    pub profile: Option<String>,
    pub workspace: PathBuf,
    pub sessions: PathBuf,
    pub(crate) catalog: EmbeddedShimCatalog,
    pub(crate) interaction: Option<Arc<UiInteraction>>,
    pub(crate) approve_all: bool,
}
impl Launch {
    pub async fn create(&self, resume: Option<SessionId>) -> Result<SessionHandle, String> {
        let mut model = self.model.clone();
        let mut profile = self.profile.clone();
        if let Some(id) = resume {
            let records = SessionStore::read_records(&self.sessions, id)
                .await
                .map_err(|e| e.to_string())?;
            if let Some((m, p)) =
                skyhook::session::agent_selection(&records, &skyhook::identity::AgentId::root(id))
            {
                model = m;
                profile = p;
            }
        }
        if !self.config.models.contains_key(&model) {
            return Err(format!(
                "Model profile {model} is missing. Restore it in the configuration before resuming."
            ));
        }
        let mut config = (*self.config).clone();
        config.default_agent_profile = profile;
        let builder = config
            .harness_builder(&self.workspace, &model)
            .map_err(|e| e.to_string())?
            .shim_catalog(self.catalog.clone());
        let builder = if self.approve_all {
            builder.policy(Arc::new(AllowAll))
        } else {
            builder.policy(Arc::new(HostApprovalPolicy::new(
                self.config.capabilities.iter().copied().collect(),
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

/// CLI capability selection is an exact override, applied before host narrowing.
pub async fn load_config(args: &Args) -> Result<Config, Box<dyn std::error::Error>> {
    let mut config = Config::load(args.config.as_deref()).await?;
    if let Some(capabilities) = &args.capabilities {
        config.capabilities.clone_from(&capabilities.0);
    }
    if args.non_interactive {
        config
            .capabilities
            .retain(|capability| *capability != Capability::Interactive);
    }
    Ok(config)
}

/// Preserve explicit, remembered, then configured model selection for both hosts.
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
            profile: args
                .agent_profile
                .clone()
                .or_else(|| config.default_agent_profile.clone()),
            workspace,
            sessions,
            approve_all: args.approve_all || config.approve_all,
            config,
            catalog: super::embedded_shims::catalog()?,
            interaction,
        })
    }
}
