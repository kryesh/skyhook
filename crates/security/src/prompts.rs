//! The persona and research modes' instructions: templates the library renders for each
//! agent, from its context and the run's live policy.

use std::sync::LazyLock;

use skyhook::prompts::{Prompts, Source, Templates};

use crate::store::LivePolicy;

const SOURCES: [(&str, Source); 29] = skyhook::templates!["security":
    Template "persona.md",
    Template "orchestrate.md",
    Template "target.md",
    Template "paths.md",
    Template "entries.md",
    Template "sweep.md",
    Template "sweep-part.md",
    Template "check.md",
    Template "probe.md",
    Template "question.md",
    Template "settle.md",
    Template "chain.md",
    Template "validate.md",
    Template "trace.md",
    Template "environment.md",
    Template "poc.md",
    Template "report.md",
    Template "recording.md",
    Template "sweep-rules.md",
    Template "cvss.md",
    Template "scripts/scheduler.js",
    Template "scripts/paths.js",
    Verbatim "scripts/target-inventory.js",
    Verbatim "scripts/target-map.js",
    Verbatim "scripts/sweep-measure.js",
    Verbatim "scripts/sweep-stretches.js",
    Verbatim "scripts/validate-gaps.js",
    Verbatim "scripts/poc-artifacts.js",
    Verbatim "scripts/report-scenarios.js",
];

/// What every template reads beside the agent's context: the run's settings, fixed for
/// the session.
#[derive(serde::Serialize)]
struct Context {
    /// Whether the run's policy allows live work.
    live: bool,
}

/// The prompts of a session whose run is under `live`.
pub(crate) fn prompts(live: LivePolicy) -> Prompts {
    static TEMPLATES: LazyLock<Templates<Context>> = LazyLock::new(|| {
        Templates::with(SOURCES, &Context { live: false }).expect("embedded templates parse")
    });
    let context = Context {
        live: live != LivePolicy::No,
    };
    let persona = "security/persona.md"
        .parse()
        .expect("template name is nonblank");
    let prompts = Prompts::host(&TEMPLATES, &context, persona);
    prompts.expect("the run's settings are a section")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Value, json};
    use skyhook::{
        config::mode::{Instructions, Mode},
        tool::policy::{Capability, CapabilitySet},
    };

    use super::*;
    use crate::{
        capabilities::SecurityCapability,
        store::{NodeId, NodeView, PathId, Store},
        tools::tests::{Fixture, bounded, capabilities},
    };

    /// Script `name` as a run under `live` is shown it, without its first line, which declares
    /// its inputs.
    fn script(name: &str, live: LivePolicy) -> String {
        let root = crate::modes::root();
        let mode = Mode {
            instructions: Some(Instructions::Template(
                format!("security/scripts/{name}").parse().unwrap(),
            )),
            ..crate::modes::modes()[&root].clone()
        };
        let agent = skyhook::prompts::Agent {
            depth: 0,
            available_depth: 1,
            capabilities: mode.capabilities.set(),
            mode: Some(&root),
            workspace: &"/workspace".into(),
            target: &skyhook_tools::target::TargetRef::Root,
            definition: None,
            facts: None,
        };
        let edition = prompts(live).edition(&mode, &agent).unwrap();
        let text = edition.instructions.unwrap();
        text.as_str().split_once('\n').unwrap().1.to_owned()
    }

    /// Records one input state, component and path reading it, a flaw on the component and a
    /// candidate of it along the path, assessed critical; binds their ids.
    const DISCOVER: &str = r#"
        const research = async action => (await tool.research(action)).unwrap();
        const at = (file, line) => ({code: {file, start_line: line, end_line: line}});
        const component = await research({action:'upsert',record:{kind:'component',title:'parser',body:'request parser',files:['input.c']}});
        const input = await research({action:'upsert',record:{kind:'state',scope:'input',title:'request',body:'raw request bytes',location:at('input.c',1),trust:'untrusted'}});
        const path = await research({action:'upsert',record:{kind:'path',title:'request parsing',body:'raw request parsing',steps:[{component,note:'parse is called on every request'}]}});
        await research({action:'link',links:[{path_state:{path,state:input,access:'read',location:at('input.c',1)}}]});
        const flaw = await research({action:'upsert',record:{kind:'flaw',title:'Unchecked copy',body:'the request length is copied into a fixed buffer',component,location:at('input.c',8),cwe:120}});
        const candidate = await research({action:'upsert',record:{kind:'candidate',title:'Unchecked copy',body:'request length overflows a fixed buffer',paths:[path],flaws:[flaw]}});
        await research({action:'assess',candidate,vector:'CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N',note:'unauthenticated request controls memory corruption'});
    "#;

    impl Fixture {
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
                    .modes(crate::modes::modes())
                    .prompts(crate::prompts::prompts(live))
            };
            Self::built(capabilities, harness, live).await
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
    }

    fn all_capabilities() -> CapabilitySet {
        capabilities(
            [Capability::Write]
                .into_iter()
                .chain(SecurityCapability::ALL.map(SecurityCapability::capability)),
        )
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
        let body = script("report-scenarios.js", LivePolicy::Yes);
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
        let body = script("validate-gaps.js", LivePolicy::Yes);
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
            const path = await research({{action:'upsert',record:{{kind:'path',title:'login',body:'login writes the session',steps:[{{component,note:'writes'}}]}}}});
            await research({{action:'link',links:[{{path_state:{{path,state:input,access:'read',location:at('store.c',1)}}}}]}});
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
            let path = fixture.root.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "int x;\n").unwrap();
        }
        let body = script("target-map.js", LivePolicy::Yes);
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
                const through = steps.map(([component, line]) => ({component,location:at(component === parser ? 'src/a b:c.c' : 'vendor/x.c', line),note:'calls'}));
                const path = await research({action:'upsert',record:{kind:'path',title:`entry ${paths.length}`,body:'request',steps:through}});
                paths.push(path);
            }
            const linked = await research({action:'upsert',record:{kind:'state',scope:'input',title:'cookie',body:'session cookie',location:at('src/a b:c.c',1),trust:'untrusted'}});
            await research({action:'link',links:[{path_state:{path:paths[4],state:linked,access:'read',location:at('src/a b:c.c',1)}}]});
            const unlinked = await research({action:'upsert',record:{kind:'state',scope:'input',title:'body',body:'request body',location:at('vendor/x.c',2),trust:'authenticated'}});
            await research({action:'upsert',record:{kind:'state',scope:'instance',title:'parser',body:'parser state'}});
            return {parser, decoder, paths, unlinked};
        "#).await;
        let paths = script("paths.js", LivePolicy::Yes);
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
            let path = fixture.root.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x".repeat(bytes)).unwrap();
        }
        let path = fixture.script(r#"
            const research = async action => (await tool.research(action)).unwrap();
            const parser = await research({action:'upsert',record:{kind:'component',title:'parser',body:'parses',files:['src/a.c','src/b.c']}});
            const main = await research({action:'upsert',record:{kind:'component',title:'main',body:'starts',files:['main.c']}});
            const path = await research({action:'upsert',record:{kind:'path',title:'request',body:'request',steps:[{component:main,note:'reads'},{component:parser,note:'parses'}]}});
            return path;
        "#).await;
        let body = script("sweep-measure.js", LivePolicy::Yes);
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
        let result = run_template(
            &fixture,
            stubs,
            &script("scheduler.js", LivePolicy::No),
            returned,
        )
        .await;
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
            let component = i64::from(view.steps[0].component.id).try_into().unwrap();
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
        let mut result = run_template(
            &fixture,
            &stubs,
            &script("scheduler.js", LivePolicy::No),
            &returned,
        )
        .await;
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
            let body = inputs.iter().fold(
                script("scheduler.js", LivePolicy::Ask),
                |body, (input, value)| {
                    assert!(body.contains(input), "{input}");
                    body.replace(input, value)
                },
            );
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
        let result = run_template(
            &fixture,
            stubs,
            &script("scheduler.js", LivePolicy::No),
            "started",
        )
        .await;
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
        let run = fixture.session.run_script(format!(
            "{killed}\n{}",
            script("scheduler.js", LivePolicy::No)
        ));
        let _ = bounded(async { tokio::join!(run, tried) }).await;

        let stubs =
            format!("{RECORDING}\nconst workers = 2, artifacts = 'artifacts', source = '.';");
        let returned = format!(
            "{{started: {HEADS}, exhausted: outcome.synthesis.exhausted.map(e => [e.phase, e.node])}}"
        );
        let result = run_template(
            &fixture,
            &stubs,
            &script("scheduler.js", LivePolicy::No),
            &returned,
        )
        .await;
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
