use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::target::{
    Connection, ConnectionKind, Password, SshAuthKind, TargetConfig, TargetDefinition,
    TargetRegistry, TargetSource,
};
use crate::tool::catalog::{RegistryError, ToolOptions, serialized};
use crate::tool::policy::{Capability, PermissionUse, ResourceId};
use crate::{
    target::TargetRouter,
    tool::{ToolError, ToolRegistryBuilder, catalog::Invocation},
};
use skyhook_tools::target::TargetRef;
use skyhook_worker::diagnostic::Effects;
use skyhook_worker::origin::winrm::{AuthKind, TlsKind};
use skyhook_worker::target::Os;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TargetsArgs {}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
struct TargetAddArgs {
    /// Session-wide name; root is reserved
    name: skyhook_tools::target::TargetName,
    #[serde(flatten)]
    config: TargetConfig,
}

/// The configuration reads the arguments' own entries, `name` aside, where `flatten`
/// would buffer them first: a refused one keeps its place among the arguments.
impl<'de> Deserialize<'de> for TargetAddArgs {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Arguments;
        impl<'de> serde::de::Visitor<'de> for Arguments {
            type Value = TargetAddArgs;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a target's name and configuration")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut name = None;
                let named = Named {
                    map,
                    name: &mut name,
                };
                let config =
                    TargetConfig::deserialize(serde::de::value::MapAccessDeserializer::new(named))?;
                let name = name.ok_or_else(|| serde::de::Error::missing_field("name"))?;
                Ok(TargetAddArgs { name, config })
            }
        }
        deserializer.deserialize_map(Arguments)
    }
}

/// A map's entries but `name`, whose value it keeps.
struct Named<'a, A> {
    map: A,
    name: &'a mut Option<skyhook_tools::target::TargetName>,
}

impl<'de, A: serde::de::MapAccess<'de>> serde::de::MapAccess<'de> for Named<'_, A> {
    type Error = A::Error;

    fn next_key_seed<K: serde::de::DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, A::Error> {
        while let Some(key) = self.map.next_key::<String>()? {
            if key != "name" {
                let key = serde::de::IntoDeserializer::into_deserializer(key);
                return seed.deserialize(key).map(Some);
            }
            *self.name = Some(self.map.next_value()?);
        }
        Ok(None)
    }

    fn next_value_seed<V: serde::de::DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, A::Error> {
        self.map.next_value_seed(seed)
    }
}

/// local identifies the Skyhook session host; ssh and winrm identify a remote target
#[derive(Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum TargetKind {
    Local,
    #[serde(untagged)]
    Remote(ConnectionKind),
}

/// Where a listed target comes from: built in, for the session host, or its definition's
/// source.
#[derive(Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum SourceView {
    Builtin,
    Config,
    Session,
}

/// The session host's own access, or how SSH or WinRM authenticates.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum AuthView {
    Local,
    #[serde(untagged)]
    Ssh(SshAuthKind),
    #[serde(untagged)]
    Winrm(AuthKind),
}

#[serde_with::skip_serializing_none]
#[derive(Serialize, JsonSchema)]
struct TargetView {
    name: String,
    r#type: TargetKind,
    os: Os,
    source: SourceView,
    host: String,
    user: Option<String>,
    port: Option<u16>,
    workspace: skyhook_worker::op::TargetPath,
    via: Option<String>,
    origin: Option<String>,
    #[schemars(with = "String")]
    auth: AuthView,
    forward_agent: bool,
    /// How a WinRM target trusts its listener, without a CA path or fingerprint
    #[schemars(with = "Option<String>")]
    tls: Option<TlsKind>,
    kdc: Option<String>,
}

impl TargetView {
    /// The session host, which runs `os`.
    fn root(os: Os) -> Self {
        Self {
            name: TargetRef::Root.to_string(),
            r#type: TargetKind::Local,
            os,
            source: SourceView::Builtin,
            host: "localhost".into(),
            user: None,
            port: None,
            workspace: skyhook_tools::spec::DEFAULT_PATH.into(),
            via: None,
            origin: None,
            auth: AuthView::Local,
            forward_agent: false,
            tls: None,
            kdc: None,
        }
    }
}

impl From<TargetDefinition> for TargetView {
    fn from(target: TargetDefinition) -> Self {
        let kind = target.connection.kind();
        let (user, port, auth, forward_agent, tls, kdc) = match target.connection {
            Connection::Ssh(ssh) => (
                ssh.user.map(String::from),
                ssh.port,
                AuthView::Ssh(ssh.auth.kind()),
                ssh.forward_agent,
                None,
                None,
            ),
            Connection::Winrm(winrm) => (
                Some(winrm.auth.user()),
                winrm.port,
                AuthView::Winrm(winrm.auth.kind()),
                false,
                Some(winrm.tls.kind()),
                winrm.auth.kdc().map(ToString::to_string),
            ),
        };
        Self {
            name: target.name.into(),
            r#type: TargetKind::Remote(kind),
            os: target.os,
            source: match target.source {
                TargetSource::Config => SourceView::Config,
                TargetSource::Session => SourceView::Session,
            },
            host: target.host,
            user,
            port: port.map(std::num::NonZeroU16::get),
            workspace: target.workspace,
            via: target.via.map(String::from),
            origin: target.origin.map(String::from),
            auth,
            forward_agent,
            tls,
            kdc,
        }
    }
}

pub(super) fn register(
    builder: &mut ToolRegistryBuilder,
    router: TargetRouter,
) -> Result<(), RegistryError> {
    let listed = router.clone();
    builder.register_checked(
        "targets",
        "List available execution targets and their hostnames without connecting",
        ToolOptions::default()
            .requires(Capability::Targets)
            .result::<Vec<TargetView>>(),
        Ok,
        move |_: TargetsArgs| {
            let router = listed.clone();
            Invocation::new(move |context| async move {
                let os = context.target()?.facts().platform.os;
                let targets = router.targets(TargetRegistry::list).into_iter();
                serialized(
                    std::iter::once(TargetView::root(os))
                        .chain(targets.map(TargetView::from))
                        .collect::<Vec<_>>(),
                )
            })
        },
    )?;
    builder.register_checked(
        "target_add",
        r#"Add or replace a session target without connecting to it or changing your current target.

Example: `tool.target_add({name:"db", host:"db.internal", transport:{ssh:{}}, via:"bastion"})`.
ssh.options can run commands and a password env, file or command is read on the origin, so they also require exec"#,
        ToolOptions::new(vec![Capability::Write])
            .requires(Capability::Targets)
            .permission_resource(ResourceId::session("targets"))
            .argument_permissions(|_, args: &TargetAddArgs| Ok(add_permissions(&args.config))),
        Ok,
        move |args: TargetAddArgs| {
            let router = router.clone();
            Invocation::unit(move |context| async move {
                let definition = TargetDefinition::from_config(args.name.into(), args.config)
                    .map_err(|error| ToolError::from(error).effects(Effects::Unchanged))?;
                router
                    .add(definition, context.invocation_subject()?)
                    .await?;
                Ok(())
            })
        },
    )?;
    Ok(())
}

/// Options can run commands on the origin, and any password but a literal is read
/// from it: its environment, a file there or a command it runs.
fn add_permissions(config: &TargetConfig) -> Vec<PermissionUse> {
    let password = config.transport.password().and_then(Password::given);
    let read = password.is_some_and(|password| password.literal().is_none());
    let options = config
        .transport
        .ssh()
        .is_some_and(|ssh| !ssh.options.is_empty());
    let reaches = read || options;
    (reaches.then(|| PermissionUse::new(Capability::Exec, ResourceId::session("targets"))))
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn saved_target_failures_hide_submitted_aliases_from_reduced_capabilities() {
        use crate::tool::policy::CapabilitySet;
        use crate::{tests::TestRuntime, tool::policy::AllowAll};
        use skyhook_tools::diagnostic::FailureSite;
        use skyhook_worker::diagnostic::Subject;
        use std::sync::Arc;

        let runtime = TestRuntime::new().await;
        let router = TargetRouter::test(&runtime.store, [], Arc::new(AllowAll)).await;
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, router.clone()).unwrap();
        let mut privileged = CapabilitySet::default();
        privileged.insert(Capability::Targets);
        let executor = runtime
            .executor(builder)
            .with_capabilities(privileged.clone());
        let mut reduced = privileged.clone();
        reduced.remove(&Capability::Targets);
        for (arguments, field) in [
            (
                serde_json::json!({"name":"private-requested","host":"host","transport":{"ssh":{}},"via":"private-jump"}),
                "via",
            ),
            (
                serde_json::json!({"name":"private-requested","host":"host","transport":{"ssh":{}},"origin":"private-origin"}),
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
            let inspection = envelope.response_view(&reduced, None).into_value();
            assert!(!inspection.to_string().contains("private"), "{inspection}");
        }
        router.shutdown().await;
    }

    /// A refused SSH user or option names the argument it is in. A user is one word,
    /// while an option's value may hold spaces.
    #[tokio::test]
    async fn refused_ssh_text_names_its_argument() {
        use crate::tool::policy::CapabilitySet;
        use crate::{tests::TestRuntime, tool::policy::AllowAll};
        use skyhook_worker::diagnostic::Subject;
        use std::sync::Arc;

        let runtime = TestRuntime::new().await;
        let router = TargetRouter::test(&runtime.store, [], Arc::new(AllowAll)).await;
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, router.clone()).unwrap();
        let mut capabilities = CapabilitySet::default();
        capabilities.insert(Capability::Targets);
        let executor = runtime.executor(builder).with_capabilities(capabilities);
        let add = async |ssh: serde_json::Value| {
            let arguments =
                serde_json::json!({"name": "db", "host": "db", "transport": {"ssh": ssh}});
            let added = executor.run_model(&runtime.agent, "target_add", arguments);
            match added.await {
                Ok(added) => {
                    assert!(!added.is_error, "{}", added.output.value);
                    None
                }
                Err(error) => Some(error.diagnostic().context.subject),
            }
        };
        let user = Subject::argument(["transport", "ssh", "user"]);
        // An option names no argument of its own, so its name is not quoted.
        let options = Subject::argument(["transport", "ssh", "options", "*"]);
        for (ssh, refused) in [
            (serde_json::json!({"user": "a\u{7}b"}), Some(&user)),
            (serde_json::json!({"user": " "}), Some(&user)),
            (serde_json::json!({"user": "deploy user"}), Some(&user)),
            (
                serde_json::json!({"options": {"Server Alive": "1"}}),
                Some(&options),
            ),
            (
                serde_json::json!({"options": {"LogLevel": ""}}),
                Some(&options),
            ),
            (
                serde_json::json!({"user": "deploy", "options": {"ProxyCommand": "nc %h %p"}}),
                None,
            ),
        ] {
            assert_eq!(add(ssh.clone()).await.as_ref(), refused, "{ssh}");
        }
        router.shutdown().await;
    }

    #[test]
    fn target_views_omit_absent_routing_and_hide_key_paths() {
        assert_eq!(
            serde_json::to_value(TargetView::root(Os::Linux)).unwrap(),
            serde_json::json!({
                "name":"root", "type":"local", "os":"linux", "source":"builtin", "host":"localhost",
                "workspace":".", "auth":"local", "forward_agent":false
            })
        );
        let view =
            |target: TargetDefinition| serde_json::to_value(TargetView::from(target)).unwrap();
        let mut build = crate::target::tests::definition("build", "/srv", Some("gateway"));
        *build.ssh_mut() = serde_json::from_value(
            serde_json::json!({"key": "secret-key", "password": "secret-passphrase"}),
        )
        .unwrap();
        let keyed = view(build);
        assert!(!keyed.to_string().contains("secret"), "{keyed}");
        assert_eq!(keyed["auth"], "key");
        // WinRM shows its method and trust as SSH shows its method, without a CA path.
        let config = serde_json::json!({"host": "win", "transport": {"winrm": {
            "user": "alice@CORP", "password": "secret-password", "auth": "kerberos",
            "kdc": "dc.corp", "tls": {"ca": "~/secret-ca.pem"}, "port": 15986}}});
        let config = serde_json::from_value(config).unwrap();
        let win = TargetDefinition::from_config("win".into(), config).unwrap();
        let view = view(win);
        assert!(!view.to_string().contains("secret"), "{view}");
        let shape = ["type", "user", "auth", "tls", "kdc", "port"].map(|field| &view[field]);
        assert_eq!(
            serde_json::json!(shape),
            serde_json::json!(["winrm", "alice@CORP", "kerberos", "ca", "dc.corp", 15986])
        );
    }

    #[test]
    fn add_requires_exec_for_options_and_passwords_read_on_the_origin() {
        let capabilities = |config: serde_json::Value| {
            let config = serde_json::from_value(config).unwrap();
            let permissions = add_permissions(&config).into_iter();
            permissions
                .map(|permission| permission.capability)
                .collect::<Vec<_>>()
        };
        let ssh = |ssh| serde_json::json!({"host": "h", "transport": {"ssh": ssh}});
        let winrm = |password| {
            serde_json::json!({"host": "h", "transport": {"winrm": {"user": "u",
                "password": password, "auth": "negotiate", "tls": "system"}}})
        };
        for (config, expected) in [
            (ssh(serde_json::json!({})), Vec::new()),
            (ssh(serde_json::json!({"password": "literal"})), Vec::new()),
            (
                ssh(serde_json::json!({"password": {"env": "P"}})),
                vec![Capability::Exec],
            ),
            (
                ssh(serde_json::json!({"key": "~/k", "password": {"file": "~/p"}})),
                vec![Capability::Exec],
            ),
            (
                ssh(serde_json::json!({"password": {"command": "pass show p"}})),
                vec![Capability::Exec],
            ),
            (
                ssh(serde_json::json!({"options": {"ServerAliveInterval": "15"}})),
                vec![Capability::Exec],
            ),
            (winrm(serde_json::json!("literal")), Vec::new()),
            (
                winrm(serde_json::json!({"env": "P"})),
                vec![Capability::Exec],
            ),
            (
                winrm(serde_json::json!({"file": "~/p"})),
                vec![Capability::Exec],
            ),
            (
                winrm(serde_json::json!({"command": ["pass", "p"]})),
                vec![Capability::Exec],
            ),
        ] {
            assert_eq!(capabilities(config), expected);
        }
    }

    /// Admission validates against this schema before serde runs.
    #[test]
    fn the_schema_takes_every_password_form_and_rejects_unknown_fields() {
        let schema = serde_json::to_value(schemars::schema_for!(TargetAddArgs)).unwrap();
        let valid = |ssh: serde_json::Value| {
            let arguments = serde_json::json!({"name":"db","host":"db","transport":{"ssh":ssh}});
            jsonschema::is_valid(&schema, &arguments)
        };
        for password in [
            serde_json::json!("pw"),
            serde_json::json!({"env": "P"}),
            serde_json::json!({"command": "pass show db"}),
            serde_json::json!({"command": ["pass", "show", "db"]}),
            serde_json::json!({"file": "~/pw"}),
        ] {
            assert!(
                valid(serde_json::json!({"password": password})),
                "{password}"
            );
        }
        let invalid =
            serde_json::json!({"name":"db","host":"db","transport":{"ssh":{}},"target":"root"});
        assert!(!jsonschema::is_valid(&schema, &invalid), "{schema}");
    }
}
