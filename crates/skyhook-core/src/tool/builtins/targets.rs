use schemars::JsonSchema;
use serde::Deserialize;

use crate::{
    session::{SessionEvent, SessionStore},
    target::{TargetConfig, TargetDefinition, TargetRecord, TargetRouter, TargetSource},
    tool::{RegistryError, ToolError, ToolOptions, ToolRegistryBuilder, policy::Capability},
};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TargetsArgs {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TargetAddArgs {
    name: String,
    #[serde(flatten)]
    config: TargetConfig,
}

pub(super) fn register(
    builder: &mut ToolRegistryBuilder,
    store: SessionStore,
    router: TargetRouter,
) -> Result<(), RegistryError> {
    let listed = router.clone();
    builder.register::<TargetsArgs, Vec<TargetRecord>, _, _>(
        "targets",
        "List the built-in local `root` target and named SSH targets available in this session. Prefer named targets and target-aware tools’ target parameter over invoking ssh manually so Skyhook can apply configured authentication, jump routing, workspaces, cancellation, approvals, and audit metadata.",
        ToolOptions::default().requires(Capability::Targets),
        move |_context, _args| {
            let router = listed.clone();
            async move { Ok(router.targets().list().await) }
        },
    )?;
    builder.register::<TargetAddArgs, TargetRecord, _, _>(
        "target_add",
        "Add or replace a named SSH target for this session. This does not connect to it.",
        ToolOptions::new(vec![Capability::Write])
            .requires(Capability::Targets)
            .permission_resource(crate::tool::policy::ResourceId::session("targets")),
        move |context, args| {
            let router = router.clone();
            let store = store.clone();
            async move {
                let definition =
                    TargetDefinition::from_config(args.name, args.config, TargetSource::Session)
                        .map_err(target_error)?;
                let persisted = router.upsert(definition).await.map_err(target_error)?;
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

#[allow(clippy::needless_pass_by_value)]
fn target_error(error: impl ToString) -> ToolError {
    ToolError::InvalidArguments(error.to_string())
}
