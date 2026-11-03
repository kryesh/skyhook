//! Session launch settings shared by terminal and headless hosts.
use super::{
    Error, Identity, Opening, PreparedSession, Product, RootMode, SessionContext, SessionExtension,
    SessionFactory,
    args::{self, ConfigRequest},
    config::HostConfig,
    interaction::{HostApprovalPolicy, UiInteraction},
};
use skyhook::{
    agent::{HarnessError, SessionHandle},
    config::{ConfigError, ConfiguredModel, ResolvedConfig, RuntimeConfig, resolve_layers},
    fs::RegularFileError,
    media::{Attachment, Classified, MAX_IMAGE_BYTES, MAX_TEXT_BYTES, MediaError, classify},
    provider::profile::ModelRef,
    remote::{ArtifactError, EmbeddedShimCatalog},
    session::{SessionError, SessionStore},
    tool::builtins::HostSkills,
    tool::policy::{AllowAll, Capability, CapabilitySet, ModeName, Policy},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// Why a file could not be attached to a prompt.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AttachmentError {
    #[error("{0}: {1}")]
    Read(PathBuf, #[source] RegularFileError),
    #[error("File reference leaves the workspace")]
    OutsideWorkspace,
    #[error("File is too large to attach; ask the agent to read it instead")]
    TooLarge,
    #[error("File is larger than {} MiB; ask the agent to read it instead", MAX_TEXT_BYTES >> 20)]
    TextTooLarge,
    #[error("{0} is neither a supported image nor UTF-8 text")]
    Unsupported(PathBuf),
    #[error("{0} is not a supported image")]
    NotImage(PathBuf),
    #[error("{0}: {1}")]
    InvalidImage(PathBuf, #[source] MediaError),
}

/// Read a workspace file as a prompt attachment: an image when its bytes are a
/// supported image, otherwise UTF-8 text.
pub(crate) async fn read_attachment(
    workspace: &Path,
    path: &Path,
) -> Result<Attachment, AttachmentError> {
    let path = tokio::fs::canonicalize(workspace.join(path))
        .await
        .map_err(|error| AttachmentError::Read(path.to_owned(), error.into()))?;
    if !path.starts_with(workspace) {
        return Err(AttachmentError::OutsideWorkspace);
    }
    let bytes = skyhook::fs::read_regular(&path, MAX_IMAGE_BYTES)
        .await
        .map_err(|error| match error {
            RegularFileError::TooLarge { .. } => AttachmentError::TooLarge,
            error => AttachmentError::Read(path.clone(), error),
        })?;
    let classified = classify(bytes)
        .await
        .map_err(|error| AttachmentError::InvalidImage(path.clone(), error))?;
    match classified {
        Classified::Image(image) => Ok(Attachment::Image {
            file: Some(path),
            image,
        }),
        Classified::Text(content) if content.len() as u64 > MAX_TEXT_BYTES => {
            Err(AttachmentError::TextTooLarge)
        }
        Classified::Text(content) => Ok(Attachment::Text {
            file: Some(path),
            content,
        }),
        Classified::Binary(_) => Err(AttachmentError::Unsupported(path)),
    }
}

/// Why a submitted prompt or script failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OperationError {
    #[error(transparent)]
    Product(#[from] Error),
    #[error(transparent)]
    Harness(#[from] HarnessError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Attachment(#[from] AttachmentError),
}

impl OperationError {
    /// Whether the session's journal failed, which stopped the session.
    pub(crate) fn journal_failed(&self) -> bool {
        matches!(self, Self::Harness(error) if error.journal_failed())
    }
}

/// Read the images named on the command line.
pub(crate) async fn read_images(
    workspace: &Path,
    paths: &[PathBuf],
) -> Result<Vec<Attachment>, AttachmentError> {
    let mut images = Vec::with_capacity(paths.len());
    for path in paths {
        match read_attachment(workspace, path).await? {
            image @ Attachment::Image { .. } => images.push(image),
            Attachment::Text { .. } => return Err(AttachmentError::NotImage(path.clone())),
        }
    }
    Ok(images)
}

/// A configured mode, or the exact capabilities of a batch job that named none.
#[derive(Clone)]
pub(crate) enum Permissions {
    Mode(ModeName),
    Exact(CapabilitySet),
}

impl Permissions {
    /// A batch job's permissions. A resumed session knows modes the configuration
    /// may no longer have; it admits the name when a message selects it.
    pub(crate) fn for_batch(
        args: &args::PermissionArgs,
        resumed: bool,
        config: &RuntimeConfig,
    ) -> Result<Self, ConfigError> {
        Ok(match args {
            args::PermissionArgs::Mode(Some(mode)) if resumed => Self::Mode(mode.clone()),
            args::PermissionArgs::Mode(mode) => {
                Self::Mode(config.select_mode(mode.as_ref())?.clone())
            }
            args::PermissionArgs::Exact(capabilities) => {
                Self::Exact(capabilities.iter().cloned().collect())
            }
        })
    }
}

/// Why a session could not be created or resumed.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error(transparent)]
    Product(#[from] Error),
    #[error("workspace: {0}")]
    Workspace(#[source] std::io::Error),
    #[error(transparent)]
    Shims(#[from] ArtifactError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Harness(#[from] HarnessError),
    #[error(
        "{product} always runs its root agent in mode `{mode}`; --mode and batch capabilities cannot change it"
    )]
    RootModeFixed { product: String, mode: ModeName },
}
impl LaunchError {
    /// Another handle, in this process or another, holds the session's lock.
    pub fn already_open(&self) -> bool {
        matches!(
            self,
            Self::Session(SessionError::AlreadyOpen(_))
                | Self::Harness(HarnessError::Session(SessionError::AlreadyOpen(_)))
        )
    }
}

/// Refuse a root mode selection a product does not allow.
pub(crate) fn admit_root_mode(
    identity: &Identity,
    config: &RuntimeConfig,
    selected: bool,
) -> Result<(), LaunchError> {
    match identity.root_mode {
        RootMode::Fixed if selected => Err(LaunchError::RootModeFixed {
            product: identity.name().to_owned(),
            mode: config.default_mode().clone(),
        }),
        _ => Ok(()),
    }
}

#[derive(Clone)]
pub struct Launch {
    pub identity: Identity,
    pub factory: Arc<dyn SessionFactory>,
    pub state: PathBuf,
    pub headless: bool,
    /// Approve every tool call without consulting the interactive policy.
    pub approve_all: bool,
    pub model: ConfiguredModel,
    pub(crate) permissions: Permissions,
    pub workspace: PathBuf,
    pub sessions: PathBuf,
    pub(crate) catalog: EmbeddedShimCatalog,
    pub(crate) interaction: Option<Arc<UiInteraction>>,
}
impl Launch {
    pub async fn create(&self, opening: Opening<'_>) -> Result<OpenedSession, LaunchError> {
        let resume = match opening {
            Opening::New(_) => None,
            Opening::Resume(id) => Some(id),
        };
        let capabilities = self.ceiling(resume.is_some() || self.interaction.is_some());
        let builder = skyhook::agent::HarnessBuilder::configured(
            &self.model,
            &self.workspace,
            self.identity.host.clone(),
        )?
        .host_capabilities(self.identity.capabilities.iter().cloned())
        .session_schema(self.identity.schema)
        .skills(HostSkills::discover(&self.workspace).await);
        let builder = match &self.permissions {
            // A resumed session continues in its own mode, which may not be configured.
            Permissions::Mode(_) if resume.is_some() => builder,
            Permissions::Mode(mode) => builder.mode(mode.clone()),
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
            (Some(ui), false) => Arc::new(HostApprovalPolicy::Attended((**ui).clone())),
        };
        let builder = builder.policy(policy);
        let builder = if let Some(interaction) = &self.interaction {
            builder
                .question_handler(interaction.clone())
                .sensitive_prompt_handler(interaction.clone())
        } else {
            builder
        };
        // Another host's session opens view-only, without this product's resources; one
        // of this product's own that it cannot open is refused before any are prepared.
        let foreign = match resume {
            Some(id) => {
                let summary =
                    SessionStore::summary(&self.sessions, id, Some(self.identity.owner()));
                summary.await?.host != self.identity.host
            }
            None => false,
        };
        let prepared = if foreign {
            PreparedSession {
                builder,
                extension: Arc::new(()),
            }
        } else {
            self.factory.prepare(builder, self.context(opening)).await?
        };
        let harness = prepared.builder.build().await?;
        let session = match resume {
            Some(id) => harness.resume_session(id).await?,
            None => harness.new_session().await?,
        };
        Ok(OpenedSession {
            session,
            extension: prepared.extension,
        })
    }
}

pub struct OpenedSession {
    pub session: SessionHandle,
    pub extension: Arc<dyn SessionExtension>,
}

impl Launch {
    pub fn context<'a>(&'a self, opening: Opening<'a>) -> SessionContext<'a> {
        SessionContext {
            workspace: &self.workspace,
            opening,
            headless: self.headless,
        }
    }

    /// The most the session can hold: in `any_mode`, every mode's capabilities, as
    /// the terminal can switch between them and a resumed session may run any (its
    /// journal holds it to what it started with); a new batch job keeps its own.
    /// Human interaction is a runtime fact, never configured.
    pub(crate) fn ceiling(&self, any_mode: bool) -> CapabilitySet {
        let config = self.model.config().config();
        let mut capabilities = match &self.permissions {
            Permissions::Mode(_) if any_mode => config.ceiling(),
            Permissions::Mode(mode) => (config.modes.get(mode))
                .map_or_else(CapabilitySet::empty, |mode| mode.capabilities.set().clone()),
            Permissions::Exact(capabilities) => capabilities.clone(),
        };
        capabilities.remove(&Capability::Interactive);
        if self.interaction.is_some() {
            capabilities.insert(Capability::Interactive);
        }
        capabilities
    }
}

/// Resolve exactly the configuration shared by startup and inspection.
pub async fn resolve_config(
    request: &ConfigRequest,
) -> Result<ResolvedConfig<HostConfig>, ConfigError> {
    // The core resolver owns ordering and workspace resolution, including its
    // diagnostics. Explicit files bypass workspace probing there entirely.
    let mut resolved =
        resolve_layers::<HostConfig>(&request.source.workspace, request.source.config.as_deref())
            .await?;
    resolved.config.approve_all |= request.approve_all;
    Ok(resolved)
}

/// The main configuration with the product's edits applied, and its loader report
/// extended with the product's own.
pub async fn configure_product<P: Product>(
    product: &P,
    args: P::Args,
    request: &ConfigRequest,
    context: SessionContext<'_>,
) -> Result<(ResolvedConfig<HostConfig>, Arc<dyn SessionFactory>), LaunchError> {
    let mut resolved = resolve_config(request).await?;
    let configured = product
        .configure(&mut resolved.config.config, args, context)
        .await?;
    resolved.report.sources.extend(configured.report.sources);
    resolved
        .report
        .diagnostics
        .extend(configured.report.diagnostics);
    Ok((resolved, configured.factory))
}

pub struct ConfiguredHost {
    pub config: RuntimeConfig,
    pub approve_all: bool,
    pub identity: Identity,
    pub factory: Arc<dyn SessionFactory>,
    pub headless: bool,
    /// The canonical workspace.
    pub workspace: PathBuf,
    /// This product's UI preferences for the workspace.
    pub state: PathBuf,
}

pub async fn configure<P: Product>(
    product: &P,
    args: P::Args,
    request: &args::ExecutionRequest,
    headless: bool,
) -> Result<ConfiguredHost, LaunchError> {
    let identity = product.identity();
    let workspace = tokio::fs::canonicalize(&request.config.source.workspace)
        .await
        .map_err(LaunchError::Workspace)?;
    let context = SessionContext {
        workspace: &workspace,
        opening: request.resume.map_or(Opening::New(None), Opening::Resume),
        headless,
    };
    let (resolved, factory) = configure_product(product, args, &request.config, context).await?;
    if !headless {
        for diagnostic in &resolved.report.diagnostics {
            eprintln!(
                "{} config: {}",
                identity.name(),
                skyhook::tool::diagnostic::escape_controls(diagnostic)
            );
        }
    }
    Ok(ConfiguredHost {
        config: resolved.config.config.into_runtime()?,
        approve_all: resolved.config.approve_all,
        state: super::state::state_path(&workspace, &identity),
        identity,
        factory,
        headless,
        workspace,
    })
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
    pub fn new(
        model: ConfiguredModel,
        permissions: Permissions,
        interaction: Option<Arc<UiInteraction>>,
        host: ConfiguredHost,
    ) -> Result<Self, LaunchError> {
        // CLI history belongs only to the selected workspace, never to an
        // inherited/global session_root or an ancestor workspace's history.
        let sessions = skyhook::config::workspace_session_root(&host.workspace);
        Ok(Self {
            identity: host.identity,
            factory: host.factory,
            state: host.state,
            headless: host.headless,
            approve_all: host.approve_all,
            model,
            permissions,
            workspace: host.workspace,
            sessions,
            catalog: super::embedded_shims::catalog()?,
            interaction,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{self, Invocation};
    use std::assert_matches;

    fn history_config(root: Option<PathBuf>) -> RuntimeConfig {
        let mut config = crate::interaction::tests::test_config();
        config.session_root = root;
        config.into_runtime().unwrap()
    }

    /// Launch an interactive invocation for `workspace` with `config`'s first model.
    async fn launch(workspace: &std::path::Path, config: &RuntimeConfig) -> Launch {
        let identity = super::super::Product::identity(&super::super::Skyhook);
        let workspace = tokio::fs::canonicalize(workspace).await.unwrap();
        let mode = Permissions::Mode(config.default_mode().clone());
        Launch::new(
            config.default_model(),
            mode,
            None,
            ConfiguredHost {
                config: config.clone(),
                approve_all: false,
                state: super::super::state::state_path(&workspace, &identity),
                identity,
                factory: Arc::new(()),
                headless: false,
                workspace,
            },
        )
        .unwrap()
    }

    #[test]
    fn selection_prefers_explicit_then_remembered_then_default() {
        let config = crate::interaction::tests::two_models()
            .into_runtime()
            .unwrap();
        let name = |name: &str| name.parse::<ModelRef>().unwrap();
        for (explicit, saved, expected) in [
            (Some("test/first"), Some("test/second"), "test/first"),
            (None, Some("test/second"), "test/second"),
            (None, Some("test/removed"), "test/first"),
            (None, None, "test/first"),
        ] {
            let (explicit, saved) = (explicit.map(name), saved.map(name));
            let model = select_model(&config, explicit.as_ref(), saved.as_ref()).unwrap();
            assert_eq!(model.name(), name(expected));
        }
        let explicit = name("test/removed");
        let saved = name("test/second");
        let unknown = select_model(&config, Some(&explicit), Some(&saved));
        assert!(matches!(
            unknown,
            Err(LaunchError::Config(ConfigError::Model {
                error: skyhook::config::SelectionError::UnknownModel,
                ..
            }))
        ));
    }

    /// Sessions live under the selected workspace whatever the storage setting, and
    /// resuming never falls back to another root holding valid history.
    #[tokio::test]
    async fn history_is_workspace_local_without_fallback() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("project/nested");
        std::fs::create_dir_all(&workspace).unwrap();
        let parent = std::fs::canonicalize(workspace.parent().unwrap()).unwrap();
        let configured = root.path().join("shared-sessions");
        for storage in [
            None,
            Some(configured.clone()),
            Some(PathBuf::from("relative-sessions")),
        ] {
            let launch = launch(&workspace.join(".."), &history_config(storage)).await;
            // This is the same root passed to the TUI history loader.
            assert_eq!(launch.sessions, parent.join(".skyhook/sessions"));
            assert!(!launch.sessions.exists());
        }
        // Seed valid, resumable history, not merely an empty session directory.
        let model = history_config(None).default_model();
        let builder =
            skyhook::agent::HarnessBuilder::configured(&model, &workspace, "test".parse().unwrap())
                .unwrap();
        let builder = builder.session_root(configured.clone());
        let outside = builder.build().await.unwrap().new_session().await.unwrap();
        let outside_id = outside.id();
        outside.shutdown().await.unwrap();
        let launch = launch(&workspace, &history_config(Some(configured.clone()))).await;
        assert!(launch.create(Opening::Resume(outside_id)).await.is_err());
        assert!(!launch.sessions.exists());

        let session = launch.create(Opening::New(None)).await.unwrap().session;
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
    }

    #[tokio::test]
    async fn ceiling_follows_the_host_and_the_permissions() {
        let root = tempfile::tempdir().unwrap();
        let mut config = history_config(None).config().clone();
        config.modes = skyhook::config::Config::from_yaml("modes:\n  wide:\n    capabilities: [read, exec]\n  narrow:\n    capabilities: [read]\n  none:\n    capabilities: []").unwrap().modes;
        config.modes.shift_remove("general");
        config.default_mode = "wide".parse().unwrap();
        let config = config.into_runtime().unwrap();
        let mut launch = launch(root.path(), &config).await;
        assert!(matches!(&launch.permissions, Permissions::Mode(mode) if mode.as_str() == "wide"));
        let (interaction, _prompts) = UiInteraction::new();
        for (permissions, interactive, expected) in [
            // The terminal can switch modes, so its ceiling is their union.
            (
                Permissions::Mode("none".parse().unwrap()),
                true,
                &[Capability::Read, Capability::Exec, Capability::Interactive][..],
            ),
            (
                Permissions::Mode("narrow".parse().unwrap()),
                false,
                &[Capability::Read],
            ),
            (Permissions::Mode("none".parse().unwrap()), false, &[]),
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
            let ceiling = launch.ceiling(interactive);
            assert!(ceiling.iter().eq(expected.iter()), "{ceiling:?}");
        }
        // A resumed batch session may be in any mode; its journaled ceiling narrows this.
        launch.permissions = Permissions::Mode("none".parse().unwrap());
        let resumed = launch.ceiling(true);
        assert!(resumed.iter().eq(&[Capability::Read, Capability::Exec]));
    }

    /// A product with a fixed root refuses a selected root mode.
    #[test]
    fn a_fixed_root_refuses_a_selected_mode() {
        let config = history_config(None);
        let fixed = Identity {
            root_mode: RootMode::Fixed,
            ..super::super::Product::identity(&super::super::Skyhook)
        };
        assert_matches!(
            admit_root_mode(&fixed, &config, true),
            Err(LaunchError::RootModeFixed { .. })
        );
        assert!(admit_root_mode(&fixed, &config, false).is_ok());
    }

    /// A product edits the resolved configuration before it is admitted.
    #[tokio::test]
    async fn product_configuration_precedes_admission() {
        use crate::tests::Configuring;
        let root = tempfile::tempdir().unwrap();
        let mut config = history_config(None).config().clone();
        config.default_mode = "unknown".parse().unwrap();
        assert!(config.clone().into_runtime().is_err());
        let path = root.path().join("config.yaml");
        std::fs::write(&path, config.to_yaml().unwrap()).unwrap();
        let invocation = args::parse_product(
            &Configuring,
            [
                "skyhook",
                "--workspace",
                root.path().to_str().unwrap(),
                "--config",
                path.to_str().unwrap(),
            ],
        )
        .unwrap();
        let Invocation::Interactive(request, _, args) = invocation else {
            panic!("interactive")
        };
        let host = configure(&Configuring, args, &request.execution, false)
            .await
            .unwrap();
        assert_eq!(host.config.default_mode().as_str(), "inspect");
        assert_eq!(host.state, root.path().join(".skyhook/state.json"));
    }
}
