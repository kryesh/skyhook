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

mod input;
mod query;
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
        store::{Error, LivePolicy, NodeId, NodeView, PathId, RunOptions},
    };

    pub(crate) async fn bounded<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(30), future)
            .await
            .expect("test deadline")
    }

    /// Records one input state, component and path reading it, a flaw on the component and a
    /// candidate of it along the path, assessed critical; binds their ids.
    const DISCOVER: &str = r#"
        const research = async action => (await tool.research(action)).unwrap();
        const at = (file, line) => ({code: {file, start_line: line, end_line: line}});
        const component = await research({action:'upsert',record:{kind:'component',title:'parser',body:'request parser',files:['input.c']}});
        const input = await research({action:'upsert',record:{kind:'state',scope:'input',title:'request',body:'raw request bytes',location:at('input.c',1),trust:'untrusted'}});
        const path = await research({action:'upsert',record:{kind:'path',title:'request parsing',body:'raw request parsing'}});
        await research({action:'link',links:[{path_state:{path,state:input,access:'read',location:at('input.c',1)}},{path_step:{path,component,note:'parse is called on every request'}}]});
        const flaw = await research({action:'upsert',record:{kind:'flaw',title:'Unchecked copy',body:'the request length is copied into a fixed buffer',component,location:at('input.c',8),cwe:120}});
        const candidate = await research({action:'upsert',record:{kind:'candidate',title:'Unchecked copy',body:'request length overflows a fixed buffer',paths:[path],flaws:[flaw]}});
        await research({action:'assess',candidate,vector:'CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N',note:'unauthenticated request controls memory corruption'});
    "#;

    struct Fixture {
        _root: tempfile::TempDir,
        session: SessionHandle,
    }
    impl Fixture {
        async fn new(capabilities: CapabilitySet) -> Self {
            Self::built(capabilities, crate::store::tests::harness, LivePolicy::Yes).await
        }

        /// A run under `live` whose children are [`Children`] deciding by brief with `then`,
        /// with every mode and the capabilities they need.
        async fn with_children(
            live: LivePolicy,
            then: impl Fn(&str) -> Then + Send + Sync + 'static,
        ) -> Self {
            let children = Arc::new(Children(Arc::new(then)));
            let capabilities = capabilities(
                [
                    Capability::Write,
                    Capability::Exec,
                    Capability::Network,
                    Capability::Agents,
                    Capability::Mcp,
                    Capability::Interactive,
                ]
                .into_iter()
                .chain(SecurityCapability::ALL.map(SecurityCapability::capability)),
            );
            let harness = move |root: &std::path::Path| {
                (crate::store::tests::harness_on(root, children))
                    .modes(crate::presets::modes())
                    .prompts(crate::prompts::prompts(live))
            };
            Self::built(capabilities, harness, live).await
        }

        async fn built(
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
            tokio::task::spawn_blocking(move || store.start_run(RunOptions::default(), live))
                .await
                .unwrap()
                .unwrap();
            Self {
                _root: root,
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

        /// Score and validate `candidate` through a reviewing child, [`scripted`] within its
        /// work.
        async fn validate(&self, candidate: &Value) {
            self.script(format!(
                r#"await tool.agent({{mode: "validate", depth: 1, name: "validate-{candidate}", prompt: "Review candidate {candidate}."}});"#
            ))
            .await;
        }

        async fn discover(&self) -> Value {
            self.script(format!(
                "{DISCOVER}\nreturn {{input, path, component, flaw, candidate}};"
            ))
            .await
        }

        async fn close(self) {
            bounded(self.session.shutdown()).await.unwrap();
        }
    }

    const RESEARCH_TOOLS: [&str; 2] = ["research", "research_query"];

    /// The input schema `tool` surfaces to an agent holding `capabilities`, if it surfaces.
    fn schema(registry: &ToolRegistry, capabilities: &CapabilitySet, tool: &str) -> Option<Value> {
        let agent = AgentId::root(skyhook::identity::SessionId::from_bytes([0; 16]));
        let surface = registry.surface_for_agent(capabilities, &agent);
        Some(surface.get(tool)?.input_schema.clone())
    }

    /// The actions `tool` surfaces to an agent holding `capabilities`, sorted.
    fn actions(
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

    /// A script template without its first line, which declares its inputs.
    fn template_body(script: &str) -> &str {
        script.split_once('\n').unwrap().1
    }

    /// Each capability admits its action group at the surface, and the executor refuses an
    /// action outside the surfaced schema before it reaches the store, as it does a field its
    /// action does not define at any depth, so a misspelt or renamed field is an error rather
    /// than silently dropped, another action's argument, even null, a missing one, and a status
    /// its node's kind does not take.
    #[tokio::test]
    async fn actions_follow_capabilities_at_the_surface_and_the_executor() {
        let [research, verdict, test, scheduled, coordinate] =
            SecurityCapability::ALL.map(SecurityCapability::capability);
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
        for (mode, preset) in crate::presets::tests::editions(LivePolicy::Yes) {
            let granted: CapabilitySet = preset.capabilities.into();
            let offers = |tool, action| {
                (actions_of(&granted, tool).unwrap_or_default().iter()).any(|a| a == action)
            };
            let root = mode == crate::presets::root();
            assert_eq!(offers("research_query", "startable"), root, "{mode}");
            let query = schema(&registry, &granted, "research_query").unwrap();
            assert_eq!(query.to_string().contains("startable"), root, "{mode}");
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
        // An edit's kind is its node's: what the kind takes is admitted, and nothing else.
        let id = fixture.success("research", component).await;
        let edit = json!({"action":"upsert","record":{"id":id,"title":"request parser"}});
        fixture.success("research", edit).await;
        let foreign = json!({"action":"upsert","record":{"id":id,"scope":"code"}});
        let refused = fixture.call("research", foreign).await;
        assert_eq!(refused["state"], "failed", "{refused}");
        // A record's field left beside it is pointed inside.
        let beside = json!({"action":"upsert","kind":"path","title":"t","body":"b"});
        let refused = fixture.call("research", beside).await;
        let error = refused["error"].as_str().unwrap();
        assert!(error.contains("goes inside `record`"), "{error}");
        // A check names at least one premise it rests on, missing and empty alike.
        let mut record = json!({"kind":"state","id":1,"status":"checked"});
        for rests_on in [None, Some(json!([]))] {
            if let Some(rests_on) = rests_on {
                record["rests_on"] = rests_on;
            }
            let unrested = json!({"action":"set_status","record":record,"note":"n"});
            let refused = fixture.call("research", unrested).await;
            let error = refused["error"].as_str().unwrap();
            assert!(error.contains(&Error::Unrested.to_string()), "{error}");
        }
        fixture.close().await;
    }

    /// The names an object literal at the start of `source` spells, spreads and computed keys
    /// aside.
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
            .filter(|entry| {
                !entry.is_empty() && !entry.starts_with("...") && !entry.starts_with('[')
            })
            .map(|entry| entry.split(':').next().unwrap().trim())
            .collect()
    }

    /// Every research tool call a mode's templates spell, and every action its prose names
    /// after the tool (`research` `begin`), names an action that mode surfaces, a call with
    /// only arguments that action's signature lists. The template tests run with every
    /// capability, so only this ties a call to the grant of the mode whose prompt holds it;
    /// argument values are checked by running the templates.
    #[test]
    fn every_template_call_names_an_action_its_mode_surfaces() {
        let registry = registry().unwrap();
        let mut checked = 0;
        for live in [LivePolicy::Yes, LivePolicy::No] {
            for (mode, granted) in crate::presets::tests::editions(live) {
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
                    let schema = schema(&registry, &granted, tool);
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
                        let offered = actions(&registry, &granted, tool).unwrap_or_default();
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
    }

    /// The report script reads a root's reportable scenarios with the test each stands on and the
    /// previous report, before and after a report.
    #[tokio::test]
    async fn the_report_script_reads_the_group_and_its_previous_report() {
        let fixture = Fixture::with_children(LivePolicy::Yes, scripted).await;
        let ids = fixture.discover().await;
        let candidate = ids["candidate"].clone();
        let test =
            json!({"action":"test","candidate":candidate,"result":"failed","note":"no crash"});
        fixture.validate(&candidate).await;
        fixture.success("research", test.clone()).await;
        fixture.validate(&candidate).await;
        let settling = fixture.success("research", test).await;
        fixture.success("research", json!({"action":"test","candidate":candidate,"result":"pending","note":"needs a board"})).await;
        let body = template_body(include_str!("../prompts/scripts/report-scenarios.js"));
        let script = format!("const root = {};\n{body}", ids["flaw"]);
        let first = fixture.script(script.clone()).await;
        assert_eq!(first["scenarios"][0]["id"], candidate, "{first}");
        assert_eq!(
            first["scenarios"][0]["test"]["id"], settling,
            "the test the report states: {first}"
        );
        assert_eq!(first["previous"], Value::Null, "{first}");
        let report = json!({"action":"report","root":ids["flaw"],"title":"Unchecked copy","body":"## Summary\n\nAny request overflows."});
        let finding = fixture.success("research", report).await;
        let again = fixture.script(script).await;
        assert_eq!(
            (&again["previous"]["id"], &again["previous"]["current"]),
            (&finding, &json!(true)),
            "{again}"
        );
        fixture.close().await;
    }

    /// The validation script lists what the candidate's verdict still rests on: here an
    /// undecided premise of its flaw, a flaw off every action's steps until an action reaches
    /// its component, and the premises of a path performed twice once.
    #[tokio::test]
    async fn the_validation_script_lists_an_undecided_premise_and_a_flaw_off_its_actions() {
        let fixture = Fixture::new(all_capabilities()).await;
        let ids = fixture.discover().await;
        let premise = fixture
            .success("research", json!({"action":"upsert","record":{"kind":"premise","title":"the listener is enabled","body":"set at startup","scope":"code"}}))
            .await;
        let link = json!({"action":"link","links":[{"flaw_premise":{"flaw":ids["flaw"],"premise":premise}}]});
        fixture.success("research", link).await;
        let body = template_body(include_str!("../prompts/scripts/validate-gaps.js"));
        let gaps = async || {
            let result = fixture
                .script(format!(
                    "const candidates = [{}];\n{body}",
                    ids["candidate"]
                ))
                .await;
            result[0]["gaps"].clone()
        };
        let undecided = format!(
            "code premise {premise} of flaw {} is undecided: the listener is enabled",
            ids["flaw"]
        );
        assert_eq!(gaps().await, json!([undecided]));
        let later = fixture.script(format!(r#"
            const research = async action => (await tool.research(action)).unwrap();
            const at = (file, line) => ({{code: {{file, start_line: line, end_line: line}}}});
            const component = await research({{action:'upsert',record:{{kind:'component',title:'store',body:'session store',files:['store.c']}}}});
            const input = await research({{action:'upsert',record:{{kind:'state',scope:'input',title:'login',body:'login form',location:at('store.c',1),trust:'untrusted'}}}});
            const path = await research({{action:'upsert',record:{{kind:'path',title:'login',body:'login writes the session'}}}});
            await research({{action:'link',links:[{{path_state:{{path,state:input,access:'read',location:at('store.c',1)}}}},{{path_step:{{path,component,note:'writes'}}}}]}});
            const flaw = await research({{action:'upsert',record:{{kind:'flaw',title:'Stale session',body:'kept after logout',component,location:at('store.c',4)}}}});
            await research({{action:'upsert',record:{{kind:'candidate',id:{candidate},flaws:[{root},flaw]}}}});
            return {{path, flaw}};
        "#, candidate = ids["candidate"], root = ids["flaw"])).await;
        let off = format!(
            "flaw {} lies off every action's steps: a missing step or action?",
            later["flaw"]
        );
        assert_eq!(gaps().await, json!([undecided, off]));
        let actions = json!({"action":"upsert","record":{"kind":"candidate","id":ids["candidate"],"paths":[ids["path"],later["path"]]}});
        fixture.success("research", actions).await;
        assert_eq!(gaps().await, json!([undecided]));
        let link = json!({"action":"link","links":[{"path_premise":{"path":later["path"],"premise":premise}}]});
        fixture.success("research", link).await;
        let repeated = json!({"action":"upsert","record":{"kind":"candidate","id":ids["candidate"],"paths":[later["path"],ids["path"],later["path"]]}});
        fixture.success("research", repeated).await;
        let on_path = format!(
            "code premise {premise} of path {} is undecided: the listener is enabled",
            later["path"]
        );
        assert_eq!(gaps().await, json!([undecided, on_path]), "once per path");
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
        let body = template_body(include_str!("../prompts/scripts/target-map.js"));
        let map = r#"[{title: "net", body: "network input", patterns: ["src/net/**"]}, {title: "core", body: "the rest", patterns: ["src/**"]}]"#;
        let result = fixture.script(format!("const map = {map};\n{body}")).await;
        let mut files = Vec::new();
        for id in result["components"].as_array().unwrap() {
            let node = json!({"action":"node","id":id});
            files.push(fixture.success("research_query", node).await["files"].clone());
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

    /// The paths script groups the one-step entry paths its children record by that step,
    /// whatever its file is called, never with completed routes converging on it, and lists a
    /// path without steps and a recorded input state on no path.
    #[tokio::test]
    async fn the_paths_script_groups_entries_and_lists_unlinked_inputs() {
        let fixture = Fixture::new(capabilities([SecurityCapability::Research.capability()])).await;
        let result = fixture.script(r#"
            const research = async action => (await tool.research(action)).unwrap();
            const at = (file, line) => ({code: {file, start_line: line, end_line: line}});
            const parser = await research({action:'upsert',record:{kind:'component',title:'parser',body:'parses',files:['src/a b:c.c']}});
            const decoder = await research({action:'upsert',record:{kind:'component',title:'decoder',body:'decodes',files:['vendor/x.c']}});
            const paths = [];
            for (const steps of [[[parser, 9]], [[parser, 9]], [[decoder, 4]], [], [[parser, 1], [decoder, 4]], [[parser, 2], [decoder, 4]]]) {
                const path = await research({action:'upsert',record:{kind:'path',title:`entry ${paths.length}`,body:'request'}});
                for (const [component, line] of steps) await research({action:'link',links:[{path_step:{path,component,location:at(component === parser ? 'src/a b:c.c' : 'vendor/x.c', line),note:'calls'}}]});
                paths.push(path);
            }
            const linked = await research({action:'upsert',record:{kind:'state',scope:'input',title:'cookie',body:'session cookie',location:at('src/a b:c.c',1),trust:'untrusted'}});
            await research({action:'link',links:[{path_state:{path:paths[4],state:linked,access:'read',location:at('src/a b:c.c',1)}}]});
            const unlinked = await research({action:'upsert',record:{kind:'state',scope:'input',title:'body',body:'request body',location:at('vendor/x.c',2),trust:'authenticated'}});
            await research({action:'upsert',record:{kind:'state',scope:'instance',title:'parser',body:'parser state'}});
            return {parser, decoder, paths, unlinked};
        "#).await;
        let paths = template_body(include_str!("../prompts/scripts/paths.live.js"));
        let grouped = fixture
            .script(format!("const workers = 1, inputs = [];\n{paths}"))
            .await;
        let at = |file: &str, line: u32| json!({"code": {"file": file, "start_line": line, "end_line": line, "function": null}});
        let paths = &result["paths"];
        assert_eq!(
            grouped["entries"],
            json!([
                {"component": result["parser"], "at": at("src/a b:c.c", 9), "paths": [paths[0], paths[1]]},
                {"component": result["decoder"], "at": at("vendor/x.c", 4), "paths": [paths[2]]},
            ])
        );
        assert_eq!(grouped["unmatched"], json!([paths[3]]));
        assert_eq!(grouped["unlinked"], json!([result["unlinked"]]));
        fixture.close().await;
    }

    /// The sweep's measuring script sizes each file of the components its path's steps enter
    /// from the directory listings, leaving out a listed file no step's component holds.
    #[tokio::test]
    async fn the_sweep_script_measures_its_files() {
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
        let path = fixture.script(r#"
            const research = async action => (await tool.research(action)).unwrap();
            const parser = await research({action:'upsert',record:{kind:'component',title:'parser',body:'parses',files:['src/a.c','src/b.c']}});
            const main = await research({action:'upsert',record:{kind:'component',title:'main',body:'starts',files:['main.c']}});
            const path = await research({action:'upsert',record:{kind:'path',title:'request',body:'request'}});
            await research({action:'link',links:[{path_step:{path,component:main,note:'reads'}},{path_step:{path,component:parser,note:'parses'}}]});
            return path;
        "#).await;
        let body = template_body(include_str!("../prompts/scripts/sweep-measure.js"));
        let result = fixture
            .script(format!("const path = {path};\n{body}"))
            .await;
        assert_eq!(
            result,
            json!({"steps": 2, "files": 3, "bytes": 14, "largest": [
                {"file": "src/a.c", "bytes": 10}, {"file": "src/b.c", "bytes": 3}, {"file": "main.c", "bytes": 1},
            ]})
        );
        fixture.close().await;
    }

    /// The scheduler template a run under `live` is shown, without its inputs line.
    fn scheduler(live: LivePolicy) -> String {
        template_body(match live {
            LivePolicy::No => include_str!("../prompts/scripts/scheduler.no-live.js"),
            LivePolicy::Ask | LivePolicy::Yes => {
                include_str!("../prompts/scripts/scheduler.live.js")
            }
        })
        .to_owned()
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

    /// What a scripted child does once it has begun the work its brief names.
    #[derive(Clone, Copy, Debug)]
    enum Then {
        Fail,
        /// Fails before it begins.
        Unbegun,
        /// Records the `research` actions made of the first subject it began, one a response,
        /// then replies.
        Record(fn(&Value) -> Vec<Value>),
        /// Runs until it is cancelled.
        Hang,
    }

    /// Marks the path it began swept.
    const SWEEP: Then = Then::Record(|path| {
        vec![
            json!({"action": "set_status", "record": {"kind": "path", "id": path, "status": "swept"}, "note": "walked it"}),
        ]
    });

    /// Confirms the flaw it began.
    const CONFIRM: Then = Then::Record(|flaw| {
        vec![
            json!({"action": "set_status", "record": {"kind": "flaw", "id": flaw, "status": "confirmed"}, "note": "no consequence"}),
        ]
    });

    /// Scores the candidate it began, then validates it.
    const VALIDATE: Then = Then::Record(|candidate| {
        vec![
            json!({"action": "assess", "candidate": candidate, "vector": "CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N", "note": "unauthenticated request controls memory corruption"}),
            json!({"action": "verdict", "candidate": candidate, "verdict": "validated", "note": "reviewed the path"}),
        ]
    });

    type Decide = Arc<dyn Fn(&str) -> Then + Send + Sync>;

    /// A provider standing in for every child the scheduler launches: a child's first
    /// response begins the work its brief names, as a child's first call does, and its next
    /// does what the decision makes of its brief.
    struct Children(Decide);

    struct Child {
        then: Decide,
        responses: usize,
    }

    impl skyhook::provider::Provider for Children {
        fn open_context(
            &self,
            _: skyhook::provider::protocol::ContextId,
        ) -> Result<Box<dyn skyhook::provider::ProviderContext>, skyhook::provider::ProviderError>
        {
            Ok(Box::new(Child {
                then: self.0.clone(),
                responses: 0,
            }))
        }
    }

    impl skyhook::provider::ProviderContext for Child {
        fn invoke(
            &mut self,
            request: skyhook::provider::protocol::ModelRequest,
        ) -> skyhook::provider::ResponseStream {
            use futures_util::StreamExt;
            use skyhook::provider::{
                ProviderErrorKind,
                protocol::{
                    AssistantItem, BlockId, Completion, Entry, ItemId, Message, ResponseEvent,
                    TextBlock, ToolCall, UserContent,
                },
            };
            let id = |id: &str| ItemId::try_from(id.to_owned()).unwrap();
            let call = |name: &str, arguments: Value| {
                let call = ToolCall::new(name, name, arguments).unwrap();
                let item = AssistantItem::ToolCall {
                    id: id(name),
                    position: 0.into(),
                    call,
                };
                Ok(Completion::tool_use(vec![item]).unwrap())
            };
            let answer = || {
                let text = TextBlock {
                    id: BlockId::try_from("reply:0".to_owned()).unwrap(),
                    position: 0.into(),
                    text: "done".to_owned(),
                };
                let item = AssistantItem::Text {
                    id: id("reply"),
                    position: 0.into(),
                    blocks: vec![text],
                };
                Ok(Completion::answer(vec![item]).unwrap())
            };
            let brief = (request.context.iter()).find_map(|entry| match entry {
                Entry::Message(Message::User(content)) => {
                    content.iter().find_map(|content| match content {
                        UserContent::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                }
                _ => None,
            });
            // Only the root runs without a brief: told its background question ended, it is done.
            let Some(brief) = brief else {
                return futures_util::stream::iter([answer().map(ResponseEvent::End)]).boxed();
            };
            let work = begin(&brief);
            let failed = || Err(ProviderErrorKind::Authentication.error("the child failed"));
            self.responses += 1;
            let completion = match (self.responses, (self.then)(&brief)) {
                (1, Then::Unbegun) => failed(),
                (1, _) => call("research", work),
                (2, Then::Fail) => failed(),
                (2, Then::Hang) => return futures_util::stream::pending().boxed(),
                (made, Then::Record(actions)) => {
                    match actions(&work["subjects"][0]).get(made - 2) {
                        Some(action) => call("research", action.clone()),
                        None => answer(),
                    }
                }
                _ => answer(),
            };
            futures_util::stream::iter([completion.map(ResponseEvent::End)]).boxed()
        }
    }

    /// The `research` `begin` call a brief's first work asks for, from the phase and the node
    /// ids the scheduler names in it: "Settle premises 4, 5.".
    fn begin(brief: &str) -> Value {
        let work = brief.split(['.']).next().unwrap();
        let (head, ids) = work.split_at(work.find(|c: char| c.is_ascii_digit()).unwrap());
        let phase = match head.trim().trim_end_matches('s') {
            "Sweep path" => "sweep",
            "Check state" => "check",
            "Settle premise" => "settle",
            "Chain flaw" => "chain",
            "Review candidate" => "review",
            "Test candidate" => "test",
            "Report root flaw" => "report",
            other => panic!("no phase is briefed as {other:?}"),
        };
        let subjects: Vec<u64> = ids.split(", ").map(|id| id.parse().unwrap()).collect();
        json!({"action": "begin", "phase": phase, "subjects": subjects})
    }

    /// Stubs recording each launch's brief before the real child runs, and a `receive` no
    /// steering reaches.
    const RECORDING: &str = "const started = [], real = globalThis.tool;
        const tool = {
            jobs: real.jobs, research: real.research, research_query: real.research_query,
            agent: args => (started.push(args.prompt), real.agent(args)),
        };
        const receive = () => new Promise(() => {});";

    /// A path per name in `names`, each through a component of its own and owing a sweep.
    async fn sweeps(fixture: &Fixture, names: std::ops::Range<usize>) -> Vec<PathId> {
        use crate::store::tests::{component, path};
        let store = Store::of(fixture.session.store().extension().unwrap());
        let agent = fixture.session.root_agent().clone();
        tokio::task::spawn_blocking(move || {
            names
                .map(|i| {
                    let name = format!("part{i}");
                    let component = component(&store, &agent, &name);
                    path(&store, &agent, &name, &[component])
                })
                .collect()
        })
        .await
        .unwrap()
    }

    /// The scheduler's launch briefs, each cut to its first sentence: its work and subjects.
    const HEADS: &str = "started.map(p => p.split('. ')[0])";

    /// The scheduler launches what the store starts, briefing each child with its work's phase
    /// and node ids. A pool wider than the largest page reads its work a page at a time, and a
    /// launch still waiting to begin holds its node: it is not launched again. With one worker
    /// and a long queue, the rows it reads grow with the work, not with its square. Every child
    /// fails, so each node is tried twice, a child that never began counting as a try, then
    /// exhausted with its last reply, which releases the chain a sweep held back.
    #[tokio::test]
    async fn the_scheduler_launches_a_page_of_briefed_work_at_a_time() {
        let unbegun = Arc::new(std::sync::OnceLock::<String>::new());
        let briefed = unbegun.clone();
        let fixture = Fixture::with_children(LivePolicy::No, move |brief| {
            match briefed.get().is_some_and(|path| brief.starts_with(path)) {
                true => Then::Unbegun,
                false => Then::Fail,
            }
        })
        .await;
        let wide = sweeps(&fixture, 0..2).await;
        // The first launch begins once the other sweep is exhausted.
        let stubs = "let launched = 0, waiting = null, begin;
            const held = [], real = globalThis.tool;
            const others = new Promise(resolve => begin = resolve);
            const tool = {
                jobs: real.jobs, research: real.research,
                research_query: async action => {
                    if (action.action === 'startable' && waiting !== null) held.push(action.running.includes(waiting));
                    return real.research_query(action);
                },
                agent: async args => {
                    if (++launched === 3) begin();
                    if (launched > 1) return real.agent(args);
                    waiting = Number(args.prompt.match(/\\d+/)[0]);
                    await others;
                    waiting = null;
                    return real.agent(args);
                },
            };
            const receive = () => new Promise(() => {});
            const workers = 1001, artifacts = 'artifacts', source = '.';";
        let returned = "{launched, held, exhausted: outcome.synthesis.exhausted.length}";
        let result = run_template(&fixture, stubs, &scheduler(LivePolicy::No), returned).await;
        assert_eq!(result["launched"], json!(4), "{result}");
        assert_eq!(result["exhausted"], json!(2), "{result}");
        let held = result["held"].as_array().unwrap();
        assert!(
            !held.is_empty() && held.iter().all(|running| running == true),
            "{result}"
        );

        let paths = sweeps(&fixture, 2..5).await;
        let store = Store::of(fixture.session.store().extension().unwrap());
        let agent = fixture.session.root_agent().clone();
        let first = paths[0];
        let flaw = tokio::task::spawn_blocking(move || {
            let NodeView::Path(view) = store.node(first.into()).unwrap() else {
                panic!("a path")
            };
            let component = view.steps[0].component.id.get().try_into().unwrap();
            crate::store::tests::flaw(&store, &agent, component)
        })
        .await
        .unwrap();
        unbegun.set(format!("Sweep path {first}.")).unwrap();
        let stubs = format!(
            "let rows = 0;
            {RECORDING}
            tool.research_query = async action => {{ const result = await real.research_query(action); if (action.action === 'startable') rows += result.unwrap().items.length; return result; }};
            const workers = 1, artifacts = 'artifacts', source = '.';"
        );
        let returned = format!(
            "{{rows, started: {HEADS}, exhausted: outcome.synthesis.exhausted.map(e => [e.phase, e.node]), reply: outcome.synthesis.exhausted.find(e => e.node === {first}).reply}}"
        );
        let mut result =
            run_template(&fixture, &stubs, &scheduler(LivePolicy::No), &returned).await;
        let rows = result["rows"].take().as_u64().unwrap();
        let reply = result["reply"].take();
        let launches = paths
            .iter()
            .map(|path| format!("Sweep path {path}"))
            .chain([format!("Chain flaw {flaw}")])
            .flat_map(|head| [head.clone(), head]);
        assert_eq!(
            result["started"],
            json!(launches.collect::<Vec<_>>()),
            "{result}"
        );
        assert!(rows <= 8, "{rows} rows for 8 launches");
        let exhausted = (wide.iter().chain(&paths))
            .map(|path| json!(["sweep", path]))
            .chain([json!(["chain", flaw])]);
        assert_eq!(
            result["exhausted"],
            json!(exhausted.collect::<Vec<_>>()),
            "{result}"
        );
        let reply = reply.as_str().unwrap();
        assert!(reply.contains("the child failed"), "{reply}");
        fixture.close().await;
    }

    /// Sweeps, chains confirming their flaws, and reviews scoring and validating their
    /// candidates recorded; any other work fails.
    fn scripted(brief: &str) -> Then {
        match brief.split(' ').next() {
            Some("Sweep") => SWEEP,
            Some("Chain") => CONFIRM,
            Some("Review") => VALIDATE,
            _ => Then::Fail,
        }
    }

    /// A task's read returns its finished child's reply after its job ended, by the id the
    /// synthesis names for the subject its tasks left exhausted.
    #[tokio::test]
    async fn a_finished_tasks_reply_is_read_by_the_id_its_exhaustion_names() {
        let fixture = Fixture::with_children(LivePolicy::No, |_| {
            Then::Record(|path| {
                vec![json!({"action": "evidence", "node": path, "items": [{"stance": "context", "note": "looked"}]})]
            })
        })
        .await;
        let path = sweeps(&fixture, 0..1).await[0];
        let result = fixture.script(format!(r#"
            for (let i = 0; i < 2; i++) await tool.agent({{mode: "sweep", depth: 1, name: `sweep-${{i}}`, prompt: "Sweep path {path}."}});
            const [exhausted] = (await tool.research_query({{action: "synthesis"}})).unwrap().exhausted;
            const task = (await tool.research_query({{action: "task", id: exhausted.tasks.at(-1)}})).unwrap();
            return {{tasks: exhausted.tasks.length, subjects: task.subjects, state: task.state, reply: task.reply}};
        "#)).await;
        assert_eq!(
            result,
            json!({"tasks": 2, "subjects": [path], "state": "completed", "reply": "done"})
        );
        fixture.close().await;
    }

    /// The scheduler returns with the tests it holds and nothing else to start; run again with
    /// the user's approval, it reaches the approved test, a page per launch. Two children
    /// leaving that test owed exhaust it, which releases its root's report.
    #[tokio::test]
    async fn the_scheduler_returns_held_tests_and_reaches_the_approved_one_when_rerun() {
        let fixture = Fixture::with_children(LivePolicy::Ask, scripted).await;
        let ids = fixture.discover().await;
        fixture.validate(&ids["candidate"]).await;
        let stubs = "let reads = 0;
            const started = [], real = globalThis.tool;
            const tool = {
                jobs: real.jobs, research: real.research,
                research_query: action => (action.action === 'startable' && reads++, real.research_query(action)),
                agent: args => (started.push(args.prompt), real.agent(args)),
            };
            const receive = () => new Promise(() => {});
            const workers = 1, artifacts = 'artifacts', source = '.';";
        let returned = format!("{{reads, started: {HEADS}, held: outcome.held}}");
        let run = async |inputs: Vec<(&'static str, String)>| {
            let body = inputs
                .iter()
                .fold(scheduler(LivePolicy::Ask), |body, (input, value)| {
                    assert!(body.contains(input), "{input}");
                    body.replace(input, value)
                });
            run_template(&fixture, stubs, &body, &returned).await
        };
        assert_eq!(
            run(Vec::new()).await,
            json!({"reads": 2, "started": [format!("Sweep path {}", ids["path"])], "held": true})
        );
        let test = format!("Test candidate {}", ids["candidate"]);
        let report = format!("Report root flaw {}", ids["flaw"]);
        let rerun = run(vec![(
            "[/* under ask: candidates the user approved */]",
            format!("[{}]", ids["candidate"]),
        )]);
        assert_eq!(
            rerun.await,
            json!({"reads": 5, "started": [&test, &test, &report, &report], "held": false})
        );
        fixture.close().await;
    }

    /// While the root's question to the user is open, the scheduler starts other work but no
    /// review; once the question closes, the same run starts it.
    #[tokio::test]
    async fn the_scheduler_holds_reviews_while_the_root_asks() {
        use crate::store::tests::{scenario, sweep};
        let fixture = Fixture::with_children(LivePolicy::No, scripted).await;
        let other = sweeps(&fixture, 0..1).await[0];
        let store = Store::of(fixture.session.store().extension().unwrap());
        let agent = fixture.session.root_agent().clone();
        let scenario = tokio::task::spawn_blocking(move || {
            let scenario = scenario(&store, &agent, "service");
            sweep(&store, scenario.path);
            scenario
        })
        .await
        .unwrap();
        // The question closes in the scheduler's wait for it once the sweep is done, when only
        // that wait keeps the run going.
        let stubs = "let open = true, swept;
            const done = new Promise(resolve => swept = resolve), started = [], real = globalThis.tool;
            const question = (await real.ask({header: 'profile', prompt: 'Which deployment?', bg: true})).id;
            const sleep = async () => { await done; if (open) { open = false; await real.control({job: question, cancel: true}); } };
            const tool = {
                jobs: real.jobs, research: real.research, research_query: real.research_query,
                agent: async args => {
                    started.push([args.prompt.split('. ')[0], open]);
                    const result = await real.agent(args);
                    swept();
                    return result;
                },
            };
            const receive = () => new Promise(() => {});
            const workers = 2, artifacts = 'artifacts', source = '.';";
        let result = run_template(&fixture, stubs, &scheduler(LivePolicy::No), "started").await;
        let (candidate, flaw) = (scenario.candidate, scenario.flaw);
        assert_eq!(
            result,
            json!([
                [format!("Sweep path {other}"), true],
                [format!("Review candidate {candidate}"), false],
                [format!("Report root flaw {flaw}"), false],
                [format!("Report root flaw {flaw}"), false],
            ])
        );
        fixture.close().await;
    }

    /// The scheduler keeps nothing the store does not: killed while one child runs and a
    /// relaunch waits to begin, a new scheduler gives the node whose child failed once
    /// exactly one more try, and the killed child's node its second, then each is exhausted.
    #[tokio::test]
    async fn a_rerun_scheduler_carries_over_what_the_killed_one_tried() {
        use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
        // The parser's children run until the first scheduler is killed; every other fails.
        let killing = Arc::new(AtomicBool::new(true));
        let hangs = killing.clone();
        let parsing = Arc::new(std::sync::OnceLock::<String>::new());
        let briefed = parsing.clone();
        let fixture = Fixture::with_children(LivePolicy::No, move |brief| {
            let parser = briefed
                .get()
                .is_some_and(|parser| brief.starts_with(parser));
            match parser && hangs.load(SeqCst) {
                true => Then::Hang,
                false => Then::Fail,
            }
        })
        .await;
        let paths = sweeps(&fixture, 0..2).await;
        let (decoder, parser) = (paths[0], paths[1]);
        parsing.set(format!("Sweep path {parser}.")).unwrap();
        // The decoder's second launch never reaches its child.
        let killed = format!(
            "{RECORDING}
            tool.agent = args => (started.push(args.prompt), started.filter(p => p === args.prompt).length > 1 && args.prompt.startsWith('Sweep path {decoder}.') ? new Promise(() => {{}}) : real.agent(args));
            const workers = 2, artifacts = 'artifacts', source = '.';"
        );
        let store = Store::of(fixture.session.store().extension().unwrap());
        let mut updates = fixture.session.store().observe().updates;
        let tried = async {
            loop {
                let store = store.clone();
                let tasks = tokio::task::spawn_blocking(move || {
                    store
                        .task_page(crate::store::Page::default())
                        .unwrap()
                        .items
                })
                .await
                .unwrap();
                let state = |path: PathId| {
                    (tasks.iter())
                        .filter(|task| task.subjects == [NodeId::from(path)])
                        .map(|task| task.state)
                        .collect::<Vec<_>>()
                };
                use skyhook::job::JobState::{Failed, Running};
                if state(decoder) == [Failed] && state(parser) == [Running] {
                    break;
                }
                // A begun task's call and a job's move each journal a record; a lag skipped some.
                if let Err(tokio::sync::broadcast::error::RecvError::Closed) = updates.recv().await
                {
                    panic!("the session closed");
                }
            }
            let script = (fixture.session.jobs().active(fixture.session.root_agent()))
                .into_iter()
                .find(|job| job.tool == "script")
                .unwrap();
            fixture.session.cancel_job(script.id).await.unwrap();
            killing.store(false, SeqCst);
        };
        // Killed, the script fails; what it left is the store's.
        let run = fixture
            .session
            .run_script(format!("{killed}\n{}", scheduler(LivePolicy::No)));
        let _ = bounded(async { tokio::join!(run, tried) }).await;

        let stubs =
            format!("{RECORDING}\nconst workers = 2, artifacts = 'artifacts', source = '.';");
        let returned = format!(
            "{{started: {HEADS}, exhausted: outcome.synthesis.exhausted.map(e => [e.phase, e.node])}}"
        );
        let result = run_template(&fixture, &stubs, &scheduler(LivePolicy::No), &returned).await;
        assert_eq!(
            result,
            json!({
                "started": [
                    format!("Sweep path {decoder}"),
                    format!("Sweep path {parser}"),
                ],
                "exhausted": [["sweep", decoder], ["sweep", parser]],
            })
        );
        fixture.close().await;
    }
}
