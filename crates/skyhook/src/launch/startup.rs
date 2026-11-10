//! What a front end does before its first session opens: select the model and permissions.
use super::request::{ExecutionRequest, PermissionRequest};
use super::{ConfiguredHost, Launch, LaunchError, Permissions};
use crate::config::workspace_session_root;
use crate::config::{ConfigError, ConfiguredModel, RuntimeConfig, mode::ModeName};
use crate::product::{Attendance, RootMode};
use crate::provider::model::ModelRef;
use crate::remote::EmbeddedShimCatalog;

/// Admit `host`'s configuration and start it for `request`: `permissions` name the root's
/// mode or a batch job's exact capabilities, and the front end's `remembered` model and mode
/// apply where nothing else selects one. Remote targets bootstrap with `catalog`.
pub fn prepare(
    host: ConfiguredHost,
    request: &ExecutionRequest,
    permissions: &PermissionRequest,
    (model, mode): (Option<&ModelRef>, Option<&ModeName>),
    catalog: EmbeddedShimCatalog,
) -> Result<Launch, LaunchError> {
    let ConfiguredHost {
        config,
        approve_all,
        identity,
        factory,
        warnings: _,
        workspace,
        attendance,
    } = host;
    let config = &config.into_runtime()?;
    let model = select_model(config, request.model.as_ref(), model)?;
    let resumed = request.resume.is_some();
    let permissions = match permissions {
        PermissionRequest::Mode(named) => {
            let context = (resumed, attendance, identity.root_mode);
            Permissions::Mode(select_mode(config, named.as_ref(), mode, context)?)
        }
        PermissionRequest::Exact(capabilities) => {
            Permissions::Exact(capabilities.iter().cloned().collect())
        }
    };
    Ok(Launch {
        identity,
        factory,
        approve_all,
        model,
        permissions,
        sessions: workspace_session_root(&workspace),
        workspace,
        catalog,
    })
}

/// Select the explicit model, remembered model, or configured default.
fn select_model(
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

/// The root's mode: one `named` on the command line, which a resumed session may know
/// though the configuration no longer has it (it is admitted when a message selects
/// it); else, at a terminal whose root can switch, the `remembered` one while it is
/// configured; else the configured default.
fn select_mode(
    config: &RuntimeConfig,
    named: Option<&ModeName>,
    remembered: Option<&ModeName>,
    (resumed, attendance, root_mode): (bool, Attendance, RootMode),
) -> Result<ModeName, ConfigError> {
    let remembered = match (attendance, root_mode) {
        (Attendance::Attended, RootMode::Switchable) => remembered,
        _ => None,
    };
    Ok(match named {
        Some(mode) if resumed => mode.clone(),
        Some(mode) => config.select_mode(Some(mode))?.clone(),
        None => (remembered.and_then(|mode| config.select_mode(Some(mode)).ok()))
            .unwrap_or_else(|| config.default_mode())
            .clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_prefers_explicit_then_remembered_then_default() {
        let second = "      second:\n        model: second-fixture\n        max_context: 128000\n        max_output: 4096\n";
        let yaml = format!("{}{second}", super::super::tests::CONFIG);
        let config = crate::config::Config::from_yaml(&yaml).unwrap();
        let config = config.into_runtime().unwrap();
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
                error: crate::config::SelectionError::UnknownModel,
                ..
            }))
        ));
    }

    /// The remembered mode applies only at a terminal whose root can switch; a named
    /// mode a resumed session may know is kept, and one a new session cannot have is
    /// refused.
    #[test]
    fn the_remembered_mode_applies_only_to_an_attended_switchable_root() {
        let mut config = super::super::tests::test_config();
        let live: ModeName = "live".parse().unwrap();
        config
            .modes
            .insert(live.clone(), config.modes["general"].clone());
        let config = config.into_runtime().unwrap();
        let default = config.default_mode().clone();
        let select = |named: Option<&ModeName>, resumed, attendance, root_mode| {
            select_mode(
                &config,
                named,
                Some(&live),
                (resumed, attendance, root_mode),
            )
        };
        use {Attendance::*, RootMode::*};
        assert_eq!(select(None, false, Attended, Switchable).unwrap(), live);
        assert_eq!(select(None, false, Attended, Fixed).unwrap(), default);
        assert_eq!(select(None, false, Batch, Switchable).unwrap(), default);
        let gone: ModeName = "gone".parse().unwrap();
        assert_eq!(select(Some(&gone), true, Batch, Switchable).unwrap(), gone);
        assert!(select(Some(&gone), false, Attended, Switchable).is_err());
    }
}
