use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    session::SessionStore,
    target::{TargetConfig, TargetDefinition, TargetRecord, TargetRouter, TargetSource},
    tool::{RegistryError, ToolError, ToolOptions, ToolRegistryBuilder, policy::Capability},
};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TargetsArgs {
    /// Include configuration and authentication metadata.
    #[serde(default)]
    details: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TargetAddArgs {
    /// Session-wide name to add or replace; root is reserved.
    name: String,
    /// Target that runs SSH and supplies configuration and key files; defaults to your current target.
    origin: Option<String>,
    #[serde(flatten)]
    config: TargetConfig,
}

#[derive(Serialize, JsonSchema)]
struct CompactTarget {
    name: String,
    r#type: crate::target::TargetType,
    host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace: Option<std::path::PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    via: Option<String>,
}
impl From<TargetRecord> for CompactTarget {
    fn from(record: TargetRecord) -> Self {
        Self {
            name: record.name,
            r#type: record.r#type,
            host: record.host,
            origin: (record.origin != "root").then_some(record.origin),
            workspace: (record.workspace != std::path::Path::new(".")).then_some(record.workspace),
            via: record.via,
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[serde(untagged)]
enum TargetView {
    Compact(CompactTarget),
    Detailed(TargetRecord),
}

pub(super) fn register(
    builder: &mut ToolRegistryBuilder,
    store: SessionStore,
    router: TargetRouter,
) -> Result<(), RegistryError> {
    let listed = router.clone();
    builder.register::<TargetsArgs, Vec<TargetView>, _, _>(
        "targets",
        "List available execution targets and their hostnames without connecting.",
        ToolOptions::default().requires(Capability::Targets),
        move |_context, args| {
            let router = listed.clone();
            async move {
                let records = router.targets().list().await;
                Ok(records
                    .into_iter()
                    .map(|record| {
                        if args.details {
                            TargetView::Detailed(record)
                        } else {
                            TargetView::Compact(record.into())
                        }
                    })
                    .collect())
            }
        },
    )?;
    builder.register::<TargetAddArgs, CompactTarget, _, _>(
        "target_add",
        r#"Add or replace a session target. Resolves configuration on origin and infers omitted routing; may connect to origin, but does not connect to the destination or change your current target.

Example: `tool.target_add({name:"db", type:"ssh", host:"db.internal"})`.
To use another machine's SSH configuration and credentials, set origin to its target name. via alone selects forwarding, not remote credential ownership."#,
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
                let name = definition.name.clone();
                let origin = args.origin.unwrap_or_else(|| context.caller_location.target.clone());
                let persisted = router.add(definition, origin, &context.authorization, &store).await.map_err(|e| e.into_tool_error())?;
                let record = TargetRecord::from(persisted.iter().find(|d| d.name == name).expect("added destination"));
                Ok(record.into())
            }
        },
    )?;
    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
fn target_error(error: impl ToString) -> ToolError {
    ToolError::InvalidArguments(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn add_accepts_explicit_origin_and_nested_ssh_options() {
        let args: TargetAddArgs = serde_json::from_str(r#"{"name":"database","type":"ssh","host":"db","origin":"build","ssh":{"auth":{"kind":"agent"}}}"#).unwrap();
        assert_eq!(args.origin.as_deref(), Some("build"));
        assert_eq!(args.config.ssh.auth, crate::target::TargetAuth::Agent);
        for invalid in [
            r#"{"name":"db","host":"db"}"#,
            r#"{"name":"db","type":"local","host":"db"}"#,
            r#"{"name":"db","type":"ssh","host":"db","target":"root"}"#,
        ] {
            assert!(
                serde_json::from_str::<TargetAddArgs>(invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }
}
