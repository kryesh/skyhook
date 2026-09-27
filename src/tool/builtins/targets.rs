use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    session::SessionStore,
    target::{
        SshAuth, SshOptions, TargetConfig, TargetDefinition, TargetRef, TargetRouter, TargetSource,
    },
    tool::{
        RegistryError, ToolError, ToolOptions, ToolRegistryBuilder,
        diagnostic::Effects,
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

/// local identifies the Skyhook session host; ssh identifies a remote target.
#[derive(Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum TargetKind {
    Local,
    Ssh,
}

/// The session host's own access, or how SSH authenticates, without a key path.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum AuthView {
    Local,
    #[serde(untagged)]
    Ssh(SshAuth),
}

#[serde_with::skip_serializing_none]
#[derive(Serialize, JsonSchema)]
struct CompactTarget {
    name: String,
    r#type: TargetKind,
    host: String,
    #[schemars(with = "std::path::PathBuf")]
    workspace: Option<std::path::PathBuf>,
    #[schemars(with = "String")]
    via: Option<String>,
    #[schemars(with = "String")]
    origin: Option<String>,
}

impl From<DetailedTarget> for CompactTarget {
    fn from(detailed: DetailedTarget) -> Self {
        Self {
            name: detailed.name,
            host: detailed.host,
            workspace: match detailed.r#type {
                TargetKind::Local => None,
                TargetKind::Ssh => Some(detailed.workspace),
            },
            r#type: detailed.r#type,
            via: detailed.via,
            origin: detailed.origin,
        }
    }
}

#[serde_with::skip_serializing_none]
#[derive(Serialize, JsonSchema)]
struct DetailedTarget {
    name: String,
    r#type: TargetKind,
    source: TargetSource,
    host: String,
    #[schemars(with = "String")]
    user: Option<String>,
    #[schemars(with = "u16")]
    port: Option<u16>,
    workspace: std::path::PathBuf,
    #[schemars(with = "String")]
    via: Option<String>,
    #[schemars(with = "String")]
    origin: Option<String>,
    #[schemars(with = "String")]
    auth: AuthView,
    external_agent: bool,
}

impl DetailedTarget {
    fn root() -> Self {
        Self {
            name: TargetRef::Root.to_string(),
            r#type: TargetKind::Local,
            source: TargetSource::Builtin,
            host: "localhost".into(),
            user: None,
            port: None,
            workspace: crate::tool::registry::DEFAULT_PATH.into(),
            via: None,
            origin: None,
            auth: AuthView::Local,
            external_agent: false,
        }
    }
}

impl From<TargetDefinition> for DetailedTarget {
    fn from(target: TargetDefinition) -> Self {
        Self {
            name: target.name.into(),
            r#type: TargetKind::Ssh,
            source: target.source,
            host: target.host,
            user: target.ssh.user,
            port: target.ssh.port.map(std::num::NonZeroU16::get),
            workspace: target.workspace,
            via: target.via.map(String::from),
            origin: target.origin.map(String::from),
            auth: AuthView::Ssh(target.ssh.auth.kind()),
            external_agent: target.ssh.external_agent,
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[serde(untagged)]
enum TargetView {
    Compact(CompactTarget),
    Detailed(DetailedTarget),
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
                let targets = router.targets().list(context.capabilities()).await;
                let listed = std::iter::once(DetailedTarget::root())
                    .chain(targets.into_iter().map(DetailedTarget::from));
                Ok(listed
                    .map(|target| {
                        if args.details {
                            TargetView::Detailed(target)
                        } else {
                            TargetView::Compact(target.into())
                        }
                    })
                    .collect())
            }
        },
    )?;
    builder.register_unit::<TargetAddArgs, _, _>(
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
            .argument_permissions(|_, args: &TargetAddArgs| Ok(add_permissions(&args.config.ssh))),
        move |context, args| {
            let router = router.clone();
            let store = store.clone();
            async move {
                let definition = TargetDefinition::from_config(args.name, args.config)
                    .map_err(|error| {
                        ToolError::from(error.into_admission_error()).effects(Effects::Unchanged)
                    })?;
                router
                    .add(definition, context.invocation_subject()?, &store)
                    .await
                    .map_err(|e| e.into_tool_error())?;
                Ok(())
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
    fn target_views_omit_absent_routing_and_keep_detailed_defaults() {
        assert_eq!(
            serde_json::to_value(CompactTarget::from(DetailedTarget::root())).unwrap(),
            serde_json::json!({"name":"root", "type":"local", "host":"localhost"})
        );
        assert_eq!(
            serde_json::to_value(DetailedTarget::root()).unwrap(),
            serde_json::json!({
                "name":"root", "type":"local", "source":"builtin", "host":"localhost",
                "workspace":".", "auth":"local", "external_agent":false
            })
        );
        let mut build = TargetDefinition::test("build", "/srv", Some("gateway"));
        build.ssh.auth = crate::target::TargetAuth::Key {
            path: "secret-key".into(),
        };
        assert_eq!(
            serde_json::to_value(CompactTarget::from(DetailedTarget::from(build.clone()))).unwrap(),
            serde_json::json!({
                "name":"build", "type":"ssh", "host":"build.example.com",
                "workspace":"/srv", "via":"gateway"
            })
        );
        let detailed = serde_json::to_value(DetailedTarget::from(build)).unwrap();
        assert_eq!(detailed["auth"], "key");
        assert!(!detailed.to_string().contains("secret-key"));
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
