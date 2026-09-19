use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    session::SessionStore,
    target::{
        SshOptions, TargetConfig, TargetDefinition, TargetRecord, TargetRouter, TargetSource,
    },
    tool::{
        RegistryError, ToolError, ToolOptions, ToolRegistryBuilder,
        policy::{Capability, PermissionUse, ResourceId},
    },
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
    #[serde(flatten)]
    config: TargetConfig,
}

#[derive(Serialize, JsonSchema)]
struct CompactTarget {
    name: String,
    r#type: crate::target::TargetType,
    host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace: Option<std::path::PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    via: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
}
impl From<TargetRecord> for CompactTarget {
    fn from(record: TargetRecord) -> Self {
        Self {
            name: record.name,
            r#type: record.r#type,
            host: record.host,
            workspace: (record.workspace != std::path::Path::new(".")).then_some(record.workspace),
            via: record.via,
            origin: record.origin,
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
        move |context, args| {
            let router = listed.clone();
            async move {
                let records = router.targets().list(context.capabilities()).await;
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
        r#"Add or replace a session target without connecting to it or changing your current target.

Example: `tool.target_add({name:"db", type:"ssh", host:"db.internal", via:"bastion"})`.
origin (default root) is the target whose shim starts SSH, using its key paths and agents. via jumps through a target with the same origin; both may be set.
ssh.options can run commands, so it also requires exec."#,
        ToolOptions::new(vec![Capability::Write])
            .requires(Capability::Targets)
            .permission_resource(ResourceId::session("targets"))
            .conditional_nested_input(
                "/$defs/SshOptions",
                "external_agent",
                Capability::SshAgent,
                serde_json::json!({
                    "type": "boolean",
                    "default": false,
                    "description": "Authenticate with, and forward, the agent the origin inherited in SSH_AUTH_SOCK instead of Skyhook's private agent; keys are never added to it. Adding the target and later connecting to it are approved separately."
                }),
            )
            .argument_permissions(|_, arguments| {
                let args: TargetAddArgs =
                    serde_json::from_value(arguments.clone()).map_err(ToolError::invalid)?;
                Ok(add_permissions(&args.config.ssh))
            }),
        move |context, args| {
            let router = router.clone();
            let store = store.clone();
            async move {
                let definition =
                    TargetDefinition::from_config(args.name, args.config, TargetSource::Session)
                        .map_err(ToolError::invalid)?;
                let added = router
                    .add(definition, context.invocation_subject()?, &store)
                    .await
                    .map_err(|e| e.into_tool_error())?;
                let record = TargetRecord::from(&added);
                Ok(record.into())
            }
        },
    )?;
    Ok(())
}

/// Options can run commands on the origin; an external agent exposes keys Skyhook does not own.
fn add_permissions(ssh: &SshOptions) -> Vec<PermissionUse> {
    let required = [
        (!ssh.options.is_empty(), Capability::Exec),
        (ssh.external_agent, Capability::SshAgent),
    ];
    (required.into_iter())
        .filter(|(required, _)| *required)
        .map(|(_, capability)| PermissionUse::new(capability, ResourceId::session("targets")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_requires_exec_for_options_and_ssh_agent_for_external_agents() {
        let capabilities = |ssh: serde_json::Value| {
            let ssh = serde_json::from_value(ssh).unwrap();
            let permissions = add_permissions(&ssh).into_iter();
            permissions
                .map(|permission| permission.capability)
                .collect::<Vec<_>>()
        };
        assert_eq!(capabilities(serde_json::json!({})), Vec::new());
        let both =
            serde_json::json!({"options": {"ServerAliveInterval": "15"}, "external_agent": true});
        assert_eq!(capabilities(both), [Capability::Exec, Capability::SshAgent]);
    }

    #[test]
    fn external_agent_is_absent_from_the_base_schema() {
        // The registry adds it back under this definition only with ssh_agent.
        let schema = serde_json::to_value(schemars::schema_for!(TargetAddArgs)).unwrap();
        let properties = &schema["$defs"]["SshOptions"]["properties"];
        assert!(properties.is_object(), "{schema}");
        assert!(properties.get("external_agent").is_none(), "{schema}");
    }

    #[test]
    fn add_accepts_nested_ssh_options() {
        let args: TargetAddArgs = serde_json::from_str(r#"{"name":"database","type":"ssh","host":"db","origin":"build","ssh":{"auth":{"kind":"agent"},"options":{"ServerAliveInterval":"15"}}}"#).unwrap();
        assert_eq!(args.config.origin.as_deref(), Some("build"));
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
