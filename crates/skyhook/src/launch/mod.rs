//! Opening a product's sessions for a front end, attended or not: the configuration it
//! hands in with the product's edit, the root's model and mode, the approval policy, and
//! the session.
pub mod config;
pub mod request;
pub mod startup;

use crate::product::{
    Attendance, Error, Identity, Opening, PreparedSession, Product, SessionContext,
    SessionExtension, SessionFactory,
};
use crate::provider::model::ModelRef;
use crate::tool::policy::{Capability, CapabilitySet};
use crate::{
    agent::{HarnessError, SessionHandle},
    config::{Config, ConfigError, ConfiguredModel, mode::ModeName},
    interaction::{UiInteraction, policy::HostApprovalPolicy},
    remote::EmbeddedShimCatalog,
    session::{SessionError, SessionStore},
    tool::{
        builtins::HostSkills,
        policy::{AllowAll, Policy},
    },
};
use config::HostConfig;
use request::ExecutionRequest;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// A configured mode, or the exact capabilities of a batch job that named none.
#[derive(Clone)]
enum Permissions {
    Mode(ModeName),
    Exact(CapabilitySet),
}

/// Who answers a session's approvals and questions: nobody in a batch job, or whoever
/// attends it, through a prompt channel of the session's own.
#[derive(Clone)]
pub enum Interface {
    Batch,
    Attended(Arc<UiInteraction>),
}

impl Interface {
    pub fn attendance(&self) -> Attendance {
        match self {
            Self::Batch => Attendance::Batch,
            Self::Attended(_) => Attendance::Attended,
        }
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
    Session(#[from] SessionError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Harness(#[from] HarnessError),
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

#[derive(Clone)]
pub struct Launch {
    identity: Identity,
    factory: Arc<dyn SessionFactory>,
    /// Approve every tool call without consulting the interactive policy.
    approve_all: bool,
    model: ConfiguredModel,
    permissions: Permissions,
    workspace: PathBuf,
    sessions: PathBuf,
    /// The shims remote targets are bootstrapped with.
    catalog: EmbeddedShimCatalog,
}
impl Launch {
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn factory(&self) -> &dyn SessionFactory {
        &*self.factory
    }

    pub fn model(&self) -> &ConfiguredModel {
        &self.model
    }

    /// The root's mode, unless it runs a batch job's exact capabilities.
    pub fn mode(&self) -> Option<&ModeName> {
        match &self.permissions {
            Permissions::Mode(mode) => Some(mode),
            Permissions::Exact(_) => None,
        }
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The workspace's session history.
    pub fn sessions(&self) -> &Path {
        &self.sessions
    }

    /// This launch running `model` of its own configuration, its root in `mode`.
    pub fn selecting(&self, model: &ModelRef, mode: ModeName) -> Result<Self, ConfigError> {
        Ok(Self {
            model: self.model.config().select_model(model)?,
            permissions: Permissions::Mode(mode),
            ..self.clone()
        })
    }

    pub async fn create(
        &self,
        opening: Opening<'_>,
        interface: &Interface,
    ) -> Result<OpenedSession, LaunchError> {
        let resume = match opening {
            Opening::New(_) => None,
            Opening::Resume(id) => Some(id),
        };
        let attendance = interface.attendance();
        let capabilities = self.ceiling(resume.is_some(), attendance);
        let builder = crate::agent::HarnessBuilder::configured(
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
            .shim_catalog(self.catalog.clone())
            .capabilities(capabilities.clone());
        let policy: Arc<dyn Policy> = match (interface, self.approve_all) {
            (_, true) => Arc::new(AllowAll),
            (Interface::Batch, false) => Arc::new(HostApprovalPolicy::Unattended),
            (Interface::Attended(ui), false) => {
                Arc::new(HostApprovalPolicy::Attended((**ui).clone()))
            }
        };
        let builder = builder.policy(policy);
        let builder = match interface {
            Interface::Attended(ui) => builder.question_handler(ui.clone()),
            Interface::Batch => builder,
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
            self.factory
                .prepare(builder, self.context(opening, attendance))
                .await?
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
    pub fn context<'a>(
        &'a self,
        opening: Opening<'a>,
        attendance: Attendance,
    ) -> SessionContext<'a> {
        SessionContext {
            workspace: &self.workspace,
            opening,
            attendance,
        }
    }

    /// The most the session can hold: every mode's capabilities when attended, as the
    /// terminal can switch between them, or resumed, as the session may run any (its
    /// journal holds it to what it started with); a new batch job keeps its own.
    /// Human interaction is a runtime fact, never configured.
    pub fn ceiling(&self, resumed: bool, attendance: Attendance) -> CapabilitySet {
        let config = self.model.config().config();
        let attended = attendance == Attendance::Attended;
        let mut capabilities = match &self.permissions {
            Permissions::Mode(_) if resumed || attended => config.ceiling(),
            Permissions::Mode(mode) => (config.modes.get(mode))
                .map_or_else(CapabilitySet::empty, |mode| mode.capabilities.set().clone()),
            Permissions::Exact(capabilities) => capabilities.clone(),
        };
        capabilities.remove(&Capability::Interactive);
        if attended {
            capabilities.insert(Capability::Interactive);
        }
        capabilities
    }
}

/// A product configured for one launch, attended or not, before its configuration is
/// admitted and its root selected.
pub struct ConfiguredHost {
    config: Config,
    approve_all: bool,
    pub identity: Identity,
    factory: Arc<dyn SessionFactory>,
    /// What the product warned of its configuration, for the host to show.
    pub warnings: Vec<String>,
    /// The canonical workspace.
    workspace: PathBuf,
    attendance: Attendance,
}

impl ConfiguredHost {
    /// The canonical workspace.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }
}

/// Configure `product` over `config`, the configuration its host read or built, for the
/// sessions `attendance` opens in `workspace` as `request` asks.
pub async fn configure<P: Product>(
    product: &P,
    options: P::Options,
    mut config: HostConfig,
    workspace: &Path,
    request: &ExecutionRequest,
    attendance: Attendance,
) -> Result<ConfiguredHost, LaunchError> {
    let workspace = tokio::fs::canonicalize(workspace)
        .await
        .map_err(LaunchError::Workspace)?;
    let context = SessionContext {
        workspace: &workspace,
        opening: request.resume.map_or(Opening::New(None), Opening::Resume),
        attendance,
    };
    let configured = product
        .configure(&mut config.config, options, context)
        .await?;
    Ok(ConfiguredHost {
        config: config.config,
        approve_all: config.approve_all,
        identity: product.identity(),
        factory: configured.factory,
        warnings: configured.warnings,
        workspace,
        attendance,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeConfig;
    use crate::product::{Configured, Skyhook};
    use request::{ExecutionRequest, PermissionRequest};

    /// A product whose configuration edit selects a mode of its own, `inspect`, and reports
    /// a warning carrying a control sequence.
    pub(super) struct Configuring;
    impl Product for Configuring {
        type Options = ();
        fn identity(&self) -> Identity {
            Skyhook.identity()
        }
        async fn configure(
            &self,
            config: &mut Config,
            (): (),
            context: SessionContext<'_>,
        ) -> Result<Configured, Error> {
            assert!(context.workspace.is_absolute());
            config.default_mode = "inspect".parse().unwrap();
            config
                .modes
                .insert(config.default_mode.clone(), config.modes["general"].clone());
            Ok(Configured {
                warnings: vec!["fixture warning\u{1b}[31m".into()],
                ..Configured::default()
            })
        }
    }

    /// A configuration with one model, `test/first`, on an unreachable server. Its models
    /// come last, so a test can list more after them.
    pub(super) const CONFIG: &str = "providers:\n  test:\n    dialect: compatible\n    codec: chat_completions\n    base_url: http://127.0.0.1:1\n    models:\n      first:\n        model: fixture\n        max_context: 128000\n        max_output: 4096\n";

    /// [`CONFIG`], parsed.
    pub(super) fn test_config() -> Config {
        Config::from_yaml(CONFIG).unwrap()
    }

    /// A new session's request for the configured default model.
    const NEW: ExecutionRequest = ExecutionRequest {
        resume: None,
        model: None,
    };

    /// Launch an interactive invocation for `workspace` with `config`'s first model.
    async fn launch(workspace: &Path, config: &RuntimeConfig) -> Launch {
        let host = ConfiguredHost {
            config: config.config().clone(),
            approve_all: false,
            identity: Skyhook.identity(),
            factory: Arc::new(()),
            warnings: Vec::new(),
            workspace: tokio::fs::canonicalize(workspace).await.unwrap(),
            attendance: Attendance::Attended,
        };
        let mode = PermissionRequest::Mode(None);
        startup::prepare(host, &NEW, &mode, (None, None), Default::default()).unwrap()
    }

    /// Sessions live under the selected workspace.
    #[tokio::test]
    async fn sessions_live_under_the_selected_workspace() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("project/nested");
        std::fs::create_dir_all(&workspace).unwrap();
        let parent = std::fs::canonicalize(workspace.parent().unwrap()).unwrap();
        let configured = test_config().into_runtime().unwrap();
        let launch = launch(&workspace.join(".."), &configured).await;
        // This is the same root passed to the TUI history loader.
        assert_eq!(launch.sessions, parent.join(".skyhook/sessions"));
        let session = launch
            .create(Opening::New(None), &Interface::Batch)
            .await
            .unwrap()
            .session;
        let id = session.id();
        assert!(
            launch
                .sessions
                .join(id.to_string())
                .join("session.db")
                .is_file()
        );
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn ceiling_follows_the_host_and_the_permissions() {
        let root = tempfile::tempdir().unwrap();
        let mut config = test_config();
        config.modes = crate::config::Config::from_yaml("modes:\n  wide:\n    capabilities: [read, exec]\n  narrow:\n    capabilities: [read]\n  none:\n    capabilities: []").unwrap().modes;
        config.modes.shift_remove("general");
        config.default_mode = "wide".parse().unwrap();
        let config = config.into_runtime().unwrap();
        let mut launch = launch(root.path(), &config).await;
        assert!(matches!(&launch.permissions, Permissions::Mode(mode) if mode.as_str() == "wide"));
        for (permissions, attendance, expected) in [
            // The terminal can switch modes, so its ceiling is their union.
            (
                Permissions::Mode("none".parse().unwrap()),
                Attendance::Attended,
                &[Capability::Read, Capability::Exec, Capability::Interactive][..],
            ),
            (
                Permissions::Mode("narrow".parse().unwrap()),
                Attendance::Batch,
                &[Capability::Read],
            ),
            (
                Permissions::Mode("none".parse().unwrap()),
                Attendance::Batch,
                &[],
            ),
            (
                Permissions::Exact(
                    [Capability::Targets, Capability::Interactive]
                        .into_iter()
                        .collect(),
                ),
                Attendance::Batch,
                &[Capability::Targets],
            ),
        ] {
            launch.permissions = permissions;
            let ceiling = launch.ceiling(false, attendance);
            assert!(ceiling.iter().eq(expected.iter()), "{ceiling:?}");
        }
        // A resumed batch session may be in any mode; its journaled ceiling narrows this.
        launch.permissions = Permissions::Mode("none".parse().unwrap());
        let resumed = launch.ceiling(true, Attendance::Batch);
        assert!(resumed.iter().eq(&[Capability::Read, Capability::Exec]));
    }

    /// A product edits the resolved configuration before it is admitted.
    #[tokio::test]
    async fn product_configuration_precedes_admission() {
        let root = tempfile::tempdir().unwrap();
        let mut config = test_config();
        config.default_mode = "unknown".parse().unwrap();
        assert!(config.clone().into_runtime().is_err());
        let config = HostConfig {
            approve_all: false,
            config,
        };
        let attendance = Attendance::Attended;
        let host = configure(&Configuring, (), config, root.path(), &NEW, attendance);
        let host = host.await.unwrap();
        assert_eq!(host.warnings, ["fixture warning\u{1b}[31m"]);
        let mode = PermissionRequest::Mode(None);
        let launch = startup::prepare(host, &NEW, &mode, (None, None), Default::default());
        assert_eq!(launch.unwrap().mode().unwrap().as_str(), "inspect");
    }
}
