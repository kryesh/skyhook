//! Host-placed research tools. A tool's argument schema is the set of actions the
//! agent's capabilities permit, so admission against that schema is the authorization.

/// A tool's input and the internal enum it admits into. The input is an object: `action`
/// names one action, documented by the entry's doc, and that action's arguments sit beside
/// it. An action declared with one type, `name => Variant(Type)`, takes that type's fields;
/// one declared with fields, `name => Variant { fields }`, takes those. Actions give one
/// name different types only where `unions` lists it. `-> Result` names what the action
/// returns, which its handler serializes through `output::<result::Variant>`.
macro_rules! actions {
    (
        enum $Enum:ident {
            $( $(#[doc = $doc:literal])* $name:ident => $Variant:ident $arguments:tt -> $Result:ty ),* $(,)?
        }
        $( unions: [$($union:ident),* $(,)?] )?
    ) => {
        mod payload {
            #[allow(unused_imports)]
            use super::*;
            $( actions!(@payload $Variant $arguments); )*
        }

        mod result {
            $( pub(super) struct $Variant; )*
        }

        $( impl super::Returns for result::$Variant {
            type Value = $Result;
        } )*

        enum $Enum {
            $( $Variant(payload::$Variant), )*
        }

        #[derive(Clone, Copy, serde::Deserialize)]
        #[allow(non_camel_case_types)]
        enum Name {
            $( $name, )*
        }

        impl super::Actions for Name {
            const ALL: &[Self] = &[$( Self::$name, )*];
            const UNIONS: &[&str] = &[$($( stringify!($union), )*)?];

            fn name(self) -> &'static str {
                match self {
                    $( Self::$name => stringify!($name), )*
                }
            }

            fn doc(self) -> &'static str {
                match self {
                    $( Self::$name => concat!($($doc),*), )*
                }
            }

            fn arguments(self, generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
                match self {
                    $( Self::$name => <payload::$Variant as schemars::JsonSchema>::json_schema(generator), )*
                }
            }

            fn result(self) -> serde_json::Value {
                match self {
                    $( Self::$name => skyhook_tools::schema::result_schema::<<result::$Variant as super::Returns>::Value>(), )*
                }
            }

            fn catalog() -> &'static super::Catalog {
                static CATALOG: std::sync::LazyLock<super::Catalog> =
                    std::sync::LazyLock::new(super::Catalog::new::<Name>);
                &CATALOG
            }
        }

        impl super::Admitted for $Enum {
            type Name = Name;

            fn action(&self) -> Name {
                match self {
                    $( Self::$Variant(_) => Name::$name, )*
                }
            }
        }

        impl TryFrom<super::Input<Name>> for $Enum {
            type Error = skyhook_tools::error::AdmissionError;

            fn try_from(input: super::Input<Name>) -> Result<Self, Self::Error> {
                Ok(match input.action {
                    $( Name::$name => Self::$Variant(input.arguments()?), )*
                })
            }
        }
    };
    (@payload $Variant:ident ($Type:ty)) => {
        pub(super) type $Variant = $Type;
    };
    (@payload $Variant:ident { $( $(#[$meta:meta])* $field:ident : $Type:ty ),* $(,)? }) => {
        #[derive(serde::Deserialize, schemars::JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub(super) struct $Variant {
            $( $(#[$meta])* pub(super) $field: $Type, )*
        }
    };
}

mod input;
mod query;
mod research;

use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use skyhook::tool::catalog::ToolPlacement;
use skyhook::tool::catalog::{RegistryError, ToolOptions};
use skyhook::tool::policy::{Capability, CapabilitySet, ResourceId};
use skyhook::tool::{ToolContext, ToolError, ToolRegistry, ToolRegistryBuilder};
use skyhook_tools::error::AdmissionError;

use crate::store::Store;
use input::{Catalog, Input};
use skyhook::identity::AgentId;

/// A tool's closed set of actions: the values of its input's `action`.
trait Actions: Copy + DeserializeOwned + 'static {
    const ALL: &'static [Self];
    /// The argument names whose type differs between actions.
    const UNIONS: &'static [&'static str];

    fn name(self) -> &'static str;

    fn doc(self) -> &'static str;

    /// The schema of the action's arguments: an object, or alternative objects.
    fn arguments(self, generator: &mut schemars::SchemaGenerator) -> schemars::Schema;

    /// The schema of what the action returns.
    fn result(self) -> Value;

    fn catalog() -> &'static Catalog;
}

/// A tool's internal enum, admitted from its input: each value takes one action.
trait Admitted: TryFrom<Input<Self::Name>, Error = AdmissionError> + Send + 'static {
    type Name: Actions;

    fn action(&self) -> Self::Name;
}

trait Action: Admitted {
    /// Whether an agent with `capabilities` may take this action. Every tool already
    /// requires Read to be offered at all.
    fn permitted(action: Self::Name, capabilities: &CapabilitySet) -> bool;

    fn placement(&self) -> Placement {
        Placement::Writer
    }
}

/// Where an action's work runs.
enum Placement {
    /// On a reader beside the writer: the action only reads.
    Reader,
    /// In the writer's turn: the action's work is its transactions.
    Writer,
    /// In the session's files turn, outside the writer's: most of the action's work is
    /// outside the database (artifact and report files), so it must not keep other
    /// writers waiting; each transaction it makes still has the writer's connection to
    /// itself.
    Outside,
}

fn holds(capabilities: &CapabilitySet, required: impl IntoIterator<Item = Capability>) -> bool {
    (required.into_iter()).all(|capability| capabilities.contains(&capability))
}

/// Internal bookkeeping needs capability authority, not a second human approval:
/// Read is the execution capability, and the session's run is the approval resource.
fn options<A: Action>() -> ToolOptions<A> {
    ToolOptions::new(vec![Capability::Read])
        .placement(ToolPlacement::Host)
        .permission_resource(ResourceId::Session {
            name: "security.run".to_owned(),
        })
        .computed_schema(input::schema::<A>)
        .internal_result(|action: &A| A::Name::catalog().result(action.action()))
}

fn register<A: Action>(
    builder: &mut ToolRegistryBuilder,
    name: &str,
    description: &str,
    options: ToolOptions<A>,
    execute: impl Fn(&Store, &AgentId, &ToolContext, A) -> Result<Value, ToolError>
    + Send
    + Sync
    + 'static,
) -> Result<(), RegistryError> {
    let execute = Arc::new(execute);
    builder.register_admitted(
        name,
        description,
        options,
        A::try_from,
        move |context: ToolContext, action: A| {
            let execute = execute.clone();
            async move {
                let store = Store::of_session(context.extension()).map_err(ToolError::failed)?;
                let placement = action.placement();
                let (worker, waiting) = (store.clone(), context.clone());
                let work = move || {
                    let actor = context.agent().clone();
                    execute(&worker, &actor, &context, action)
                };
                match placement {
                    Placement::Reader => store.reading(work).await,
                    Placement::Writer => store.writing(work).await,
                    Placement::Outside => (store.filing(waiting.cancelled(), work).await)
                        .unwrap_or_else(|()| Err(ToolError::cancelled())),
                }
            }
        },
    )?;
    Ok(())
}

/// What an action returns, named by its `result::Variant`.
trait Returns {
    type Value: Serialize + JsonSchema;
}

/// The action `R`'s result as the tool's output document.
fn output<R: Returns>(
    result: Result<R::Value, impl std::fmt::Display>,
) -> Result<Value, ToolError> {
    serde_json::to_value(result.map_err(ToolError::failed)?).map_err(ToolError::failed)
}

/// The research tools, which act on the tables of the session that calls them.
pub(crate) fn registry() -> Result<ToolRegistry, RegistryError> {
    let mut builder = ToolRegistryBuilder::default();
    research::register(&mut builder)?;
    query::register(&mut builder)?;
    Ok(builder.build())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{future::Future, sync::Arc, time::Duration};

    use serde_json::json;
    use skyhook::agent::SessionHandle;

    use super::*;
    use crate::{
        capabilities::SecurityCapability,
        store::{Error, LivePolicy, RunOptions, StatusChange},
    };

    pub(crate) async fn bounded<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(30), future)
            .await
            .expect("test deadline")
    }

    pub(crate) struct Fixture {
        pub(crate) root: tempfile::TempDir,
        pub(crate) session: SessionHandle,
    }
    impl Fixture {
        pub(crate) async fn new(capabilities: CapabilitySet) -> Self {
            Self::built(capabilities, crate::store::tests::harness, LivePolicy::Yes).await
        }

        pub(crate) async fn built(
            capabilities: CapabilitySet,
            harness: impl FnOnce(&std::path::Path) -> skyhook::agent::HarnessBuilder,
            live: LivePolicy,
        ) -> Self {
            let root = tempfile::tempdir().unwrap();
            let harness = harness(root.path())
                .capabilities(capabilities)
                .policy(Arc::new(skyhook::tool::policy::AllowAll))
                .tools(registry().unwrap())
                .build()
                .await
                .unwrap();
            let session = harness.new_session().await.unwrap();
            let store = Store::of(session.store().extension().unwrap());
            let options = RunOptions {
                live,
                ..RunOptions::default()
            };
            tokio::task::spawn_blocking(move || store.start_run(options))
                .await
                .unwrap()
                .unwrap();
            Self { root, session }
        }

        pub(crate) async fn script(&self, source: impl Into<String>) -> Value {
            bounded(self.session.run_script(source))
                .await
                .unwrap()
                .value["value"]
                .clone()
        }

        pub(crate) async fn call(&self, name: &str, action: Value) -> Value {
            self.script(format!("return await tool.{name}({action});"))
                .await
        }

        pub(crate) async fn success(&self, name: &str, action: Value) -> Value {
            let response = self.call(name, action).await;
            assert_ne!(response["state"], "failed", "{response}");
            assert!(response["error"].is_null(), "{response}");
            response["result"].clone()
        }

        pub(crate) async fn close(self) {
            bounded(self.session.shutdown()).await.unwrap();
        }
    }

    /// The input schema `tool` surfaces to an agent holding `capabilities`, if it surfaces.
    pub(crate) fn schema(
        registry: &ToolRegistry,
        capabilities: &CapabilitySet,
        tool: &str,
    ) -> Option<Value> {
        let agent = AgentId::root(skyhook::identity::SessionId::from_bytes([0; 16]));
        let surface = registry.surface_for_agent(capabilities, &agent);
        Some(surface.get(tool)?.input_schema.clone())
    }

    /// The actions `tool` surfaces to an agent holding `capabilities`, sorted.
    pub(crate) fn actions(
        registry: &ToolRegistry,
        capabilities: &CapabilitySet,
        tool: &str,
    ) -> Option<Vec<String>> {
        let schema = schema(registry, capabilities, tool)?;
        let actions = schema["properties"]["action"]["enum"].as_array()?;
        let mut names: Vec<_> = (actions.iter())
            .map(|name| name.as_str().unwrap().to_owned())
            .collect();
        names.sort();
        Some(names)
    }

    pub(crate) fn capabilities(extra: impl IntoIterator<Item = Capability>) -> CapabilitySet {
        [Capability::Read].into_iter().chain(extra).collect()
    }

    /// Each capability admits its action group at the surface, and the executor refuses an
    /// action outside the surfaced schema before it reaches the store, as it does a field its
    /// action does not define at any depth, so a misspelt or renamed field is an error rather
    /// than silently dropped, another action's argument, even null, a missing one, and a status
    /// its node's kind does not take.
    #[tokio::test]
    async fn actions_follow_capabilities_at_the_surface_and_the_executor() {
        use SecurityCapability::{Coordinate, Research, Scheduled, Test, Verdict};
        let [research, verdict, test, scheduled, coordinate] =
            [Research, Verdict, Test, Scheduled, Coordinate].map(SecurityCapability::capability);
        let registry = registry().unwrap();
        let read = capabilities([]);
        let researcher = capabilities([research.clone()]);
        let actions_of = |granted: &CapabilitySet, tool: &str| actions(&registry, granted, tool);
        let owned = |actions: &[&str]| Some(actions.iter().map(|a| a.to_string()).collect());
        for (granted, tool, actions) in [
            (&read, "research", None),
            (
                &read,
                "research_query",
                owned(&["history", "list", "node", "search", "synthesis", "task"]),
            ),
            (&capabilities([verdict.clone()]), "research", None),
            (
                &capabilities([verdict.clone(), Capability::Write]),
                "research",
                owned(&["report"]),
            ),
            (&capabilities([test.clone()]), "research", owned(&["test"])),
            (&capabilities([scheduled]), "research", owned(&["begin"])),
            (
                &capabilities([coordinate]),
                "research",
                owned(&["generate"]),
            ),
        ] {
            assert_eq!(actions_of(granted, tool), actions, "{tool}");
        }
        // Only the root mode schedules the run and writes its reports out, and only a mode
        // the scheduler launches, whose instructions begin its work, can begin work.
        for (mode, preset) in crate::modes::tests::editions(LivePolicy::Yes) {
            let granted: CapabilitySet = preset.capabilities.into();
            let offers = |tool, action| {
                (actions_of(&granted, tool).unwrap_or_default().iter()).any(|a| a == action)
            };
            let root = mode == crate::modes::root();
            assert_eq!(offers("research_query", "startable"), root, "{mode}");
            assert_eq!(offers("research", "generate"), root, "{mode}");
            let begins = (preset.instructions.unwrap().as_str()).contains("`research` `begin`");
            assert_eq!(offers("research", "begin"), root || begins, "{mode}");
        }
        // A validator decides verdicts; a tester records what it observes, but never decides
        // verdicts.
        for (granted, with, without) in [
            (
                &researcher,
                &["upsert", "assess"][..],
                &["verdict", "test"][..],
            ),
            (
                &capabilities([research.clone(), verdict]),
                &["upsert", "verdict"],
                &["test"],
            ),
            (
                &capabilities([research, test]),
                &["evidence", "test"],
                &["verdict", "begin"],
            ),
        ] {
            let actions = actions_of(granted, "research").unwrap();
            assert!(
                with.iter().all(|a| actions.iter().any(|b| b == a))
                    && !without.iter().any(|a| actions.iter().any(|b| b == a)),
                "{actions:?}"
            );
        }

        let fixture = Fixture::new(researcher).await;
        let history = || fixture.success("research_query", json!({"action":"history"}));
        let before = history().await["items"].clone();
        let component = json!({"action":"upsert","record":{"kind":"component","title":"parser","body":"parses"}});
        let mut unknown = component.clone();
        unknown["priority"] = json!("high");
        for (tool, call) in [
            (
                "research",
                json!({"action":"verdict","candidate":1,"verdict":"unproven","note":"x"}),
            ),
            ("research", unknown),
            ("research_query", json!({"action":"node"})),
            ("research_query", json!({"action":"synthesis","page":{}})),
            ("research_query", json!({"action":"synthesis","page":null})),
            (
                "research",
                json!({"action":"set_status","record":{"kind":"premise","id":1,"status":"swept"},"note":"n"}),
            ),
            (
                "research_query",
                json!({"action":"list","record":{"kind":"flaw","status":"holds"}}),
            ),
        ] {
            let refused = fixture.call(tool, call.clone()).await;
            assert_eq!(refused["state"], "failed", "{call}: {refused}");
        }
        assert_eq!(
            history().await["items"],
            before,
            "refused actions must not write audit records"
        );
        // An edit names its node's kind: what the kind takes is admitted, and nothing else.
        let id = fixture.success("research", component).await;
        let edit = json!({"action":"upsert","record":{"kind":"component","id":id,"title":"request parser"}});
        fixture.success("research", edit).await;
        let foreign =
            json!({"action":"upsert","record":{"kind":"component","id":id,"scope":"code"}});
        let refused = fixture.call("research", foreign).await;
        let error = refused["error"].as_str().unwrap();
        assert!(
            error.contains("`/record` failed: property `scope`"),
            "{error}"
        );
        // A record's field left beside it is pointed inside.
        let beside = json!({"action":"upsert","kind":"path","title":"t","body":"b"});
        let refused = fixture.call("research", beside).await;
        let error = refused["error"].as_str().unwrap();
        assert!(error.contains("goes inside `record`"), "{error}");
        // A check names at least one premise it rests on, missing and empty alike: the schema
        // refuses it, and the record read without it says what to name.
        let mut record = json!({"kind":"state","id":1,"status":"checked"});
        for (rests_on, refusal) in [
            (None, "`/record` failed: missing required field `rests_on`"),
            (
                Some(json!([])),
                "`/record/rests_on` failed: minimum items 1",
            ),
        ] {
            if let Some(rests_on) = rests_on {
                record["rests_on"] = rests_on;
            }
            let unrested = json!({"action":"set_status","record":record,"note":"n"});
            let refused = fixture.call("research", unrested).await;
            let error = refused["error"].as_str().unwrap();
            assert!(error.contains(refusal), "{error}");
            let read = serde_json::from_value::<StatusChange>(record.clone());
            let error = read.unwrap_err().to_string();
            assert!(error.contains(&Error::Unrested.to_string()), "{error}");
        }
        fixture.close().await;
    }
}
