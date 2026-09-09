//! MCP wiring through the session runtime (no external model or MCP service).

use super::*;
use crate::{
    execution::ExecutionLocation,
    provider::{
        ProviderContext, ProviderError, ProviderFuture, ResponseStream,
        protocol::{StopReason, events_for_content},
    },
    tool::{ScriptBinding, ToolExposure, ToolPlacement, policy::Capability},
};
use std::{sync::Mutex as StdMutex, time::Duration};

// Keep the protocol fixture dependency-free, as in mcp/tests.rs. The marker is
// written before initialization so even a failed/aborted launch is observable.
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

#[derive(Clone, Default)]
struct RecordingProvider(Arc<StdMutex<Vec<ModelRequest>>>);

impl Provider for RecordingProvider {
    fn open_context(&self, _: String) -> Result<Box<dyn ProviderContext>, ProviderError> {
        Ok(Box::new(self.clone()))
    }
}

impl ProviderContext for RecordingProvider {
    fn invoke(&mut self, request: ModelRequest) -> ProviderFuture {
        self.0.lock().unwrap().push(request);
        let mut events = events_for_content(&[AssistantContent::text("answer", 0, "done")]);
        events.push(ResponseChunk::ResponseEnded {
            stop_reason: StopReason::EndTurn,
        });
        Box::pin(async move {
            Ok(Box::pin(futures_util::stream::iter(events.into_iter().map(Ok))) as ResponseStream)
        })
    }
}

fn builder(root: &Path, provider: RecordingProvider) -> HarnessBuilder {
    HarnessBuilder::new(root)
        .session_root(root.join("sessions"))
        .provider("test", Arc::new(provider))
        .model_profile(
            "test",
            ModelProfile {
                provider: "test".into(),
                model: "test".into(),
                reasoning: None,
                max_context: 128_000,
                max_output: 16_384,
                supports_images: false,
            },
        )
        .default_model_profile("test")
}

fn stdio_config(root: &Path, capabilities: Vec<Capability>) -> McpServerConfig {
    serde_json::from_value(json!({
        "transport":"stdio",
        "start_command":["python3", "-u", "-c", FIXTURE],
        "env":{"MCP_RUNTIME_MARKER":root.join("launched")},
        "capabilities":capabilities,
        "startup_timeout_secs":5,
        "call_timeout_secs":5
    }))
    .unwrap()
}

fn servers(config: McpServerConfig) -> BTreeMap<String, McpServerConfig> {
    BTreeMap::from([("fixture".into(), config)])
}

fn mcp_name(session: &SessionHandle) -> String {
    let names = session
        .runtime
        .executor
        .registry()
        .tools()
        .filter(|tool| tool.name().starts_with("mcp_"))
        .map(|tool| tool.name().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(names.len(), 1, "{:?}", session.startup_warnings());
    names[0].clone()
}

#[tokio::test]
async fn every_required_capability_is_checked_before_stdio_launch_or_http_contact() {
    for missing in Capability::ALL {
        let root = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut capabilities = CapabilitySet::default();
        for capability in Capability::ALL {
            capabilities.insert(capability);
        }
        capabilities.remove(missing);
        let http: McpServerConfig = serde_json::from_value(json!({
            "transport":"streamable_http",
            "url":format!("http://{}/mcp", listener.local_addr().unwrap()),
            "capabilities":Capability::ALL,
            "startup_timeout_secs":1
        }))
        .unwrap();
        let configs = BTreeMap::from([
            (
                "stdio".into(),
                stdio_config(root.path(), Capability::ALL.to_vec()),
            ),
            ("http".into(), http),
        ]);
        let harness = builder(root.path(), RecordingProvider::default())
            .capabilities(capabilities)
            .mcp(configs)
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        assert!(
            session.startup_warnings().is_empty(),
            "missing {missing:?}: {:?}",
            session.startup_warnings()
        );
        assert!(
            !session
                .runtime
                .executor
                .registry()
                .tools()
                .any(|tool| tool.name().starts_with("mcp_"))
        );
        // Shutdown before checking the marker also catches a deferred launch.
        tests::shutdown_session(session).await;
        assert!(
            !root.path().join("launched").exists(),
            "launched with missing {missing:?}"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err(),
            "contacted HTTP server with missing {missing:?}"
        );
    }
}

#[tokio::test]
async fn depth_zero_root_omits_agent_gated_mcp_before_launch_or_http_contact() {
    let root = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http: McpServerConfig = serde_json::from_value(json!({
        "transport":"streamable_http",
        "url":format!("http://{}/mcp", listener.local_addr().unwrap()),
        "capabilities":[Capability::Agents],
        "startup_timeout_secs":1
    }))
    .unwrap();
    // The configured set allows Agents, but a depth-zero root's effective set
    // does not. Startup filtering must use the same set as tool exposure.
    assert!(CapabilitySet::default().contains(Capability::Agents));
    let harness = builder(root.path(), RecordingProvider::default())
        .max_child_depth(0)
        .mcp(BTreeMap::from([
            (
                "stdio".into(),
                stdio_config(root.path(), vec![Capability::Agents]),
            ),
            ("http".into(), http),
        ]))
        .build()
        .await
        .unwrap();
    let session = harness.new_session().await.unwrap();
    assert!(session.startup_warnings().is_empty());
    assert!(
        !session
            .runtime
            .executor
            .registry()
            .tools()
            .any(|tool| tool.name().starts_with("mcp_"))
    );
    tests::shutdown_session(session).await;
    assert!(!root.path().join("launched").exists());
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn empty_capability_requirements_expose_direct_and_script_tools() {
    let root = tempfile::tempdir().unwrap();
    let provider = RecordingProvider::default();
    // Neither Read nor Exec is implicitly required to use an MCP connection.
    let mut capabilities = CapabilitySet::default();
    for capability in Capability::ALL {
        capabilities.remove(capability);
    }
    let harness = builder(root.path(), provider.clone())
        .capabilities(capabilities.clone())
        .mcp(servers(stdio_config(root.path(), vec![])))
        .build()
        .await
        .unwrap();
    let session = harness.new_session().await.unwrap();
    assert!(session.startup_warnings().is_empty());
    let name = mcp_name(&session);
    let executor = session
        .runtime
        .executor
        .clone()
        .with_capabilities(capabilities);
    let surface = executor.surface();
    let spec = surface.get(&name).unwrap();
    assert!(matches!(spec.exposure, ToolExposure::ModelVisible));
    assert!(matches!(spec.script_binding, ScriptBinding::TopLevel));
    assert_eq!(session.prompt("list your tools").await.unwrap(), "done");
    assert!(
        provider.0.lock().unwrap()[0]
            .tools
            .iter()
            .any(|tool| tool.name == name)
    );
    let direct = executor
        .execute(session.root.clone(), &name, json!({"text":"direct"}), None)
        .await
        .unwrap();
    assert_eq!(
        direct.output.value["structuredContent"],
        json!({"text":"direct"})
    );
    let script = session
        .run_script(format!("return await tool.{name}({{text:'script'}});"))
        .await
        .unwrap();
    assert_eq!(
        script.value["value"]["structuredContent"],
        json!({"text":"script"})
    );
    tests::shutdown_session(session).await;
    assert_eq!(
        std::fs::read_to_string(root.path().join("launched")).unwrap(),
        "launched\n"
    );
}

#[tokio::test]
async fn depth_zero_child_omits_agent_gated_mcp_without_reconnecting() {
    let root = tempfile::tempdir().unwrap();
    let provider = RecordingProvider::default();
    let harness = builder(root.path(), provider.clone())
        .max_child_depth(1)
        .mcp(servers(stdio_config(root.path(), vec![Capability::Agents])))
        .build()
        .await
        .unwrap();
    let session = harness.new_session().await.unwrap();
    let name = mcp_name(&session);
    session.prompt("root request").await.unwrap();
    let child = session
        .run_script("return await tool.agent({prompt:'child request', depth:0});")
        .await
        .unwrap();
    assert_eq!(child.value["value"], "done");
    {
        let requests = provider.0.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].tools.iter().any(|tool| tool.name == name));
        assert!(!requests[1].tools.iter().any(|tool| tool.name == name));
    }
    let child_executor = session
        .runtime
        .executor
        .clone()
        .with_capabilities(session.runtime.harness.capabilities.for_agent(0));
    assert!(
        child_executor
            .execute(session.root.clone(), &name, json!({"text":"denied"}), None)
            .await
            .is_err()
    );
    let script = child_executor
        .execute(
            session.root.clone(),
            "script",
            json!({"source":format!("return typeof tool.{name};")}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(script.output.value["value"], "undefined");
    tests::shutdown_session(session).await;
    assert_eq!(
        std::fs::read_to_string(root.path().join("launched")).unwrap(),
        "launched\n"
    );
}

#[tokio::test]
async fn mcp_stays_on_host_for_remote_location_and_is_absent_from_worker_registry() {
    let root = tempfile::tempdir().unwrap();
    let harness = builder(root.path(), RecordingProvider::default())
        .mcp(servers(stdio_config(root.path(), vec![])))
        .build()
        .await
        .unwrap();
    let session = harness.new_session().await.unwrap();
    let name = mcp_name(&session);
    let tool = session
        .runtime
        .executor
        .registry()
        .tools()
        .find(|tool| tool.name() == name)
        .unwrap();
    assert_eq!(tool.placement(), ToolPlacement::Host);
    // This location has no route/worker. A targeted dispatch would fail; Host
    // dispatch must still use the session-owning process and its MCP manager.
    let remote = session
        .runtime
        .executor
        .clone()
        .with_location(ExecutionLocation::named(
            "unconnected-remote",
            PathBuf::from("/remote/workspace"),
        ));
    let output = remote
        .execute(session.root.clone(), &name, json!({"text":"host"}), None)
        .await
        .unwrap();
    assert_eq!(output.output.value["structuredContent"]["text"], "host");
    let mut worker = ToolRegistryBuilder::default();
    crate::tool::builtins::register_worker_tools(&mut worker, session.runtime.store.clone())
        .unwrap();
    crate::tool::builtins::skill_transfer::register_worker(&mut worker).unwrap();
    let worker = worker.build();
    assert!(!worker.tools().any(|tool| tool.name().starts_with("mcp_")));
    tests::shutdown_session(session).await;
}

#[tokio::test]
async fn failed_startup_is_reported_without_breaking_the_session() {
    let root = tempfile::tempdir().unwrap();
    let mut config = stdio_config(root.path(), vec![]);
    config.start_command = Some(vec![
        root.path()
            .join("nonexistent-mcp")
            .to_string_lossy()
            .into_owned(),
    ]);
    let harness = builder(root.path(), RecordingProvider::default())
        .mcp(servers(config))
        .build()
        .await
        .unwrap();
    let session = harness.new_session().await.unwrap();
    assert_eq!(session.startup_warnings().len(), 1);
    assert!(session.startup_warnings()[0].contains("fixture"));
    assert!(
        !session
            .runtime
            .executor
            .registry()
            .tools()
            .any(|tool| tool.name().starts_with("mcp_"))
    );
    assert_eq!(session.prompt("still usable").await.unwrap(), "done");
    tests::shutdown_session(session).await;
}
