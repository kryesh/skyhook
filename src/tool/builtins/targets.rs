use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    remote::RemoteManager,
    session::{SessionEvent, SessionStore},
    target::{
        TargetAuth, TargetConfig, TargetDefinition, TargetRecord, TargetRegistry, TargetSource,
    },
    tool::{RegistryError, ToolError, ToolRegistryBuilder, policy::ToolEffect},
};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TargetsArgs {}

#[derive(Serialize)]
struct TargetsOutput {
    targets: Vec<TargetRecord>,
}

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
    builder.register::<TargetsArgs, TargetsOutput, _, _>(
        "targets",
        "List the local root and named SSH targets available in this session.",
        vec![ToolEffect::SessionState],
        false,
        false,
        move |_context, _args| {
            let targets = listed.clone();
            async move {
                Ok(TargetsOutput {
                    targets: targets.list().await,
                })
            }
        },
    )?;
    builder.register::<TargetAddArgs, TargetRecord, _, _>(
        "target_add",
        "Add or replace a named SSH target for this session. This does not connect to it.",
        vec![ToolEffect::ManageTargets, ToolEffect::SessionState],
        false,
        false,
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
