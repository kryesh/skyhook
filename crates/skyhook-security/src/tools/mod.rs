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
                    $( Self::$name => skyhook::tool::result_schema::<<result::$Variant as super::Returns>::Value>(), )*
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
            type Error = skyhook::tool::AdmissionError;

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

mod environment;
mod input;
mod query;
mod report;
mod research;

use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use skyhook::tool::{
    AdmissionError, RegistryError, ToolContext, ToolError, ToolOptions, ToolPlacement,
    ToolRegistry, ToolRegistryBuilder,
    policy::{Capability, CapabilitySet, ResourceId},
};

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
    /// On a thread of its own, outside the writer's turn: most of the action's work
    /// is outside the database (artifact files), so it must not keep other writers
    /// waiting; each transaction it makes still has the writer's connection to itself.
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
                let store = context
                    .extension()
                    .map(Store::of)
                    .ok_or_else(|| ToolError::failed("this session keeps no security research"))?;
                let placement = action.placement();
                let worker = store.clone();
                let work = move || {
                    let actor = context.agent().clone();
                    execute(&worker, &actor, &context, action)
                };
                match placement {
                    Placement::Reader => store.reading(work).await,
                    Placement::Writer => store.writing(work).await,
                    Placement::Outside => skyhook::blocking(work).await,
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
pub fn registry() -> Result<ToolRegistry, RegistryError> {
    let mut builder = ToolRegistryBuilder::default();
    research::register(&mut builder)?;
    query::register(&mut builder)?;
    environment::register(&mut builder)?;
    report::register(&mut builder)?;
    Ok(builder.build())
}

#[cfg(test)]
mod tests {
    use std::{future::Future, sync::Arc, time::Duration};

    use serde_json::json;
    use skyhook::{
        agent::SessionHandle,
        tool::policy::{Approval, AuthorizationRequest, Policy, PolicyDecision},
    };

    use super::*;
    use crate::{
        capabilities::SecurityCapability,
        store::{PocPolicy, RunOptions, tests::revision},
    };

    async fn bounded<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(30), future)
            .await
            .expect("tool test deadline")
    }

    struct Fixture {
        _root: tempfile::TempDir,
        registry: ToolRegistry,
        session: SessionHandle,
    }
    impl Fixture {
        async fn new(capabilities: CapabilitySet) -> Self {
            Self::with_policy(capabilities, Arc::new(skyhook::tool::policy::AllowAll)).await
        }

        async fn with_policy(capabilities: CapabilitySet, policy: Arc<dyn Policy>) -> Self {
            let root = tempfile::tempdir().unwrap();
            let registry = registry().unwrap();
            let harness = crate::store::tests::harness(root.path())
                .capabilities(capabilities)
                .policy(policy)
                .tools(registry.clone())
                .build()
                .await
                .unwrap();
            let session = harness.new_session().await.unwrap();
            let store = Store::of(session.store().extension().unwrap());
            tokio::task::spawn_blocking(move || {
                store.start_run(RunOptions::default(), &revision(), PocPolicy::Yes)
            })
            .await
            .unwrap()
            .unwrap();
            Self {
                _root: root,
                registry,
                session,
            }
        }

        async fn script(&self, source: impl Into<String>) -> Value {
            bounded(self.session.run_script(source))
                .await
                .unwrap()
                .value["value"]
                .clone()
        }

        async fn call(&self, name: &str, action: Value) -> Value {
            self.script(format!("return await tool.{name}({action});"))
                .await
        }

        async fn success(&self, name: &str, action: Value) -> Value {
            let response = self.call(name, action).await;
            assert_ne!(response["state"], "failed", "{response}");
            assert!(response["error"].is_null(), "{response}");
            response["result"].clone()
        }

        fn schema(&self, capabilities: &CapabilitySet, tool: &str) -> Option<Value> {
            let surface = self
                .registry
                .surface_for_agent(capabilities, self.session.root_agent());
            Some(surface.get(tool)?.input_schema.clone())
        }

        fn actions(&self, capabilities: &CapabilitySet, tool: &str) -> Option<Vec<String>> {
            let schema = self.schema(capabilities, tool)?;
            let actions = schema["properties"]["action"]["enum"].as_array()?;
            let mut names: Vec<_> = (actions.iter())
                .map(|name| name.as_str().unwrap().to_owned())
                .collect();
            names.sort();
            Some(names)
        }

        /// One path reaching one component with one flaw it contributes to, raised as an
        /// assessed candidate; `expose` false leaves the flaw undecided.
        async fn discover(&self, expose: bool) -> Value {
            self.script(format!(r#"
            const research = async action => (await tool.research(action)).unwrap();
            const at = (file, line) => ({{file, start_line: line, end_line: line}});
            const source = await research({{action:'add_source',kind:'network',trust:'unauthenticated',entry:at('input.c',1),description:'raw request bytes'}});
            const component = await research({{action:'add_component',name:'parser',description:'request parser',files:['input.c']}});
            const path = await research({{action:'add_path',description:'raw request parsing',sources:[source],reaches:[{{component,note:'parse is called on every request'}}]}});
            const flaw = await research({{action:'add_flaw',component,title:'Unchecked copy',statement:'the request length is copied into a fixed buffer',operation:'memory_write',deficiency:{{guard:'bounds_check',deficiency:'missing'}},effects:[{{property:'integrity',note:'adjacent memory'}}],evidence:[{{location:at('input.c',8),role:'operation',stance:'supports',note:'no bounds check'}}]}});
            if ({expose}) await research({{action:'add_exposure',flaw,path,outcome:'contributes',conditions:'the request sets the length',note:'direct call',route:[{{location:at('input.c',3),basis:'inspected',note:'length passed through'}}]}});
            const candidate = await research({{action:'add_candidate',title:'Unchecked copy',description:'request length overflows a fixed buffer',cwe:120,members:[{{flaw,step:null}}],paths:[path]}});
            await research({{action:'assess',candidate,vector:'CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N',confidence:'established',rationale:'unauthenticated request controls memory corruption'}});
            return {{source,path,component,flaw,candidate}};
        "#)).await
        }

        async fn close(self) {
            bounded(self.session.shutdown()).await.unwrap();
        }
    }

    const RESEARCH_TOOLS: [&str; 4] = [
        "research",
        "research_query",
        "environment",
        "finding_report",
    ];

    fn capabilities(extra: impl IntoIterator<Item = Capability>) -> CapabilitySet {
        [Capability::Read].into_iter().chain(extra).collect()
    }

    fn all_capabilities() -> CapabilitySet {
        capabilities(
            [Capability::Write]
                .into_iter()
                .chain(SecurityCapability::ALL.map(SecurityCapability::capability)),
        )
    }

    /// A prompt's first JS template.
    fn template(prompt: &str) -> &str {
        let script = prompt.split("```js\n").nth(1).unwrap();
        script.split("```").next().unwrap()
    }

    /// A prompt's first JS template without its first line, which declares its inputs.
    fn template_body(prompt: &str) -> &str {
        template(prompt).split_once('\n').unwrap().1
    }

    /// Each capability admits its action group at the surface, and the executor
    /// refuses an action outside the surfaced schema before it reaches the store.
    #[tokio::test]
    async fn actions_follow_capabilities_at_the_surface_and_the_executor() {
        let [research, verdict, test] = [
            SecurityCapability::Research,
            SecurityCapability::Verdict,
            SecurityCapability::Test,
        ]
        .map(SecurityCapability::capability);
        let fixture = Fixture::new(all_capabilities()).await;
        let read = capabilities([]);
        let researcher = capabilities([research.clone()]);
        let reviewer = capabilities([verdict.clone()]);
        let writer = capabilities([verdict.clone(), Capability::Write]);
        for (granted, tool, actions) in [
            (&read, "research", None),
            (&read, "finding_report", None),
            (&read, "environment", None),
            (&researcher, "environment", Some(&["add_environment"][..])),
            (
                &capabilities([Capability::Write]),
                "environment",
                Some(&["list"][..]),
            ),
            (&reviewer, "research", None),
            (&reviewer, "finding_report", None),
            (
                &capabilities([test.clone()]),
                "research",
                Some(&["record_poc", "start_poc"][..]),
            ),
            (&writer, "finding_report", Some(&["populate"][..])),
            (
                &capabilities([SecurityCapability::Coordinate.capability()]),
                "finding_report",
                Some(&["generate"][..]),
            ),
        ] {
            assert_eq!(
                fixture.actions(granted, tool),
                actions.map(|actions| actions.iter().map(|a| a.to_string()).collect()),
                "{tool}"
            );
        }
        // Only the root mode schedules the run and writes its reports out.
        for (mode, granted) in crate::presets::modes() {
            let granted: CapabilitySet = granted.capabilities.into();
            let offers = |tool, action| {
                (fixture.actions(&granted, tool).unwrap_or_default().iter()).any(|a| a == action)
            };
            let root = mode == crate::presets::root();
            assert_eq!(offers("research_query", "startable"), root, "{mode}");
            assert_eq!(offers("finding_report", "generate"), root, "{mode}");
        }
        // A validator decides verdicts; a tester records what it observes and plans
        // follow-ups, but never decides verdicts.
        for (granted, with, without) in [
            (&researcher, &["add_candidate"][..], &["verdict"][..]),
            (
                &capabilities([research.clone(), verdict]),
                &["add_candidate", "reassess", "verdict"],
                &["record_poc"],
            ),
            (
                &capabilities([research, test]),
                &[
                    "add_evidence",
                    "add_unit",
                    "record_poc",
                    "start_poc",
                    "undermine",
                ],
                &["verdict", "reassess"],
            ),
        ] {
            let actions = fixture.actions(granted, "research").unwrap();
            assert!(
                with.iter().all(|a| actions.iter().any(|b| b == a))
                    && !without.iter().any(|a| actions.iter().any(|b| b == a)),
                "{actions:?}"
            );
        }
        fixture.close().await;

        let fixture = Fixture::new(researcher).await;
        assert_eq!(
            fixture.script("return typeof tool.finding_report;").await,
            "undefined"
        );
        let history = || fixture.success("research_query", json!({"action":"history"}));
        let before = history().await["items"].clone();
        let denied = fixture
            .call(
                "research",
                json!({"action":"verdict","candidate":1,"verdict":{"kind":"unproven"},"rationale":"x"}),
            )
            .await;
        assert_eq!(denied["state"], "failed", "{denied}");
        assert_eq!(
            history().await["items"],
            before,
            "refused actions must not write audit records"
        );
        fixture.close().await;
    }

    /// Research is recorded as the calling agent's, and queries read it back by their filters.
    #[tokio::test]
    async fn research_is_attributed_to_the_calling_agent() {
        let fixture = Fixture::new(capabilities([SecurityCapability::Research.capability()])).await;
        let ids = fixture.discover(true).await;
        let listed = fixture
            .success(
                "research_query",
                json!({"action":"candidates","filter":{"path":ids["path"],"severity":"critical","operation":"memory_write"}}),
            )
            .await;
        assert_eq!(listed["items"][0]["id"], ids["candidate"]);
        assert_eq!(listed["items"][0]["status"], "shortlisted");
        let history = fixture
            .success(
                "research_query",
                json!({"action":"history","subject":{"kind":"candidate","id":ids["candidate"]}}),
            )
            .await;
        assert!(!history["items"].as_array().unwrap().is_empty());
        for event in history["items"].as_array().unwrap() {
            assert_eq!(event["actor"], fixture.session.root_agent().to_string());
        }
        fixture.close().await;
    }

    /// A finding's write-up round-trips through the tools: populated, generated, read back
    /// whole in the shape populate takes and populated again. Research bookkeeping and report
    /// publication ask for no approval beyond the tools' own capabilities: reports are the
    /// session's files, not the workspace's.
    #[tokio::test]
    async fn report_tools_round_trip_the_write_up_without_filesystem_approval() {
        struct NoWrites;
        impl Policy for NoWrites {
            fn authorize(&self, request: AuthorizationRequest) -> Approval<'_> {
                assert!(request.permissions.iter().all(|permission| !matches!(
                    permission.capability,
                    Capability::Host(_) | Capability::Write
                )));
                Approval::Decided(PolicyDecision::allow())
            }
        }
        let fixture = Fixture::with_policy(all_capabilities(), Arc::new(NoWrites)).await;
        let ids = fixture.discover(true).await;
        let candidate = ids["candidate"].clone();
        fixture.success("research", json!({"action":"verdict","candidate":candidate,"verdict":{"kind":"validated"},"rationale":"reviewed the traced route"})).await;
        let report = json!({
            "action":"populate","root":ids["flaw"],"title":"Unchecked request copy", "summary":"Attacker-controlled length reaches a fixed-size copy.",
            "analysis":"The traced route reaches the copy without a length check.","impact":"Out-of-bounds write can corrupt process memory.",
            "remediation":"Reject lengths exceeding the destination capacity before copying.",
            "reproduction":{"prerequisites":["Build the target with memory checking in a disposable environment."],
                "steps":[{"instruction":"Send the oversized request to the local test listener.","expected_result":"The bounds checker reports the copy write."}],
                "outcome":{"kind":"unobserved","expected_result":"The oversized request reaches the unchecked copy.","limitations":"Source review only; this report has no empirical test."},
                "cleanup":["Stop the disposable test listener."]}
        });
        let mut stepless = report.clone();
        stepless["reproduction"]["steps"] = json!([]);
        let refused = fixture.call("finding_report", stepless).await;
        let error = refused["error"].as_str().unwrap_or_default();
        assert!(error.contains("`/reproduction/steps`"), "{refused}");
        let first = fixture.success("finding_report", report).await;
        let generated = fixture
            .success("finding_report", json!({"action":"generate"}))
            .await;
        assert_eq!(generated["reports"], json!([ids["flaw"]]));

        let read = json!({"action":"finding","root":ids["flaw"]});
        let mut write_up =
            fixture.success("research_query", read.clone()).await["write_up"].clone();
        write_up["notes"] = json!([{"candidate":candidate,"note":"Any request reaches it."}]);
        // A write-up beyond the preview budget reads back whole.
        write_up["analysis"] = json!("Each request reaches the copy unchecked.\n".repeat(2000));
        let mut populate = write_up.clone();
        populate["action"] = json!("populate");
        let repeated = fixture.success("finding_report", populate).await;
        assert_eq!(
            (&repeated["id"], &repeated["status"]),
            (&first["id"], &json!("current"))
        );
        // The view a model reads of the call shows the write-up whole.
        let view = fixture
            .script(format!(
                "await tool.research_query({read}); const jobs = (await tool.jobs({{all: true}})).unwrap().jobs; \
                 return await tool.jobs({{job: jobs.filter(j => j.meta.tool === 'research_query').pop().id}});"
            ))
            .await;
        let stored = view["result"]["write_up"].clone();
        assert_eq!(stored, write_up);
        fixture.close().await;
    }

    /// An input refuses a field it does not define at any depth, so a misspelt or renamed
    /// field is an error rather than silently dropped, and a value its type cannot hold by
    /// the field's path.
    #[tokio::test]
    async fn inputs_refuse_unknown_fields() {
        let fixture = Fixture::new(capabilities(
            [SecurityCapability::Research, SecurityCapability::Verdict]
                .map(SecurityCapability::capability),
        ))
        .await;
        let unit = json!({"action":"add_unit","kind":"question","question":"can the copy overrun?","stopping":"a counterexample or a proof"});
        let mut unknown = unit.clone();
        unknown["priority"] = json!("high");
        let refused = fixture.call("research", unknown).await;
        assert_eq!(refused["state"], "failed", "{refused}");
        fixture.success("research", unit).await;
        // Schema validation refuses a nested id below 1 at its path, before deserializing.
        let zero = fixture
            .call(
                "research",
                json!({"action":"add_path","description":"entry","sources":[0],"reaches":[]}),
            )
            .await;
        let error = |response: &Value| response["error"].as_str().unwrap_or_default().to_owned();
        assert!(
            error(&zero).contains("`/sources/0`") && error(&zero).contains("minimum 1"),
            "{zero}"
        );
        // An action's arguments sit beside it: a missing required one is refused at its
        // name, and so is another action's, even null.
        let missing = fixture
            .call("research_query", json!({"action":"unit"}))
            .await;
        assert!(error(&missing).contains("`/unit`"), "{missing}");
        for argument in [json!({}), Value::Null] {
            let stray = fixture
                .call(
                    "research_query",
                    json!({"action":"synthesis","page":argument}),
                )
                .await;
            assert!(error(&stray).contains("`/page`"), "{stray}");
        }
        // A union admits only the action's own alternative.
        let other = fixture
            .call(
                "research_query",
                json!({"action":"flaws","filter":{"status":"validated"}}),
            )
            .await;
        assert!(error(&other).contains("`/filter/status`"), "{other}");
        // A resolution carries only what its kind takes: a deployment fact only when the
        // condition holds, a flaw only when it fails.
        for resolution in [
            json!({"kind":"fails","deployment":1}),
            json!({"kind":"asked","flaw":1}),
        ] {
            let refused = fixture
                .call(
                    "research",
                    json!({"action":"resolve","premise":1,"resolution":resolution,"note":"n"}),
                )
                .await;
            assert!(error(&refused).contains("`/resolution`"), "{refused}");
        }
        // A verdict carries only what its kind takes: a deployment fact only on a policy
        // exclusion.
        for verdict in [
            json!({"kind":"disproved","deployment":1}),
            json!({"kind":"validated","deployment":1}),
        ] {
            let refused = fixture
                .call(
                    "research",
                    json!({"action":"verdict","candidate":1,"verdict":verdict,"rationale":"e"}),
                )
                .await;
            assert!(error(&refused).contains("`/verdict`"), "{refused}");
        }
        fixture
            .success("research_query", json!({"action":"history"}))
            .await;
        fixture.close().await;
    }

    /// The names an object literal at the start of `source` spells, spreads aside.
    fn literal_keys(source: &str) -> Vec<&str> {
        let (mut entries, mut depth, mut quote, mut start) = (Vec::new(), 0, None, 1);
        for (at, c) in source.char_indices() {
            match (quote, c) {
                (Some(open), c) if c == open => quote = None,
                (Some(_), _) => {}
                (None, '"' | '\'' | '`') => quote = Some(c),
                (None, '{' | '(' | '[') => depth += 1,
                (None, '}' | ')' | ']') => {
                    depth -= 1;
                    if depth == 0 {
                        entries.push(&source[start..at]);
                        break;
                    }
                }
                (None, ',') if depth == 1 => {
                    entries.push(&source[start..at]);
                    start = at + 1;
                }
                _ => {}
            }
        }
        (entries.into_iter().map(str::trim))
            .filter(|entry| !entry.is_empty() && !entry.starts_with("..."))
            .map(|entry| entry.split(':').next().unwrap().trim())
            .collect()
    }

    /// Every research tool call a mode's templates spell, and every action its prose names
    /// after the tool (`research` `outcome`), names an action that mode surfaces, a call with
    /// only arguments that action's signature lists. The template tests run with every
    /// capability, so only this ties a call to the grant of the mode whose prompt holds it;
    /// argument values are checked by running the templates.
    #[tokio::test]
    async fn every_template_call_names_an_action_its_mode_surfaces() {
        let fixture = Fixture::new(all_capabilities()).await;
        let mut checked = 0;
        for poc in [PocPolicy::Yes, PocPolicy::No] {
            for (mode, granted) in crate::presets::for_policy(crate::presets::modes(), poc) {
                let instructions = granted.instructions.unwrap();
                let granted: CapabilitySet = granted.capabilities.into();
                for (at, _) in instructions.as_str().match_indices("tool.") {
                    let call = &instructions.as_str()[at + "tool.".len()..];
                    let Some((tool, arguments)) =
                        (call.split_once('(')).filter(|(tool, _)| RESEARCH_TOOLS.contains(tool))
                    else {
                        continue;
                    };
                    let action = (arguments.strip_prefix("{action: \""))
                        .and_then(|rest| rest.split_once('"'))
                        .unwrap_or_else(|| panic!("{mode}: no literal action in {call:.80}"))
                        .0;
                    let schema = fixture.schema(&granted, tool);
                    let signature = schema.as_ref().and_then(|schema| {
                        let description = schema["properties"]["action"]["description"].as_str()?;
                        (description.lines())
                            .find_map(|line| line.strip_prefix(action)?.strip_prefix('('))
                    });
                    let takes = signature
                        .unwrap_or_else(|| panic!("{mode}: {tool} offers no {action}"))
                        .split_once(')')
                        .unwrap()
                        .0;
                    let takes: Vec<_> =
                        takes.split(", ").map(|a| a.trim_end_matches('?')).collect();
                    for key in literal_keys(arguments) {
                        assert!(
                            key == "action" || takes.contains(&key),
                            "{mode}: {tool} {action} takes no {key}"
                        );
                    }
                    checked += 1;
                }
                for tool in RESEARCH_TOOLS {
                    let named = format!("`{tool}`");
                    for (at, _) in instructions.as_str().match_indices(&named) {
                        let Some((action, _)) = (instructions.as_str()[at + named.len()..])
                            .trim_start()
                            .strip_prefix('`')
                            .and_then(|rest| rest.split_once('`'))
                        else {
                            continue;
                        };
                        let offered = fixture.actions(&granted, tool).unwrap_or_default();
                        assert!(
                            offered.iter().any(|offered| offered == action),
                            "{mode}: prose names {tool} {action}, which it does not offer"
                        );
                        checked += 1;
                    }
                }
            }
        }
        assert_ne!(checked, 0);
        fixture.close().await;
    }

    /// The report script reads a root's reportable scenarios, their tests and the previous
    /// finding's write-up, and lists the fields still empty, before and after a finding.
    #[tokio::test]
    async fn the_report_script_reads_the_group_and_its_previous_write_up() {
        let fixture = Fixture::new(all_capabilities()).await;
        let ids = fixture.discover(true).await;
        let candidate = ids["candidate"].clone();
        fixture.success("research", json!({"action":"verdict","candidate":candidate,"verdict":{"kind":"validated"},"rationale":"reviewed the traced route"})).await;
        let body = template_body(include_str!("../prompts/report.md"));
        let script = format!("const root = {};\n{body}", ids["flaw"]);
        let first = fixture.script(script.clone()).await;
        assert_eq!(first["scenarios"][0]["id"], candidate, "{first}");
        assert_eq!(first["prior"], Value::Null, "{first}");
        let text = "Settled from the traced route.";
        let report = json!({"action":"populate","root":ids["flaw"],"title":text,"summary":text,"analysis":text,"impact":text,"remediation":text,
            "reproduction":{"prerequisites":[],"steps":[{"instruction":text,"expected_result":text}],
                "outcome":{"kind":"unobserved","expected_result":text,"limitations":text},"cleanup":[]}});
        let finding = fixture.success("finding_report", report).await;
        let again = fixture.script(script).await;
        assert_eq!(
            (&again["prior"], &again["empty"]),
            (
                &json!({"id": finding["id"], "status": "current"}),
                &json!([format!("notes: {candidate}")])
            ),
            "{again}"
        );
        fixture.close().await;
    }

    /// The validation script lists what the candidate's verdict still waits on: here only an
    /// open condition gating its scenario.
    #[tokio::test]
    async fn the_validation_script_lists_an_open_condition() {
        let fixture = Fixture::new(all_capabilities()).await;
        let ids = fixture.discover(true).await;
        let condition = fixture
            .success("research", json!({"action":"add_premise","on":{"kind":"candidate","id":ids["candidate"],"gates":{"kind":"scenario"}},"kind":"state","statement":"the listener is enabled"}))
            .await;
        let body = template_body(include_str!("../prompts/validate.md"));
        let result = fixture
            .script(format!("const candidate = {};\n{body}", ids["candidate"]))
            .await;
        assert_eq!(
            result["gaps"],
            json!([format!(
                "premise {condition} (gates the scenario) is open: the listener is enabled"
            )]),
            "{result}"
        );
        fixture.close().await;
    }

    /// The target's mapping script records a component per entry of the map with the files
    /// its patterns match that no earlier entry took.
    #[tokio::test]
    async fn the_mapping_script_assigns_each_file_once() {
        let fixture = Fixture::new(all_capabilities()).await;
        for file in ["src/net/a.c", "src/net/b.c", "src/parse.c"] {
            let path = fixture._root.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "int x;\n").unwrap();
        }
        let target = include_str!("../prompts/target.md");
        let body = template_body(target.split_once("```\n").unwrap().1);
        let map = r#"[{name: "net", description: "network input", patterns: ["src/net/**"]}, {name: "core", description: "the rest", patterns: ["src/**"]}]"#;
        let result = fixture.script(format!("const map = {map};\n{body}")).await;
        let mut files = Vec::new();
        for id in result["components"].as_array().unwrap() {
            let dossier = json!({"action":"dossier","component":id});
            files.push(fixture.success("research_query", dossier).await["files"].clone());
        }
        assert_eq!(
            files,
            [
                json!(["src/net/a.c", "src/net/b.c"]),
                json!(["src/parse.c"])
            ],
            "{result}"
        );
        fixture.close().await;
    }

    /// The paths script groups sources by the handoff each records, whatever its file is
    /// called, names the component owning it, and lists sources without one.
    #[tokio::test]
    async fn the_paths_script_groups_sources_by_their_recorded_handoff() {
        let fixture = Fixture::new(capabilities([SecurityCapability::Research.capability()])).await;
        let at = |file: &str, line: u32| json!({"file": file, "start_line": line, "end_line": line, "function": null});
        let component = fixture
            .success("research", json!({"action":"add_component","name":"parser","description":"request parser","files":["src/a b:c.c"]}))
            .await;
        let source = |line: u32, handoff: Value| json!({"action":"add_source","kind":"network","trust":"unauthenticated","entry":at("src/entry.c", line),"description":"request","handoff":handoff});
        let mut ids = Vec::new();
        for (line, handoff) in [
            (1, json!({"at": at("src/a b:c.c", 9), "calls": "parse"})),
            (2, json!({"at": at("src/a b:c.c", 9), "calls": "parse"})),
            (3, json!({"at": at("vendor/x.c", 4), "calls": "decode"})),
            (4, Value::Null),
        ] {
            ids.push(fixture.success("research", source(line, handoff)).await);
        }
        let body = template_body(include_str!("../prompts/paths.md"));
        let result = fixture
            .script(format!("const workers = 1, inputs = [];\n{body}"))
            .await;
        assert_eq!(
            result["handoffs"],
            json!([
                {"at": at("src/a b:c.c", 9), "calls": "parse", "component": component, "sources": [ids[0], ids[1]]},
                {"at": at("vendor/x.c", 4), "calls": "decode", "component": null, "sources": [ids[2]]},
            ])
        );
        assert_eq!(result["unmatched"], json!([ids[3]]));
        fixture.close().await;
    }

    /// A script reads an optional field a result omits as `null`, as its action's result
    /// type declares it, in a record and in a page of them.
    #[tokio::test]
    async fn scripts_read_an_omitted_optional_result_field_as_null() {
        let fixture = Fixture::new(all_capabilities()).await;
        let result = fixture.script(r#"
            const at = {file: 'input.c', start_line: 1, end_line: 1};
            const source = (await tool.research({action: 'add_source', kind: 'network', trust: 'unauthenticated', entry: at, description: 'request'})).unwrap();
            const one = (await tool.research_query({action: 'source', source})).unwrap();
            const page = (await tool.research_query({action: 'sources'})).unwrap();
            return [one.handoff, page.items[0].handoff, page.next_after].map(field => field === null);
        "#).await;
        assert_eq!(result, json!([true, true, true]));
        fixture.close().await;
    }

    /// The scheduler template rendered under `poc`, without its inputs line.
    fn scheduler(poc: PocPolicy) -> String {
        let orchestrate = crate::presets::rendered(include_str!("../prompts/orchestrate.md"), poc);
        template_body(&orchestrate).to_owned()
    }

    /// Runs `body` after `stubs`, which declare the `tool` and `receive` it uses and its
    /// inputs, and returns `returned`, which reads the body's result as `outcome`.
    async fn run_template(fixture: &Fixture, stubs: &str, body: &str, returned: &str) -> Value {
        fixture
            .script(format!(
                "{stubs}\nconst outcome = await (async () => {{ {body} }})();\nreturn {returned};"
            ))
            .await
    }

    /// Plans `units` follow-ups.
    async fn follow_ups(fixture: &Fixture, units: usize) {
        fixture
            .script(format!(
                "for (let i = 0; i < {units}; i++) (await tool.research({{action: 'add_unit', kind: 'follow_up', question: `question ${{i}}`, stopping: 'settled'}})).unwrap();"
            ))
            .await;
    }

    /// Stubs whose every child fails, counting launches and the rows the scheduler reads.
    const FAILING: &str = "let rows = 0, launched = 0;
        const real = globalThis.tool;
        const tool = {
            jobs: real.jobs, research: real.research,
            agent: async () => (launched++, {id: 8, state: 'failed', error: 'stub'}),
            research_query: async action => { const result = await real.research_query(action); try { rows += result.unwrap().items?.length ?? 0; } catch {} return result; },
        };
        const receive = () => new Promise(() => {});";

    /// The orchestrator's scheduler reads only what it starts: with one worker and a long
    /// queue, the rows it reads grow with the work, not with its square.
    #[tokio::test]
    async fn the_scheduler_reads_a_bounded_page_per_launch() {
        const UNITS: usize = 30;
        let fixture = Fixture::new(all_capabilities()).await;
        follow_ups(&fixture, UNITS).await;
        let stubs = format!("{FAILING}\nconst workers = 1;");
        let returned = "{rows, launched, left: outcome.left}";
        let result = run_template(&fixture, &stubs, &scheduler(PocPolicy::No), returned).await;
        // Every follow-up, each launched twice before it is blocked.
        assert_eq!(result["launched"], json!(2 * UNITS), "{result}");
        assert_eq!(result["left"], json!({}), "{result}");
        let rows = result["rows"].as_u64().unwrap() as usize;
        assert!(rows <= 4 * UNITS, "{rows} rows for {UNITS} units");
        fixture.close().await;
    }

    /// A pool wider than the largest page still reads its work a page at a time.
    #[tokio::test]
    async fn the_scheduler_pages_a_pool_wider_than_a_page() {
        let fixture = Fixture::new(all_capabilities()).await;
        follow_ups(&fixture, 2).await;
        let stubs = format!("{FAILING}\nconst workers = 1001;");
        let returned = "{launched, left: outcome.left}";
        let result = run_template(&fixture, &stubs, &scheduler(PocPolicy::No), returned).await;
        assert_eq!(result, json!({"launched": 4, "left": {}}));
        fixture.close().await;
    }

    /// A child that asks keeps its `tool.agent` call, and so its unit, running until it
    /// finishes, and its real outcome is the attempt's: the unit is neither retried nor
    /// blocked.
    #[tokio::test]
    async fn the_scheduler_holds_an_asking_childs_unit_until_it_finishes() {
        const UNITS: usize = 6;
        // Every launch but the asking child's retry.
        const LAUNCHES: usize = 2 * UNITS - 1;
        let fixture = Fixture::new(all_capabilities()).await;
        follow_ups(&fixture, UNITS).await;
        // The asking child finishes after every other launch, ending its unit.
        let stubs = format!(
            "let launched = 0, asking = null, finish;
            const held = [], real = globalThis.tool;
            const finished = new Promise(resolve => finish = resolve);
            const answered = async () => {{
                await finished;
                (await real.research({{action: 'outcome', unit: asking, outcome: 'deferred', note: 'answered'}})).unwrap();
                return {{id: 7, state: 'completed', result: {{reply: 'settled'}}}};
            }};
            const tool = {{
                jobs: real.jobs, research: real.research,
                research_query: async action => {{
                    if (action.action === 'startable' && asking !== null && launched < {LAUNCHES}) held.push(action.running.some(s => s.kind === 'unit' && s.id === asking));
                    return real.research_query(action);
                }},
                agent: async ({{prompt}}) => {{
                    if (++launched === {LAUNCHES}) finish();
                    if (asking !== null || !prompt.startsWith('Follow-up')) return {{id: 8, state: 'failed', error: 'stub'}};
                    asking = Number(prompt.match(/\\d+/)[0]);
                    return answered();
                }},
            }};
            const receive = () => new Promise(() => {{}});
            const workers = 2;"
        );
        let returned = "{launched, held, left: outcome.left, unit: (await real.research_query({action: 'unit', unit: asking})).unwrap()}";
        let result = run_template(&fixture, &stubs, &scheduler(PocPolicy::No), returned).await;
        assert_eq!(result["launched"], json!(LAUNCHES), "{result}");
        assert_eq!(result["left"], json!({}), "{result}");
        assert_eq!(result["unit"]["outcome"]["note"], "answered", "{result}");
        let held = result["held"].as_array().unwrap();
        assert!(
            !held.is_empty() && held.iter().all(|running| running == true),
            "{result}"
        );
        fixture.close().await;
    }

    /// The store passes over the tests the scheduler may not start: one slot and an
    /// approval reach the approved test behind many unapproved ones, then the report
    /// behind them all, a page per launch; with tests still held, it waits for steering.
    #[tokio::test]
    async fn the_scheduler_reaches_work_behind_held_tests_in_one_read() {
        use crate::store::{
            PocResult, Store,
            tests::{poc, root_of, validated},
        };
        const HELD: usize = 30;
        let fixture = Fixture::new(all_capabilities()).await;
        let store = Store::of(fixture.session.store().extension().unwrap());
        let agent = fixture.session.root_agent().clone();
        let (approved, root) = tokio::task::spawn_blocking(move || {
            let actor = agent;
            for _ in 0..HELD {
                validated(&store, &actor);
            }
            let approved = validated(&store, &actor);
            let tested = validated(&store, &actor);
            poc(&store, &actor, tested, PocResult::Confirmed);
            store.set_poc_policy(PocPolicy::Ask).unwrap();
            (json!(approved), json!(root_of(&store, tested)))
        })
        .await
        .unwrap();
        let stubs = "let reads = 0;
            const started = [], real = globalThis.tool;
            const tool = {
                jobs: real.jobs, research: real.research,
                research_query: action => (action.action === 'startable' && reads++, real.research_query(action)),
                agent: async ({prompt}) => (started.push(prompt.match(/^\\D+\\d+/)[0]), {id: 8, state: 'failed', error: 'stub'}),
            };
            const receive = () => reads >= 5 ? Promise.resolve({stop: true}) : new Promise(() => {});
            const workers = 1, artifacts = 'artifacts';";
        let body = scheduler(PocPolicy::Ask);
        let inputs = [
            (
                "new Set(/* under ask: candidates the user approved */)",
                format!("new Set([{approved}])"),
            ),
            ("let pocSlots = 0;", "let pocSlots = 1;".to_owned()),
        ];
        let body = inputs.iter().fold(body, |body, (input, value)| {
            assert!(body.contains(input), "{input}");
            body.replace(input, value)
        });
        let returned = "{reads, started, left: Object.keys(outcome.left)}";
        let result = run_template(&fixture, stubs, &body, returned).await;
        // What was tried twice and is still owed is left.
        let (test, report) = (
            format!("Demonstrate candidate {approved}"),
            format!("Report root flaw {root}"),
        );
        assert_eq!(
            result,
            json!({"reads": 5, "started": [&test, &test, &report, &report], "left": ["poc", "report"]})
        );
        fixture.close().await;
    }

    /// An owed check's child is briefed with its contract, and plans the check by naming it.
    #[tokio::test]
    async fn the_scheduler_briefs_an_owed_check_with_its_contract() {
        use crate::store::{
            Store,
            tests::{component, path, reviewed},
        };
        let fixture = Fixture::new(all_capabilities()).await;
        let store = Store::of(fixture.session.store().extension().unwrap());
        let agent = fixture.session.root_agent().clone();
        tokio::task::spawn_blocking(move || {
            let actor = agent;
            let decoder = component(&store, &actor, "decoder");
            path(&store, &actor, "stream", &[decoder]);
            reviewed(&store, &actor);
        })
        .await
        .unwrap();
        let stubs = "const started = [], real = globalThis.tool;
            (await real.research({action: 'retry_site', location: {file: 'decoder/lib.c', start_line: 9, end_line: 9, function: 'DecodeStream'}, status: 'NEEDS_MORE_INPUT', class: 'invalid'})).unwrap();
            const owed = (await real.research_query({action: 'startable', limit: 1000})).unwrap().items.find(e => e.phase === 'check').subject;
            let planned;
            const tool = {
                jobs: real.jobs, research: real.research, research_query: real.research_query,
                agent: async ({prompt}) => {
                    started.push(prompt.match(/^\\D+\\d+/)[0]);
                    const contract = prompt.match(/^Check contract (\\d+)\\./)?.[1];
                    if (contract) planned = (await real.research({action: 'add_check', contract: Number(contract), question: 'does it keep its contract?', stopping: 'a guarantee or a flaw'})).unwrap();
                    return {id: 8, state: 'failed', error: 'stub'};
                },
            };
            const receive = () => new Promise(() => {});
            const workers = 1;";
        let returned = "{owed, started: started, check: (await real.research_query({action: 'unit', unit: planned})).unwrap().check.contract}";
        let result = run_template(&fixture, stubs, &scheduler(PocPolicy::No), returned).await;
        let (contract, unit) = (&result["check"], result["started"][1].as_str().unwrap());
        assert_eq!(result["owed"], json!({"kind": "contract", "id": contract}));
        assert_eq!(
            result["started"],
            json!([format!("Check contract {contract}"), unit, unit])
        );
        assert!(unit.starts_with("Question unit "), "{unit}");
        fixture.close().await;
    }

    /// The component's measuring script sizes each of its files from the directory listings,
    /// leaving out a listed file the component does not hold.
    #[tokio::test]
    async fn the_component_script_measures_its_files() {
        let fixture = Fixture::new(capabilities([SecurityCapability::Research.capability()])).await;
        for (file, bytes) in [
            ("src/a.c", 10),
            ("src/b.c", 3),
            ("src/other.c", 5),
            ("main.c", 1),
        ] {
            let path = fixture._root.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x".repeat(bytes)).unwrap();
        }
        let component = fixture
            .success("research", json!({"action":"add_component","name":"parser","description":"request parser","files":["src/a.c","src/b.c","main.c"]}))
            .await;
        let unit = fixture
            .success(
                "research",
                json!({"action":"add_unit","kind":"sweep","component":component}),
            )
            .await;
        let body = template_body(include_str!("../prompts/component.md"));
        let result = fixture
            .script(format!("const unit = {unit};\n{body}"))
            .await;
        assert_eq!(
            result,
            json!({"component": component, "files": 3, "bytes": 14, "largest": [
                {"file": "src/a.c", "bytes": 10}, {"file": "src/b.c", "bytes": 3}, {"file": "main.c", "bytes": 1},
            ]})
        );
        fixture.close().await;
    }
}
