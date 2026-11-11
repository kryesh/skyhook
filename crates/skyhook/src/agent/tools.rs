//! Agent-specific tools layered on top of the general coding tool set.
use std::sync::{Arc, OnceLock, Weak};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tool::catalog::{RegistryError, ToolOptions};
use crate::tool::policy::Capability;
use crate::{
    agent::Question,
    provider::model::{ModelEntry, ModelRef},
    session::{ReplaceError, TodoItem, TodoStatus, TodoUpdate},
    tool::{ToolError, ToolRegistryBuilder, catalog},
};
use skyhook_tools::{diagnostic::FailureSite, error::AdmissionError};
use skyhook_worker::diagnostic::{Effects, Operation, Subject};

use super::{SessionRuntime, child::register_child_agent};

/// Connect only root-eligible MCP servers; adapters enforce per-agent gates later.
pub(super) async fn connect_mcp(
    builder: &mut ToolRegistryBuilder,
    harness: &super::HarnessInner,
    capabilities: &crate::tool::policy::CapabilitySet,
    store: &crate::session::SessionStore,
) -> (Arc<crate::mcp::manager::McpManager>, Vec<String>) {
    let capabilities = capabilities.for_agent(harness.max_child_depth);
    let manager = Arc::new(
        crate::mcp::manager::McpManager::connect(
            &harness.mcp,
            &capabilities,
            &harness.workspace,
            tokio_util::sync::CancellationToken::new(),
        )
        .await,
    );
    let mut warnings = manager.warnings();
    warnings.extend(crate::mcp::adapter::register(
        builder,
        manager.clone(),
        store.clone(),
    ));
    (manager, warnings)
}

pub(super) fn register(
    builder: &mut ToolRegistryBuilder,
    runtime_slot: Arc<OnceLock<Weak<SessionRuntime>>>,
    models: &indexmap::IndexMap<ModelRef, ModelEntry>,
    modes: &super::builder::Modes,
) -> Result<(), RegistryError> {
    let (wait, ask, todo) = (
        runtime_slot.clone(),
        runtime_slot.clone(),
        runtime_slot.clone(),
    );
    builder.register_checked(
        "wait",
        "Wait for any notification or input relevant to this agent. Cancellation fails the wait with reason `cancelled`. A timeout ends this wait, not background work; wait again if still dependent on it",
        ToolOptions::default().job_role(crate::job::JobRole::Wait).result::<super::wait::WaitOutput>(),
        Ok,
        move |args: super::wait::WaitArgs| {
            let runtime = wait.get().and_then(Weak::upgrade);
            catalog::Invocation::new(move |context| async move {
                crate::tool::catalog::serialized(runtime.ok_or_else(runtime_unavailable)?.wait_for_event(&context, args).await?)
            })
        },
    )?;
    builder.register_checked(
        "ask",
        "Ask one structured question. Issue independent questions concurrently; the runtime merges calls that become ready together, each under its own header. Offer the real choices as options; never add a catch-all option that expects the user to type. Use bg:true to continue independent work while awaiting an answer",
        ToolOptions::default().job_role(crate::job::JobRole::Question).background().input().requires_for_root(Capability::Interactive).result::<Value>(),
        |question: Question| match question.refusal() {
            None => Ok(question),
            Some(refusal) => Err(AdmissionError::invalid_arguments(refusal)
                .operation(Operation::Validate, Subject::argument(["options"])).effects(Effects::NotStarted)),
        },
        move |input: Question| {
            let runtime = ask.get().and_then(Weak::upgrade);
            catalog::Invocation::new(move |context| async move {
                let runtime = runtime.ok_or_else(runtime_unavailable)?;
                crate::tool::catalog::serialized(runtime.questions.coordinate_question(context, input).await?)
            })
        },
    )?;
    builder.register_checked(
        "todo",
        "Read or edit your list. Give at most one of items and status; edits return no result",
        ToolOptions::default().result::<TodoList>(),
        |TodoArgs { items, status }| match (items, status) {
            (Some(items), None) => Ok(TodoRequest::Replace(items)),
            (None, Some(status)) => Ok(TodoRequest::Status(status)),
            (None, None) => Ok(TodoRequest::Inspect),
            (Some(_), Some(_)) => Err(AdmissionError::invalid_arguments(
                "give at most one of items and status",
            )
            .operation(Operation::Validate, Subject::argument(["items"]))
            .effects(Effects::NotStarted)),
        },
        move |request| {
            let runtime = todo.get().and_then(Weak::upgrade);
            let update = match request {
                TodoRequest::Replace(items) => TodoUpdate::Items(items),
                TodoRequest::Status(statuses) => TodoUpdate::Statuses(statuses),
                TodoRequest::Inspect => {
                    return catalog::Invocation::new(move |context| async move {
                        let runtime = runtime.ok_or_else(runtime_unavailable)?;
                        crate::tool::catalog::serialized(TodoList {
                            items: runtime.store.inspect_todos(context.agent()),
                        })
                    });
                }
            };
            catalog::Invocation::unit(move |context| async move {
                let runtime = runtime.ok_or_else(runtime_unavailable)?;
                let issued_by = runtime.jobs.call_origin(context.job()).await;
                let subject = Subject::Label(format!("todos for agent {}", context.agent()));
                runtime
                    .store
                    .replace_todos(context.agent(), update, issued_by)
                    .await
                    .map_err(|error| match error {
                        ReplaceError::Superseded => ToolError::failed(error)
                            .operation(Operation::Save, subject)
                            .effects(Effects::Unchanged),
                        ReplaceError::Statuses { .. } => ToolError::invalid_arguments(error)
                            .operation(Operation::Validate, Subject::argument(["status"]))
                            .effects(Effects::Unchanged),
                        ReplaceError::Session(error) => harness_error(error.into())
                            .operation(Operation::Save, subject)
                            .effects(Effects::Unknown),
                    })
            })
        },
    )?;
    register_child_agent(builder, runtime_slot, models, modes)
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TodoArgs {
    /// Replace your entire list; an empty array clears it
    items: Option<Vec<TodoItem>>,
    /// Set every item's status, one per item in list order, keeping their text
    status: Option<Vec<TodoStatus>>,
}

enum TodoRequest {
    Replace(Vec<TodoItem>),
    Status(Vec<TodoStatus>),
    Inspect,
}

#[derive(Serialize, JsonSchema)]
struct TodoList {
    items: Vec<TodoItem>,
}

pub(super) fn harness_error(error: crate::agent::HarnessError) -> ToolError {
    use crate::agent::HarnessError;
    match error {
        HarnessError::Interrupted => ToolError::interrupted(),
        HarnessError::Io(error) => ToolError::io(error),
        HarnessError::Session(error) => error.into(),
        HarnessError::Job(error) => error.into(),
        HarnessError::Execution(error) => error,
        error => ToolError::failed(error),
    }
}

pub(super) fn runtime_unavailable() -> ToolError {
    ToolError::failed("session runtime is unavailable")
        .operation(
            Operation::Lookup,
            Subject::Label("session runtime".to_owned()),
        )
        // Also returned by resumption, which dispatch does not bind.
        .at(FailureSite::Host)
        .effects(Effects::NotStarted)
}

#[cfg(test)]
mod tests {
    use crate::agent::tests::*;
    use crate::mcp::McpServerConfig;
    use crate::tool::catalog::InvocationKind;
    use crate::tool::policy::CapabilitySet;
    use skyhook_tools::target::ExecutionLocation;
    use std::{assert_matches, collections::BTreeMap, path::Path};

    const FIXTURE: &str = r#"
import json, os, sys
with open(os.environ['MCP_RUNTIME_MARKER'], 'a') as marker:
    marker.write('launched\n')
for line in sys.stdin:
    req = json.loads(line)
    if 'id' not in req:
        continue
    method = req['method']
    if method == 'initialize':
        result = {'protocolVersion':req['params']['protocolVersion'],
                  'capabilities':{'tools':{}},
                  'serverInfo':{'name':'runtime-fixture','version':'1'}}
    elif method == 'tools/list':
        result = {'tools':[{'name':'echo', 'description':'runtime echo',
                  'inputSchema':{'type':'object', 'properties':{'text':{'type':'string'}},
                                 'required':['text'], 'additionalProperties':False}}]}
    elif method == 'tools/call':
        result = {'content':[], 'structuredContent':req['params']['arguments'], 'isError':False}
    else:
        result = {}
    print(json.dumps({'jsonrpc':'2.0','id':req['id'],'result':result}), flush=True)
"#;

    fn stdio_config(root: &Path, capabilities: Vec<Capability>) -> McpServerConfig {
        crate::tests::decode(json!({
            "transport":"stdio",
            "start_command":["python3", "-u", "-c", FIXTURE],
            "env":{"MCP_RUNTIME_MARKER":root.join("launched")},
            "capabilities":capabilities
        }))
        .unwrap()
    }

    fn servers(config: McpServerConfig) -> BTreeMap<String, McpServerConfig> {
        BTreeMap::from([("fixture".into(), config)])
    }

    fn mcp_names(session: &SessionHandle) -> Vec<String> {
        let all = crate::tool::policy::Capability::ALL.into_iter().collect();
        let tools = session.runtime.executor.registry();
        let tools =
            crate::tool::catalog::definitions(&tools.surface_for_agent(&all, &session.root));
        let names = tools.iter().map(|tool| tool.name.clone());
        names.filter(|name| name.starts_with("mcp_")).collect()
    }

    fn mcp_name(session: &SessionHandle) -> String {
        let names = mcp_names(session);
        assert_eq!(names.len(), 1, "{:?}", session.warnings());
        names[0].clone()
    }

    /// `typeof tool.<name>` inside a script run by `executor`.
    async fn script_type(executor: &ToolExecutor, session: &SessionHandle, name: &str) -> String {
        let source = json!({"source":format!("return typeof tool.{name};")});
        let script = executor.execute(session.root.clone(), "script", source, None);
        let output = script.await.unwrap().output.value;
        output["value"].as_str().unwrap().to_owned()
    }

    fn launched(root: &Path) -> String {
        std::fs::read_to_string(root.join("launched")).unwrap()
    }

    #[tokio::test]
    async fn mcp_gates_prevent_stdio_launch_and_http_contact() {
        // The root's effective capabilities apply before transport startup, even with
        // an approve-all policy: a depth of 0 withholds `agents` as its absence does.
        let mut without_agents = Capability::ALL.into_iter().collect::<CapabilitySet>();
        without_agents.remove(&Capability::Agents);
        let cases = [
            (CapabilitySet::default(), vec![Capability::Agents], 0),
            (without_agents, Capability::ALL.to_vec(), 1),
        ];
        for (capabilities, required, depth) in cases {
            let root = tempfile::tempdir().unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let http = crate::tests::decode(json!({
                "transport":"streamable_http",
                "url":format!("http://{}/mcp", listener.local_addr().unwrap()),
                "capabilities":required
            }))
            .unwrap();
            let mcp = BTreeMap::from([
                ("stdio".into(), stdio_config(root.path(), required)),
                ("http".into(), http),
            ]);
            let harness = builder(root.path(), Requests::default())
                .capabilities(capabilities)
                .max_child_depth(depth)
                .policy(Arc::new(crate::tool::policy::AllowAll))
                .mcp(mcp)
                .build()
                .await
                .unwrap();
            let session = ephemeral_session(&harness).await;
            assert!(session.warnings().is_empty());
            assert!(mcp_names(&session).is_empty());
            shutdown_session(session).await;
            assert!(!root.path().join("launched").exists());
            // Startup has finished, so any contact is already in the backlog.
            let accepted = listener.accept().unwrap_err();
            assert_eq!(accepted.kind(), std::io::ErrorKind::WouldBlock);
        }
    }

    #[tokio::test]
    async fn empty_capability_requirements_expose_direct_and_script_tools() {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        // Empty per-server requirements still need the global gate, but neither
        // Read nor Exec is implicitly required to use an MCP connection.
        let capabilities = [Capability::Mcp].into_iter().collect::<CapabilitySet>();
        let harness = builder(root.path(), requests.clone())
            .capabilities(capabilities.clone())
            .mcp(servers(stdio_config(root.path(), vec![])))
            .build()
            .await
            .unwrap();
        let session = ephemeral_session(&harness).await;
        assert!(session.warnings().is_empty());
        let name = mcp_name(&session);
        let executor = session.runtime.executor.clone();
        let executor = executor.with_capabilities(capabilities);
        assert_eq!(session.prompt("list your tools").await.unwrap(), "done");
        let tools = requests.lock().unwrap()[0].tools.clone();
        assert!(tools.iter().any(|tool| tool.name == name));
        let direct = executor.execute(session.root.clone(), &name, json!({"text":"direct"}), None);
        let direct = direct.await.unwrap().output.value;
        assert_eq!(direct["structuredContent"], json!({"text":"direct"}));
        let script = format!("return await tool.{name}({{text:'script'}});");
        let script = session.run_script(script).await.unwrap().value;
        let found = &script["value"]["result"]["structuredContent"];
        assert_eq!(*found, json!({"text":"script"}));
        // Also test a discovered adapter with empty server requirements. Startup
        // omission alone would not catch a missing adapter-level global gate.
        let disabled = executor.with_capabilities(CapabilitySet::empty());
        assert!(
            disabled
                .surface_for_agent(&session.root)
                .get(&name)
                .is_none()
        );
        let denied = json!({"text":"denied"});
        let direct = disabled.execute(session.root.clone(), &name, denied.clone(), None);
        assert!(direct.await.is_err());
        let scripted = disabled.execute_as(
            InvocationKind::Script,
            session.root.clone(),
            &name,
            denied,
            None,
        );
        assert!(scripted.await.is_err());
        assert_eq!(script_type(&disabled, &session, &name).await, "undefined");
        shutdown_session(session).await;
        assert_eq!(launched(root.path()), "launched\n");
    }

    /// Replacing a list completes without a result, inspecting returns the
    /// items, and blank text is rejected at its argument path.
    #[tokio::test]
    async fn todo_replace_has_no_result_and_inspect_returns_items() {
        use skyhook_tools::diagnostic::Cause;
        use skyhook_worker::diagnostic::Subject;
        let root = tempfile::tempdir().unwrap();
        let harness = builder(root.path(), Requests::default())
            .build()
            .await
            .unwrap();
        let session = ephemeral_session(&harness).await;
        let executor = &session.runtime.executor;
        let todo = |arguments| bounded(executor.run_model(&session.root, "todo", arguments));
        let items = json!([{"text":"done","status":"completed"}]);
        let replaced = todo(json!({"items":items})).await.unwrap();
        assert_eq!(replaced.output.value, json!({}));
        let inspected = todo(json!({})).await.unwrap();
        assert_eq!(inspected.output.value["result"], json!({"items":items}));
        let blank = json!([{"text":"done","status":"completed"}, {"text":" ","status":"pending"}]);
        let error = executor.execute(session.root.clone(), "todo", json!({"items":blank}), None);
        let error = bounded(error).await.unwrap_err();
        let diagnostic = error.diagnostic();
        assert_eq!(
            diagnostic.context.subject,
            Subject::argument(["items", "1", "text"])
        );
        assert!(matches!(diagnostic.cause, Cause::InvalidArguments(_)));
        shutdown_session(session).await;
    }

    #[tokio::test]
    async fn depth_zero_child_omits_agent_gated_mcp_without_reconnecting() {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let harness = builder(root.path(), requests.clone())
            .mcp(servers(stdio_config(root.path(), vec![Capability::Agents])))
            .build()
            .await
            .unwrap();
        let session = ephemeral_session(&harness).await;
        let name = mcp_name(&session);
        session.prompt("root request").await.unwrap();
        let child =
            session.run_script("return await tool.agent({prompt:'child request', depth:0});");
        assert_eq!(
            child.await.unwrap().value["value"]["result"]["reply"],
            "done"
        );
        {
            let requests = requests.lock().unwrap();
            let offered = |index: usize| requests[index].tools.iter().any(|tool| tool.name == name);
            assert_eq!((requests.len(), offered(0), offered(1)), (2, true, false));
        }
        let child_capabilities = session.runtime.capabilities.for_agent(0);
        let child_executor = session.runtime.executor.clone();
        let child_executor = child_executor.with_capabilities(child_capabilities);
        let denied =
            child_executor.execute(session.root.clone(), &name, json!({"text":"denied"}), None);
        assert!(denied.await.is_err());
        let found = script_type(&child_executor, &session, &name).await;
        assert_eq!(found, "undefined");
        shutdown_session(session).await;
        assert_eq!(launched(root.path()), "launched\n");
    }

    #[tokio::test]
    async fn mcp_stays_on_host_for_remote_location_and_failed_startup_is_reported() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("nonexistent-mcp");
        let failing = json!({"transport": "stdio", "start_command": [missing.to_string_lossy()]});
        let mut mcp = servers(stdio_config(root.path(), vec![]));
        mcp.insert("failing".into(), crate::tests::decode(failing).unwrap());
        let harness = builder(root.path(), Requests::default())
            .mcp(mcp)
            .build()
            .await
            .unwrap();
        let session = ephemeral_session(&harness).await;
        let warnings = session.warnings();
        assert!(
            warnings.len() == 1 && warnings[0].contains("failing"),
            "{warnings:?}"
        );
        let name = mcp_name(&session);
        // This location has no route/worker. A targeted dispatch would fail; Host
        // dispatch must still use the session-owning process and its MCP manager.
        let location = ExecutionLocation::named(
            "unconnected-remote".parse().unwrap(),
            "/remote/workspace".into(),
        );
        let remote = session.runtime.executor.clone().with_location(location);
        let output = remote.execute(session.root.clone(), &name, json!({"text":"host"}), None);
        let output = output.await.unwrap().output.value;
        assert_eq!(output["structuredContent"]["text"], "host");
        shutdown_session(session).await;
    }

    #[tokio::test]
    async fn root_interactive_gate_covers_dispatch_and_nested_scripts() {
        for enabled in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let requests = Requests::default();
            let batches: Arc<StdMutex<Vec<Vec<Question>>>> = Arc::default();
            let mut capabilities = CapabilitySet::default();
            if !enabled {
                capabilities.remove(&Capability::Interactive);
            }
            let questions = RecordingQuestions {
                batches: batches.clone(),
                answer: "host-answer".into(),
                ..Default::default()
            };
            // No turn is scripted for the completions: the held window keeps them.
            let harness = builder(root.path(), requests.clone())
                .coalescing(inbox::Coalescing::held().0)
                .capabilities(capabilities.clone())
                .question_handler(Arc::new(questions))
                .build()
                .await
                .unwrap();
            let session = ephemeral_session(&harness).await;
            let executor = session.runtime.executor.clone();
            let executor = executor.with_capabilities(capabilities);
            session.prompt("list tools").await.unwrap();
            let tools = requests.lock().unwrap()[0].tools.clone();
            assert_eq!(tools.iter().any(|tool| tool.name == "ask"), enabled);
            let ask_type = session.run_script("return typeof tool.ask;").await.unwrap();
            let found = &ask_type.value["value"];
            assert_eq!(*found, if enabled { "function" } else { "undefined" });
            for background in [false, true] {
                for kind in [
                    InvocationKind::Host,
                    InvocationKind::Model,
                    InvocationKind::Script,
                ] {
                    let args = json!({"header":"root", "prompt":"question", "bg":background});
                    let agent = session.root.clone();
                    let result = bounded(executor.execute_as(kind, agent, "ask", args, None)).await;
                    if enabled {
                        let output = terminal(&session, result.unwrap().job).await;
                        assert_eq!(output.output, Some(json!("host-answer")));
                    } else {
                        let cause = result.unwrap_err().diagnostic().cause;
                        assert!(
                            matches!(&cause, skyhook_tools::diagnostic::Cause::Unavailable { tool } if tool == "ask"),
                            "{cause:?}"
                        );
                    }
                }
                // Nested root asks cannot escape the gate through script jobs.
                let source =
                    "return (await tool.ask({header:'nested', prompt:'question'})).unwrap();";
                let arguments = json!({"source":source, "bg":background});
                let result = executor
                    .execute(session.root.clone(), "script", arguments, None)
                    .await;
                if !enabled && !background {
                    assert_matches!(
                        result.unwrap_err().diagnostic().cause,
                        skyhook_tools::diagnostic::Cause::Message(_)
                    );
                    continue;
                }
                let output = terminal(&session, result.unwrap().job).await;
                if enabled {
                    assert_eq!(output.state, crate::job::JobState::Completed);
                    assert_eq!(output.output.unwrap()["value"], "host-answer");
                } else {
                    assert_eq!(output.state, crate::job::JobState::Failed);
                }
            }
            assert_eq!(batches.lock().unwrap().len(), if enabled { 8 } else { 0 });
            if !enabled {
                let jobs = session.runtime.jobs.list(&session.root).await.unwrap();
                assert!(jobs.iter().all(|job| job.tool != "ask"));
            }
            shutdown_session(session).await;
        }
    }

    /// Every built-in is offered with every capability, and its input schema compiles:
    /// schemars does not check declared constraints, and a built-in's schema compiles
    /// only at its first call.
    #[tokio::test]
    async fn builtin_input_schemas_compile() {
        // `skill` is offered only when the host has skills.
        let root = tempfile::tempdir().unwrap();
        let skill = root.path().join(".agents/skills/demo");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "Demo instructions.").unwrap();
        let provider = scripted_provider(&Requests::default(), []);
        let skills = crate::tool::builtins::HostSkills::discover(root.path()).await;
        let harness = test_builder(root.path(), &root.path().join("sessions"), provider, false)
            .skills(skills)
            .build()
            .await
            .unwrap();
        let session = ephemeral_session(&harness).await;
        let capabilities = crate::tool::policy::Capability::ALL.into_iter().collect();
        let registry = session.runtime.executor.registry();
        let surface = registry.surface_for_agent(&capabilities, &session.root);
        let mut found = Vec::new();
        let mut checked = Vec::new();
        for (tool, spec) in surface.every() {
            let spec = spec.unwrap_or_else(|withheld| panic!("{tool}: {withheld}"));
            if let Err(reason) = spec.validator.get() {
                found.push(format!("{tool}: {reason}"));
            }
            checked.push(tool);
        }
        assert!(checked.contains(&"skill"), "{checked:?}");
        assert!(found.is_empty(), "{found:#?}");
    }
}
