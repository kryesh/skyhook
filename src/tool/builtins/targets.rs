use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    session::SessionStore,
    target::{
        SshOptions, TargetConfig, TargetDefinition, TargetRecord, TargetRouter, TargetSource,
    },
    tool::{
        RegistryError, ToolError, ToolOptions, ToolRegistryBuilder,
        diagnostic::{Effects, deserialize_arguments},
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
    workspace: Option<std::path::PathBuf>,
    via: Option<String>,
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
                    deserialize_arguments(arguments.clone()).map_err(|error| {
                        error.effects(Effects::Unchanged)
                    })?;
                Ok(add_permissions(&args.config.ssh))
            }),
        move |context, args| {
            let router = router.clone();
            let store = store.clone();
            async move {
                let definition =
                    TargetDefinition::from_config(args.name, args.config, TargetSource::Session)
                        .map_err(|error| {
                            ToolError::from(error.into_admission_error())
                                .effects(Effects::Unchanged)
                        })?;
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

    #[tokio::test]
    async fn saved_target_failures_hide_submitted_aliases_from_reduced_capabilities() {
        use crate::{
            remote::{
                EmbeddedShimCatalog, PendingHandshakeFactory, RejectSensitivePrompts, RemoteManager,
            },
            target::TargetRegistry,
            tests::TestRuntime,
            tool::{
                authorization::AuthorizationCoordinator,
                diagnostic::{FailureSite, Subject},
                policy::{AllowAll, CapabilitySet},
            },
        };
        use std::sync::{Arc, atomic::Ordering};

        let runtime = TestRuntime::new().await;
        let authorization = AuthorizationCoordinator::new(Arc::new(AllowAll));
        let factory = PendingHandshakeFactory::new();
        let remote = RemoteManager::new(
            EmbeddedShimCatalog::default(),
            Arc::new(RejectSensitivePrompts),
            authorization.clone(),
        )
        .with_connection_factory(factory.clone());
        let router = TargetRouter::new(TargetRegistry::default(), remote, authorization);
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, runtime.store.clone(), router.clone()).unwrap();
        let mut privileged = CapabilitySet::default();
        privileged.insert(Capability::Targets);
        let executor = runtime
            .executor(builder)
            .with_capabilities(privileged.clone());
        let mut reduced = privileged.clone();
        reduced.remove(Capability::Targets);
        for (arguments, field) in [
            (
                serde_json::json!({"name":"private requested","type":"ssh","host":"host"}),
                "name",
            ),
            (
                serde_json::json!({"name":"private-requested","type":"ssh","host":"host","via":"private-jump"}),
                "via",
            ),
            (
                serde_json::json!({"name":"private-requested","type":"ssh","host":"host","origin":"private-origin"}),
                "origin",
            ),
        ] {
            let result = executor
                .run_model(&runtime.agent, "target_add", arguments)
                .await
                .unwrap();
            assert!(result.is_error);
            let envelope = runtime.jobs.snapshot(result.job).await.unwrap();
            let diagnostic = envelope.diagnostic.as_ref().unwrap();
            assert_eq!(diagnostic.context.subject, Subject::argument([field]));
            assert_eq!(diagnostic.context.site, FailureSite::Host);
            assert_eq!(diagnostic.context.effects, Effects::Unchanged);
            let inspection = envelope.response_view(&reduced).into_value();
            assert!(!inspection.to_string().contains("private"), "{inspection}");
        }
        assert_eq!(factory.starts.load(Ordering::SeqCst), 0);
        router.shutdown().await;
    }

    #[test]
    fn target_views_keep_null_routing_and_detailed_defaults() {
        let detailed = TargetRecord {
            name: "root".into(),
            r#type: crate::target::TargetType::Local,
            source: TargetSource::Builtin,
            host: "localhost".into(),
            user: None,
            port: None,
            workspace: ".".into(),
            via: None,
            origin: None,
            auth: "local",
            external_agent: false,
        };
        assert_eq!(
            serde_json::to_value(CompactTarget::from(detailed.clone())).unwrap(),
            serde_json::json!({
                "name":"root", "type":"local", "host":"localhost",
                "workspace":null, "via":null, "origin":null
            })
        );
        assert_eq!(
            serde_json::to_value(detailed).unwrap(),
            serde_json::json!({
                "name":"root", "type":"local", "source":"builtin", "host":"localhost",
                "user":null, "port":null, "workspace":".", "via":null, "origin":null,
                "auth":"local", "external_agent":false
            })
        );
    }

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
