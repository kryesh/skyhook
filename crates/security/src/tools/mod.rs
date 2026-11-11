//! Host-placed research tools, one per action on the session's research graph. A tool is
//! offered to the agents whose capabilities permit it, so the tools an agent holds are what
//! it may do.

mod query;
mod research;

use std::{fmt::Display, sync::Arc};

use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use skyhook::tool::catalog::ToolPlacement;
use skyhook::tool::catalog::{RegistryError, ToolOptions};
use skyhook::tool::policy::{Capability, ResourceId};
use skyhook::tool::{ToolContext, ToolError, ToolRegistry, ToolRegistryBuilder};

use crate::store::Store;
use skyhook::identity::AgentId;

/// Where a tool's work runs.
#[derive(Clone, Copy)]
enum Placement {
    /// On a reader beside the writer: the tool only reads.
    Reader,
    /// In the writer's turn: the tool's work is its transactions.
    Writer,
    /// In the session's files turn, outside the writer's: most of the tool's work is
    /// outside the database (artifact and report files), so it must not keep other
    /// writers waiting; each transaction it makes still has the writer's connection to
    /// itself.
    Outside,
}

/// Where a tool's description gives its result.
enum Documented {
    /// In the `Result:` line generated from its type.
    Generated,
    /// In its own words, where the generated type would outrun them.
    Described,
}

/// One research tool, offered to agents holding `requires`.
struct Tool<'a> {
    name: &'a str,
    description: &'a str,
    requires: &'a [Capability],
    placement: Placement,
    documented: Documented,
}

impl Tool<'_> {
    /// Internal bookkeeping needs capability authority, not a second human approval:
    /// Read is the execution capability, and the session's run is the approval resource.
    fn register<I, O, E>(
        self,
        builder: &mut ToolRegistryBuilder,
        work: impl Fn(&Store, &AgentId, &ToolContext, I) -> Result<O, E> + Send + Sync + 'static,
    ) -> Result<(), RegistryError>
    where
        I: DeserializeOwned + JsonSchema + Send + 'static,
        O: Serialize + JsonSchema + Send + 'static,
        E: Display,
    {
        let mut options = ToolOptions::new(vec![Capability::Read])
            .placement(ToolPlacement::Host)
            .permission_resource(ResourceId::Session {
                name: "security.run".to_owned(),
            });
        for capability in self.requires {
            options = options.requires(capability.clone());
        }
        if let Documented::Described = self.documented {
            options = options.internal_result(|_| skyhook_tools::schema::result_schema::<O>());
        }
        let placement = self.placement;
        let work = Arc::new(work);
        builder.register(
            self.name,
            self.description,
            options,
            move |context: ToolContext, input: I| {
                let work = work.clone();
                async move {
                    let store =
                        Store::of_session(context.extension()).map_err(ToolError::failed)?;
                    let (worker, waiting) = (store.clone(), context.clone());
                    let work = move || {
                        let actor = context.agent().clone();
                        work(&worker, &actor, &context, input).map_err(ToolError::failed)
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

    use serde_json::{Value, json};
    use skyhook::agent::SessionHandle;
    use skyhook::tool::policy::CapabilitySet;

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

    /// The research tools surfaced to an agent holding `capabilities`, without their prefix,
    /// sorted.
    pub(crate) fn offered(registry: &ToolRegistry, capabilities: &CapabilitySet) -> Vec<String> {
        let agent = AgentId::root(skyhook::identity::SessionId::from_bytes([0; 16]));
        let surface = registry.surface_for_agent(capabilities, &agent);
        let names = surface.specs().map(|spec| spec.name.as_str());
        let mut names: Vec<_> = (names.filter_map(|name| name.strip_prefix("research_")))
            .map(str::to_owned)
            .collect();
        names.sort();
        names
    }

    pub(crate) fn capabilities(extra: impl IntoIterator<Item = Capability>) -> CapabilitySet {
        [Capability::Read].into_iter().chain(extra).collect()
    }

    /// Each capability admits its tools at the surface, and the executor refuses a field a
    /// tool does not define at any depth before the call reaches the store, so a misspelt or
    /// renamed field is an error rather than silently dropped, a missing one, and a status
    /// its node's kind does not take.
    #[tokio::test]
    async fn tools_follow_capabilities_at_the_surface_and_the_executor() {
        use SecurityCapability::{Coordinate, Research, Scheduled, Test, Verdict};
        let [research, verdict, test, scheduled, coordinate] =
            [Research, Verdict, Test, Scheduled, Coordinate].map(SecurityCapability::capability);
        let registry = registry().unwrap();
        let researcher = capabilities([research.clone()]);
        let offered = |granted: &CapabilitySet| offered(&registry, granted);
        let reads = ["history", "list", "node", "search", "synthesis", "task"];
        let with = |extra: &[&str]| {
            let mut names: Vec<_> = reads.iter().chain(extra).map(|n| n.to_string()).collect();
            names.sort();
            names
        };
        for (granted, writes) in [
            (capabilities([]), &[][..]),
            (capabilities([verdict.clone()]), &[]),
            (
                capabilities([verdict.clone(), Capability::Write]),
                &["report"],
            ),
            (capabilities([test.clone()]), &["test"]),
            (capabilities([scheduled]), &["begin"]),
            (capabilities([coordinate]), &["generate", "startable"]),
        ] {
            assert_eq!(offered(&granted), with(writes));
        }
        // Only the root mode schedules the run and writes its reports out, and only a mode
        // the scheduler launches, whose instructions begin its work, can begin work.
        for (mode, preset) in crate::modes::tests::editions(LivePolicy::Yes) {
            let granted: CapabilitySet = preset.capabilities.into();
            let offers = |tool: &str| offered(&granted).iter().any(|name| name == tool);
            let root = mode == crate::modes::root();
            assert_eq!(offers("startable"), root, "{mode}");
            assert_eq!(offers("generate"), root, "{mode}");
            let begins = (preset.instructions.unwrap().as_str()).contains("`research_begin`");
            assert_eq!(offers("begin"), root || begins, "{mode}");
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
            let tools = offered(granted);
            assert!(
                with.iter().all(|a| tools.iter().any(|b| b == a))
                    && !without.iter().any(|a| tools.iter().any(|b| b == a)),
                "{tools:?}"
            );
        }

        let fixture = Fixture::new(researcher).await;
        let history = || fixture.success("research_history", json!({}));
        let before = history().await["items"].clone();
        let component = json!({"record":{"kind":"component","title":"parser","body":"parses"}});
        let mut unknown = component.clone();
        unknown["priority"] = json!("high");
        for (tool, call) in [
            ("research_upsert", unknown),
            ("research_node", json!({})),
            ("research_synthesis", json!({"page":{}})),
            (
                "research_set_status",
                json!({"record":{"kind":"premise","id":1,"status":"swept"},"note":"n"}),
            ),
            (
                "research_list",
                json!({"record":{"kind":"flaw","status":"holds"}}),
            ),
        ] {
            let refused = fixture.call(tool, call.clone()).await;
            assert_eq!(refused["state"], "failed", "{call}: {refused}");
        }
        assert_eq!(
            history().await["items"],
            before,
            "refused calls must not write audit records"
        );
        // An edit names its node's kind: what the kind takes is admitted, and nothing else.
        let id = fixture.success("research_upsert", component).await;
        let edit = json!({"record":{"kind":"component","id":id,"title":"request parser"}});
        fixture.success("research_upsert", edit).await;
        let foreign = json!({"record":{"kind":"component","id":id,"scope":"code"}});
        let refused = fixture.call("research_upsert", foreign).await;
        let error = refused["error"].as_str().unwrap();
        assert!(
            error.contains("`/record` failed: property `scope`"),
            "{error}"
        );
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
            let unrested = json!({"record":record,"note":"n"});
            let refused = fixture.call("research_set_status", unrested).await;
            let error = refused["error"].as_str().unwrap();
            assert!(error.contains(refusal), "{error}");
            let read = serde_json::from_value::<StatusChange>(record.clone());
            let error = read.unwrap_err().to_string();
            assert!(error.contains(&Error::Unrested.to_string()), "{error}");
        }
        fixture.close().await;
    }
}
