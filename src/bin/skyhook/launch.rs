//! Session launch settings shared by terminal and headless hosts.
use super::{
    cli::{self, ConfigRequest},
    interaction::{HostApprovalPolicy, UiInteraction},
};
use skyhook::{
    agent::{HarnessError, SessionHandle},
    config::{Config, ConfigError, ConfiguredModel, RuntimeConfig},
    fs::RegularFileError,
    identity::SessionId,
    media::{Attachment, Classified, MAX_IMAGE_BYTES, MAX_TEXT_BYTES, classify},
    provider::profile::ModelRef,
    remote::EmbeddedShimCatalog,
    session::{SessionError, SessionStore},
    tool::policy::{AllowAll, Capability, CapabilitySet, Policy},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// Read a workspace file as a prompt attachment: an image when its bytes are a
/// supported image, otherwise UTF-8 text.
pub(crate) async fn read_attachment(workspace: &Path, path: &Path) -> Result<Attachment, String> {
    let path = tokio::fs::canonicalize(workspace.join(path))
        .await
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if !path.starts_with(workspace) {
        return Err("File reference leaves the workspace".into());
    }
    let bytes = skyhook::fs::read_regular(&path, MAX_IMAGE_BYTES)
        .await
        .map_err(|error| match error {
            RegularFileError::TooLarge { .. } => {
                "File is too large to attach; ask the agent to read it instead".to_owned()
            }
            error => format!("{}: {error}", path.display()),
        })?;
    match classify(bytes) {
        Classified::Image(image) => Ok(Attachment::Image {
            file: Some(path),
            image,
        }),
        Classified::Text(content) if content.len() as u64 > MAX_TEXT_BYTES => {
            Err("File is larger than 1 MiB; ask the agent to read it instead".into())
        }
        Classified::Text(content) => Ok(Attachment::Text {
            file: Some(path),
            content,
        }),
        Classified::Binary(_) => Err(format!(
            "{} is neither a supported image nor UTF-8 text",
            path.display()
        )),
    }
}

/// Read the images named on the command line.
pub(crate) async fn read_images(
    workspace: &Path,
    paths: &[PathBuf],
) -> Result<Vec<Attachment>, String> {
    let mut images = Vec::with_capacity(paths.len());
    for path in paths {
        match read_attachment(workspace, path).await? {
            image @ Attachment::Image { .. } => images.push(image),
            Attachment::Text { .. } => {
                return Err(format!("{} is not a supported image", path.display()));
            }
        }
    }
    Ok(images)
}

/// A configured mode, or the exact capabilities of a batch job that named none.
#[derive(Clone)]
pub(crate) enum Permissions {
    Mode(String),
    Exact(CapabilitySet),
}

impl Permissions {
    /// A batch job's permissions. A resumed session knows modes the configuration
    /// may no longer have; it admits the name when a message selects it.
    pub(crate) fn for_batch(
        args: &cli::PermissionArgs,
        resumed: bool,
        config: &RuntimeConfig,
    ) -> Result<Self, ConfigError> {
        Ok(match args {
            cli::PermissionArgs::Mode(Some(mode)) if resumed => Self::Mode(mode.clone()),
            cli::PermissionArgs::Mode(mode) => {
                Self::Mode(config.select_mode(mode.as_deref())?.to_owned())
            }
            cli::PermissionArgs::Exact(capabilities) => {
                Self::Exact(capabilities.iter().copied().collect())
            }
        })
    }
}

/// Why a session could not be created or resumed.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("Model {0} is missing. Restore it in the configuration before resuming.")]
    MissingModel(ModelRef),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Harness(#[from] HarnessError),
}

#[derive(Clone)]
pub struct Launch {
    pub model: ConfiguredModel,
    pub(crate) permissions: Permissions,
    pub workspace: PathBuf,
    pub sessions: PathBuf,
    pub(crate) catalog: EmbeddedShimCatalog,
    pub(crate) interaction: Option<Arc<UiInteraction>>,
    pub(crate) approve_all: bool,
}
impl Launch {
    pub async fn create(&self, resume: Option<SessionId>) -> Result<SessionHandle, LaunchError> {
        let mut model = self.model.clone();
        if let Some(id) = resume {
            let summary = SessionStore::summary(&self.sessions, id).await?;
            if let Some(name) = summary.model {
                model = self
                    .model
                    .config()
                    .select_model(&name)
                    .map_err(|_| LaunchError::MissingModel(name))?;
            }
        }
        let capabilities = self.ceiling(model.config().config(), resume.is_some());
        let builder = model.harness_builder(&self.workspace)?;
        let builder = match &self.permissions {
            // A resumed session continues in its own mode, which may not be configured.
            Permissions::Mode(_) if resume.is_some() => builder,
            Permissions::Mode(mode) => builder.mode(mode),
            Permissions::Exact(_) => builder.modes(Default::default()),
        };
        let builder = builder
            // Keep creation/resume aligned with the workspace-local history menu,
            // even when inherited configuration supplies a library storage override.
            .session_root(self.sessions.clone())
            .shim_catalog(self.catalog.clone())
            .capabilities(capabilities.clone());
        let policy: Arc<dyn Policy> = match (&self.interaction, self.approve_all) {
            (_, true) => Arc::new(AllowAll),
            (None, false) => Arc::new(HostApprovalPolicy::Unattended),
            (Some(ui), false) => Arc::new(HostApprovalPolicy::Attended {
                capabilities,
                ui: (**ui).clone(),
            }),
        };
        let builder = builder.policy(policy);
        let builder = if let Some(interaction) = &self.interaction {
            builder
                .question_handler(interaction.clone())
                .sensitive_prompt_handler(interaction.clone())
        } else {
            builder
        };
        let harness = builder.build().await?;
        Ok(match resume {
            Some(id) => harness.resume_session(id).await?,
            None => harness.new_session().await?,
        })
    }
}

impl Launch {
    /// The most the session can hold. The terminal can switch between every mode; a
    /// new batch job keeps its own, and a resumed session is held to what it started
    /// with. Human interaction is a runtime fact, never configured.
    fn ceiling(&self, config: &Config, resumed: bool) -> CapabilitySet {
        let mut capabilities = match &self.permissions {
            Permissions::Mode(_) if resumed || self.interaction.is_some() => config.ceiling(),
            Permissions::Mode(mode) => {
                let mode = config.modes.get(mode);
                mode.map(|mode| mode.capabilities.iter().copied().collect())
                    .unwrap_or_else(CapabilitySet::empty)
            }
            Permissions::Exact(capabilities) => capabilities.clone(),
        };
        capabilities.remove(Capability::Interactive);
        if self.interaction.is_some() {
            capabilities.insert(Capability::Interactive);
        }
        capabilities
    }
}

/// Resolve exactly the configuration shared by startup and inspection.
pub async fn resolve_config(
    request: &ConfigRequest,
) -> Result<skyhook::config::ResolvedConfig, Box<dyn std::error::Error>> {
    // The core resolver owns ordering and workspace resolution, including its
    // diagnostics. Explicit files bypass workspace probing there entirely.
    let mut resolved = Config::resolve(&request.workspace, request.config.as_deref()).await?;
    resolved.config.approve_all |= request.approve_all;

    Ok(resolved)
}

pub async fn load_config(
    request: &ConfigRequest,
    display_diagnostics: bool,
) -> Result<RuntimeConfig, Box<dyn std::error::Error>> {
    let resolved = resolve_config(request).await?;
    if display_diagnostics {
        for diagnostic in &resolved.report.diagnostics {
            eprintln!(
                "skyhook config: {}",
                skyhook::tool::diagnostic::escape_controls(diagnostic)
            );
        }
    }
    Ok(resolved.config.into_runtime()?)
}

/// Select the explicit model, remembered model, or configured default for both hosts.
pub fn select_model(
    config: &RuntimeConfig,
    explicit: Option<&ModelRef>,
    saved: Option<&ModelRef>,
) -> Result<ConfiguredModel, LaunchError> {
    if let Some(name) = explicit {
        return Ok(config.select_model(name)?);
    }
    Ok(saved
        .and_then(|name| config.select_model(name).ok())
        .unwrap_or_else(|| config.default_model()))
}

impl Launch {
    pub async fn from_request(
        request: &cli::ExecutionRequest,
        model: ConfiguredModel,
        permissions: Permissions,
        interaction: Option<Arc<UiInteraction>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let request = &request.config;
        let workspace = tokio::fs::canonicalize(&request.workspace).await?;
        // CLI history belongs only to the selected workspace, never to an
        // inherited/global session_root or an ancestor workspace's history.
        let sessions = skyhook::config::workspace_session_root(&workspace);
        let approve_all = request.approve_all || model.config().config().approve_all;
        Ok(Self {
            model,
            permissions,
            workspace,
            sessions,
            approve_all,
            catalog: super::embedded_shims::catalog()?,
            interaction,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{self, Invocation};

    fn history_config(root: Option<PathBuf>) -> RuntimeConfig {
        let mut config = Config::from_yaml("providers:\n  test:\n    dialect: compatible\n    codec: chat_completions\n    base_url: http://127.0.0.1:1/v1\n    models:\n      test:\n        model: fixture\n        max_context: 128000\n        max_output: 4096\n").unwrap();
        config.session_root = root;
        config.into_runtime().unwrap()
    }

    /// Launch an interactive invocation for `workspace` with `config`'s first model.
    async fn launch(workspace: &std::path::Path, config: &RuntimeConfig) -> Launch {
        let args = ["skyhook", "--workspace", workspace.to_str().unwrap()];
        let Invocation::Interactive(request, _) = cli::parse_from(args).unwrap() else {
            panic!("interactive request")
        };
        let mode = Permissions::Mode(config.default_mode().to_owned());
        Launch::from_request(&request.execution, config.default_model(), mode, None)
            .await
            .unwrap()
    }

    #[test]
    fn selection_precedence_and_stale_memory_preserve_the_runtime_owner() {
        let original = history_config(None);
        let mut config = original.config().clone();
        let models = &mut config.providers["test"].common.models;
        let mut second = models["test"].clone();
        second.profile.model = "second-fixture".into();
        models.insert("another".parse().unwrap(), second);
        let config = config.into_runtime().unwrap();
        let name = |name: &str| name.parse::<ModelRef>().unwrap();
        for (explicit, saved, expected) in [
            (Some("test/test"), Some("test/another"), "test/test"),
            (None, Some("test/another"), "test/another"),
            (None, Some("test/removed"), "test/test"),
            (None, None, "test/test"),
        ] {
            let (explicit, saved) = (explicit.map(name), saved.map(name));
            let model = select_model(&config, explicit.as_ref(), saved.as_ref()).unwrap();
            assert_eq!(model.name(), name(expected));
            assert!(std::ptr::eq(model.config().config(), config.config()));
            let profile = config.model(&name(expected)).unwrap();
            assert!(std::ptr::eq(model.profile(), profile));
        }
        let explicit = name("test/removed");
        let saved = name("test/another");
        let Err(unknown) = select_model(&config, Some(&explicit), Some(&saved)) else {
            panic!("an explicit unknown model is rejected");
        };
        assert_eq!(
            unknown.to_string(),
            "invalid model `test/removed`: model is not configured"
        );

        // Persist names, not handles: the same name after reload belongs to the
        // newly admitted generation, even while an earlier selection is alive.
        let old = select_model(&original, None, None).unwrap();
        let mut reloaded = original.config().clone();
        reloaded.providers["test"].common.models["test"]
            .profile
            .model = "reloaded-fixture".into();
        let reloaded = reloaded.into_runtime().unwrap();
        let rebound = select_model(&reloaded, None, Some(&old.name())).unwrap();
        assert_eq!(old.profile().model, "fixture");
        assert_eq!(rebound.profile().model, "reloaded-fixture");
        assert!(!std::ptr::eq(
            old.config().config(),
            rebound.config().config()
        ));
    }

    #[tokio::test]
    async fn resume_rebinds_recorded_name_and_reports_a_missing_profile() {
        let root = tempfile::tempdir().unwrap();
        let config = history_config(None);
        let session = launch(root.path(), &config)
            .await
            .create(None)
            .await
            .unwrap();
        let id = session.id();
        session.shutdown().await.unwrap();
        drop(session);

        let mut reloaded = config.config().clone();
        let models = &mut reloaded.providers["test"].common.models;
        let profile = models.shift_remove("test").unwrap();
        models.insert("replacement".parse().unwrap(), profile);
        let launch = launch(root.path(), &reloaded.into_runtime().unwrap()).await;
        let Err(missing) = launch.create(Some(id)).await else {
            panic!("a missing recorded model is reported");
        };
        assert!(
            matches!(&missing, LaunchError::MissingModel(name) if name.to_string() == "test/test")
        );
        assert_eq!(
            missing.to_string(),
            "Model test/test is missing. Restore it in the configuration before resuming."
        );
        // A valid new selection remains usable; stale history is not silently
        // rebound to that unrelated model.
        launch.create(None).await.unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn history_root_is_selected_workspace_even_with_storage_override() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("project/nested");
        std::fs::create_dir_all(&workspace).unwrap();
        let parent = std::fs::canonicalize(workspace.parent().unwrap()).unwrap();
        for configured in [
            None,
            Some(root.path().join("shared-sessions")),
            Some(PathBuf::from("relative-sessions")),
        ] {
            let launch = launch(&workspace.join(".."), &history_config(configured)).await;
            // This is the same root passed to the TUI history loader.
            assert_eq!(launch.sessions, parent.join(".skyhook/sessions"));
            assert!(!launch.sessions.exists());
        }
    }

    #[tokio::test]
    async fn history_creation_and_resume_are_workspace_local_without_fallback() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let configured = root.path().join("shared-sessions");
        let mut outside_ids = vec![];
        for outside in [
            configured.clone(),
            root.path().join(".skyhook/sessions"),
            root.path().join("other/.skyhook/sessions"),
        ] {
            // Seed valid, resumable history, not merely empty session directories.
            let model = history_config(None).default_model();
            let builder = model
                .harness_builder(&workspace)
                .unwrap()
                .session_root(outside);
            let session = builder.build().await.unwrap().new_session().await.unwrap();
            outside_ids.push(session.id());
            session.shutdown().await.unwrap();
        }
        let launch = launch(&workspace, &history_config(Some(configured.clone()))).await;
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
                .join("session.db")
                .is_file()
        );
        assert!(!configured.join(id.to_string()).exists());
        session.shutdown().await.unwrap();
        drop(session);
        let resumed = launch.create(Some(id)).await.unwrap();
        assert_eq!(resumed.id(), id);
        resumed.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn ceiling_follows_the_host_and_the_permissions() {
        let root = tempfile::tempdir().unwrap();
        let mut config = history_config(None).config().clone();
        config.modes = Config::from_yaml("modes:\n  wide:\n    capabilities: [read, exec]\n  narrow:\n    capabilities: [read]\n  none:\n    capabilities: []").unwrap().modes;
        config.modes.shift_remove("general");
        config.default_mode = "wide".into();
        let config = config.into_runtime().unwrap();
        let mut launch = launch(root.path(), &config).await;
        assert!(matches!(&launch.permissions, Permissions::Mode(mode) if mode == "wide"));
        let (interaction, _prompts) = UiInteraction::new();
        for (permissions, interactive, expected) in [
            // The terminal can switch modes, so its ceiling is their union.
            (
                Permissions::Mode("none".into()),
                true,
                &[Capability::Read, Capability::Exec, Capability::Interactive][..],
            ),
            (
                Permissions::Mode("narrow".into()),
                false,
                &[Capability::Read],
            ),
            (Permissions::Mode("none".into()), false, &[]),
            (
                Permissions::Exact(
                    [Capability::Targets, Capability::Interactive]
                        .into_iter()
                        .collect(),
                ),
                false,
                &[Capability::Targets],
            ),
        ] {
            launch.permissions = permissions;
            launch.interaction = interactive.then(|| Arc::new(interaction.clone()));
            let ceiling = launch.ceiling(config.config(), false);
            assert!(ceiling.iter().eq(expected.iter().copied()), "{ceiling:?}");
        }
        // A resumed batch session may be in any mode; its journaled ceiling narrows this.
        launch.permissions = Permissions::Mode("none".into());
        let resumed = launch.ceiling(config.config(), true);
        assert!(resumed.iter().eq([Capability::Read, Capability::Exec]));
    }
}
