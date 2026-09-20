//! Session launch settings shared by terminal and headless hosts.
use super::{
    cli::{self, ConfigRequest},
    interaction::{HostApprovalPolicy, UiInteraction},
};
use skyhook::{
    agent::SessionHandle,
    bounded_io::BoundedReadError,
    config::{Config, ConfiguredModel, RuntimeConfig},
    identity::SessionId,
    media::{Attachment, Image, ImageFormat, MAX_IMAGE_BYTES},
    remote::EmbeddedShimCatalog,
    session::SessionStore,
    tool::policy::{AllowAll, Capability, CapabilitySet},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// Largest text file attached to a prompt; larger files are for the agent to read.
const MAX_TEXT_ATTACHMENT_BYTES: usize = 1_048_576;

/// Read a workspace file as a prompt attachment: an image when its bytes are a
/// supported image, otherwise UTF-8 text.
pub(crate) async fn read_attachment(workspace: &Path, path: &Path) -> Result<Attachment, String> {
    let path = tokio::fs::canonicalize(workspace.join(path))
        .await
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if !path.starts_with(workspace) {
        return Err("File reference leaves the workspace".into());
    }
    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|error| error.to_string())?;
    let bytes = skyhook::bounded_io::read_bounded(&mut file, MAX_IMAGE_BYTES as usize)
        .await
        .map_err(|error| match error {
            BoundedReadError::TooLarge { .. } => {
                "File is too large to attach; ask the agent to read it instead".to_owned()
            }
            error => error.to_string(),
        })?;
    if ImageFormat::sniff(&bytes).is_some() {
        let image = Image::new(bytes).map_err(|error| error.to_string())?;
        return Ok(Attachment::Image {
            file: Some(path),
            image,
        });
    }
    if bytes.len() > MAX_TEXT_ATTACHMENT_BYTES {
        return Err("File is larger than 1 MiB; ask the agent to read it instead".into());
    }
    let content =
        String::from_utf8(bytes).map_err(|_| "stream did not contain valid UTF-8".to_owned())?;
    Ok(Attachment::Text {
        file: Some(path),
        content,
    })
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
    pub async fn create(&self, resume: Option<SessionId>) -> Result<SessionHandle, String> {
        let mut model = self.model.clone();
        if let Some(id) = resume {
            let summary = SessionStore::summary(&self.sessions, id)
                .await
                .map_err(|e| e.to_string())?;
            if let Some(m) = summary.model {
                model = self.model.config().select_model(&m).map_err(|_| {
                    format!(
                        "Model profile {m} is missing. Restore it in the configuration before resuming."
                    )
                })?;
            }
        }
        let capabilities = self.ceiling(model.config().config(), resume.is_some());
        let builder = model
            .harness_builder(&self.workspace)
            .map_err(|e| e.to_string())?;
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

impl Launch {
    pub(crate) fn mode(&self) -> Option<&str> {
        match &self.permissions {
            Permissions::Mode(mode) => Some(mode),
            Permissions::Exact(_) => None,
        }
    }

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
                super::dump::diagnostic_text(diagnostic)
            );
        }
    }
    Ok(resolved.config.into_runtime()?)
}

/// Select the explicit model, remembered model, or first configured model for both hosts.
pub fn select_model(
    config: &RuntimeConfig,
    explicit: Option<&str>,
    saved: Option<&str>,
) -> Result<ConfiguredModel, String> {
    if let Some(name) = explicit {
        return config
            .select_model(name)
            .map_err(|_| format!("Unknown model profile: {name}"));
    }
    Ok(saved
        .and_then(|name| config.select_model(name).ok())
        .unwrap_or_else(|| config.first_model()))
}

impl Launch {
    pub async fn from_request(
        request: &cli::ExecutionRequest,
        model: ConfiguredModel,
        interaction: Option<Arc<UiInteraction>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let permissions = match &request.permissions {
            // A resumed session knows modes the configuration may no longer have; it
            // admits the name when a message selects it.
            cli::PermissionArgs::Mode(Some(mode)) if request.resume.is_some() => {
                Permissions::Mode(mode.clone())
            }
            cli::PermissionArgs::Mode(mode) => {
                Permissions::Mode(model.config().select_mode(mode.as_deref())?.to_owned())
            }
            cli::PermissionArgs::Exact(capabilities) => {
                Permissions::Exact(capabilities.iter().copied().collect())
            }
        };
        let request = &request.config;
        let workspace = tokio::fs::canonicalize(&request.workspace).await?;
        // CLI history belongs only to the selected workspace, never to an
        // inherited/global session_root or an ancestor workspace's history.
        let sessions = workspace.join(".skyhook/sessions");
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
        let mut config: Config = toml::from_str("[providers.test]\nkind='openai'\napi='chat_completions'\nbase_url='http://127.0.0.1:1/v1'\n[models.test]\nprovider='test'\nmodel='fixture'\nmax_context=128000\nmax_output=4096\n").unwrap();
        config.session_root = root;
        config.into_runtime().unwrap()
    }

    /// Launch an interactive invocation for `workspace` with `config`'s first model.
    async fn launch(workspace: &std::path::Path, config: &RuntimeConfig) -> Launch {
        let args = ["skyhook", "--workspace", workspace.to_str().unwrap()];
        let Invocation::Interactive(request, _) = cli::parse_from(args).unwrap() else {
            panic!("interactive request")
        };
        Launch::from_request(&request, config.first_model(), None)
            .await
            .unwrap()
    }

    #[test]
    fn selection_precedence_and_stale_memory_preserve_the_runtime_owner() {
        let original = history_config(None);
        let mut config = original.config().clone();
        let mut second = config.models["test"].clone();
        second.model = "second-fixture".into();
        config.models.insert("another".into(), second);
        let config = config.into_runtime().unwrap();
        for (explicit, saved, expected) in [
            (Some("test"), Some("another"), "test"),
            (None, Some("another"), "another"),
            (None, Some("removed"), "test"),
            (None, None, "test"),
        ] {
            let model = select_model(&config, explicit, saved).unwrap();
            assert_eq!(model.name(), expected);
            assert!(std::ptr::eq(model.config().config(), config.config()));
            assert!(std::ptr::eq(
                model.profile(),
                &config.config().models[expected]
            ));
        }
        let unknown = select_model(&config, Some("removed"), Some("another")).err();
        assert_eq!(unknown.unwrap(), "Unknown model profile: removed");

        // Persist names, not handles: the same name after reload belongs to the
        // newly admitted generation, even while an earlier selection is alive.
        let old = select_model(&original, None, None).unwrap();
        let mut reloaded = original.config().clone();
        reloaded.models["test"].model = "reloaded-fixture".into();
        let reloaded = reloaded.into_runtime().unwrap();
        let rebound = select_model(&reloaded, None, Some(old.name())).unwrap();
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
        let profile = reloaded.models.shift_remove("test").unwrap();
        reloaded.models.insert("replacement".into(), profile);
        let launch = launch(root.path(), &reloaded.into_runtime().unwrap()).await;
        assert_eq!(
            launch.create(Some(id)).await.err().unwrap(),
            "Model profile test is missing. Restore it in the configuration before resuming."
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
            let model = history_config(None).first_model();
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
        config.modes = toml::from_str("[wide]\ncapabilities = ['read', 'exec']\n[narrow]\ncapabilities = ['read']\n[none]\ncapabilities = []").unwrap();
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
