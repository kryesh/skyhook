use std::path::PathBuf;

use schemars::JsonSchema;
use serde::Deserialize;

use crate::{
    remote::RemoteManager,
    session::{SessionEvent, SessionStore},
    target::{
        TargetAuth, TargetConfig, TargetDefinition, TargetRecord, TargetRegistry, TargetSource,
    },
    tool::{RegistryError, ToolError, ToolOptions, ToolRegistryBuilder, policy::ToolEffect},
};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TargetsArgs {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TargetAddArgs {
    name: String,
    host: String,
    user: Option<String>,
    port: Option<u16>,
    #[serde(default = "default_workspace")]
    workspace: PathBuf,
    via: Option<String>,
    #[serde(default)]
    auth: TargetAuth,
}

pub(super) fn register(
    builder: &mut ToolRegistryBuilder,
    store: SessionStore,
    targets: TargetRegistry,
    remote: RemoteManager,
) -> Result<(), RegistryError> {
    let listed = targets.clone();
    builder.register::<TargetsArgs, Vec<TargetRecord>, _, _>(
        "targets",
        "List the local root and named SSH targets available in this session.",
        ToolOptions::new(vec![ToolEffect::SessionState]),
        move |_context, _args| {
            let targets = listed.clone();
            async move { Ok(targets.list().await) }
        },
    )?;
    builder.register::<TargetAddArgs, TargetRecord, _, _>(
        "target_add",
        "Add or replace a named SSH target for this session. This does not connect to it.",
        ToolOptions::new(vec![ToolEffect::ManageTargets, ToolEffect::SessionState]),
        move |context, args| {
            let targets = targets.clone();
            let store = store.clone();
            let remote = remote.clone();
            async move {
                let definition = TargetDefinition::from_config(
                    args.name,
                    TargetConfig {
                        host: args.host,
                        user: args.user,
                        port: args.port,
                        workspace: args.workspace,
                        via: args.via,
                        auth: args.auth,
                    },
                    TargetSource::Session,
                )
                .map_err(target_error)?;
                let invalidated = targets
                    .upsert(definition.clone())
                    .await
                    .map_err(target_error)?;
                remote.invalidate(&invalidated).await;
                let persisted = targets.get(&definition.name).await.map_err(target_error)?;
                store
                    .append(
                        context.agent,
                        SessionEvent::TargetUpserted {
                            target: persisted.clone(),
                        },
                    )
                    .await
                    .map_err(|error| ToolError::Failed(error.to_string()))?;
                Ok(TargetRecord::from(&persisted))
            }
        },
    )?;
    Ok(())
}

fn default_workspace() -> PathBuf {
    PathBuf::from(".")
}

#[allow(clippy::needless_pass_by_value)]
fn target_error(error: impl ToString) -> ToolError {
    ToolError::InvalidArguments(error.to_string())
}
