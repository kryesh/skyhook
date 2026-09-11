//! Adapt the startup MCP catalog to the ordinary, policy-gated tool registry.

mod arguments;
mod naming;
mod result;

use super::{config::McpServerConfig, manager::McpManager};
use crate::{
    session::SessionStore,
    tool::{
        ToolOptions, ToolPlacement, ToolRegistryBuilder,
        policy::{Capability, ResourceId},
    },
};
use arguments::Arguments;
use naming::tool_names;
use result::{map_error, map_result};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

/// Register valid discovered tools, reporting individual failures without losing
/// the rest of the catalog. The root and its children share the same manager.
/// MCP calls always execute on the host, even from a remote workspace.
pub fn register(
    builder: &mut ToolRegistryBuilder,
    manager: Arc<McpManager>,
    configs: &BTreeMap<String, McpServerConfig>,
    store: SessionStore,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let generated_names = tool_names(manager.catalog(), |name| builder.contains_name(name));
    for (discovered, name) in manager.catalog().iter().zip(generated_names) {
        let server = &discovered.server;
        let tool = &discovered.tool;
        let Some(config) = configs.get(server) else {
            warnings.push(format!(
                "MCP tool {server}/{} skipped: server configuration is missing",
                tool.name
            ));
            continue;
        };
        let adapted = match Arguments::new(Value::Object((*tool.input_schema).clone())) {
            Ok(arguments) => Arc::new(arguments),
            Err(error) => {
                warnings.push(format!("MCP tool {server}/{} skipped: {error}", tool.name));
                continue;
            }
        };
        let validator = adapted.clone();
        let options = ToolOptions::new(config.capabilities.clone())
            .requires(Capability::Mcp)
            .preserve_required()
            .preserve_schema_dialect()
            .background()
            .placement(ToolPlacement::Host)
            .permission_resource(ResourceId::new(
                "mcp",
                [server.as_str(), tool.name.as_ref()],
            ))
            .argument_validator(move |value| validator.validate(value));
        let manager = manager.clone();
        let store = store.clone();
        let server = server.clone();
        let upstream_name = tool.name.to_string();
        let description = format!(
            "MCP tool {server}/{upstream_name}. {}{}",
            tool.description.as_deref().unwrap_or(""),
            if adapted.wrapped {
                " Pass the upstream tool input in arguments; bg controls the Skyhook job."
            } else {
                ""
            },
        );
        if let Err(error) = builder.register_dynamic(
            name,
            description,
            adapted.schema.clone(),
            options,
            move |context, value| {
                let manager = manager.clone();
                let store = store.clone();
                let server = server.clone();
                let upstream_name = upstream_name.clone();
                let adapted = adapted.clone();
                async move {
                    let arguments = adapted.extract(&value)?.clone();
                    let result = manager
                        .call(
                            &server,
                            &upstream_name,
                            arguments,
                            context.authorization.cancellation.clone(),
                        )
                        .await
                        .map_err(map_error)?;
                    map_result(result, &store).await
                }
            },
        ) {
            warnings.push(format!(
                "MCP tool {}/{} skipped: {error}",
                discovered.server, tool.name
            ));
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        job::CancellationToken,
        tests::TestRuntime,
        tool::{
            executor::ToolExecutor,
            policy::{AuthorizationRequest, CapabilitySet, Policy, PolicyDecision, PolicyFuture},
        },
    };
    use serde_json::json;
    use std::sync::Mutex;

    const FIXTURE: &str = r#"
import json, sys
for line in sys.stdin:
    req = json.loads(line)
    if 'id' not in req: continue
    method = req['method']
    if method == 'initialize':
        result = {'protocolVersion':req['params']['protocolVersion'],'capabilities':{'tools':{}},'serverInfo':{'name':'adapter-test','version':'1'}}
    elif method == 'tools/list':
        result = {'tools':[
          {'name':'native','inputSchema':{'type':'object','properties':{'count':{'type':'integer','minimum':1,'default':1},'title':{'type':'string'},'format':{'type':'string'},'$schema':{'type':'string'}},'required':['count'],'additionalProperties':False}},
          {'name':'open','inputSchema':{'type':'object'}},
          {'name':'invalid','inputSchema':{'type':'array'}}]}
    elif method == 'tools/call':
        result = {'content':[], 'structuredContent':{'tool':req['params']['name'],'arguments':req['params']['arguments']},'isError':False}
    else: result = {}
    print(json.dumps({'jsonrpc':'2.0','id':req['id'],'result':result}), flush=True)
"#;

    #[derive(Default)]
    struct RecordingPolicy(Mutex<Vec<AuthorizationRequest>>);
    impl Policy for RecordingPolicy {
        fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_> {
            self.0.lock().unwrap().push(request);
            Box::pin(async { PolicyDecision::allow() })
        }
    }

    #[tokio::test]
    async fn registration_dispatch_capabilities_permissions_and_preapproval_validation() {
        let runtime = TestRuntime::new().await;
        let config = serde_json::from_value(json!({
            "transport":"stdio", "start_command":["python3","-u","-c",FIXTURE],
            "capabilities":["read","exec"], "startup_timeout_secs":5
        }))
        .unwrap();
        let configs = BTreeMap::from([("fixture".to_owned(), config)]);
        let manager = Arc::new(McpManager::connect(&configs, CancellationToken::new()).await);
        assert_eq!(manager.catalog().len(), 3, "{:?}", manager.warnings());
        let mut builder = ToolRegistryBuilder::default();
        let warnings = register(
            &mut builder,
            manager.clone(),
            &configs,
            runtime.store.clone(),
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("invalid"));
        let registry = builder.build();
        let native = "mcp_fixture_native";
        let open = "mcp_fixture_open";
        for tool in registry.tools() {
            assert_eq!(tool.placement(), ToolPlacement::Host);
        }
        for missing in [Capability::Exec, Capability::Mcp] {
            let mut limited = CapabilitySet::default();
            limited.remove(missing);
            assert!(registry.surface(&limited).get(native).is_none());
        }
        let mut without_mcp = CapabilitySet::default();
        without_mcp.remove(Capability::Mcp);
        let policy = Arc::new(RecordingPolicy::default());
        let executor = ToolExecutor::new(
            registry,
            policy.clone(),
            runtime.jobs.clone(),
            runtime.root.path().to_path_buf(),
        );
        let disabled = executor.clone().with_capabilities(without_mcp);
        assert!(
            disabled
                .execute(runtime.agent.clone(), native, json!({"count":1}), None)
                .await
                .is_err()
        );
        assert!(
            disabled
                .execute_script(runtime.agent.clone(), native, json!({"count":1}), None)
                .await
                .is_err()
        );
        assert!(policy.0.lock().unwrap().is_empty());
        for invalid in [json!({"count":"bad"}), json!({})] {
            assert!(
                executor
                    .execute(runtime.agent.clone(), native, invalid, None)
                    .await
                    .is_err()
            );
            assert!(
                policy.0.lock().unwrap().is_empty(),
                "invalid or defaulted required input must not request approval"
            );
        }
        let native_input =
            json!({"count":2,"title":"user title","format":"user format","$schema":"user data"});
        let output = executor
            .execute(runtime.agent.clone(), native, native_input.clone(), None)
            .await
            .unwrap();
        assert_eq!(
            output.output.value["structuredContent"]["arguments"],
            native_input
        );
        let output = executor
            .execute(
                runtime.agent.clone(),
                open,
                json!({"arguments":{"bg":"upstream","target":"upstream"}}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            output.output.value["structuredContent"]["arguments"],
            json!({"bg":"upstream","target":"upstream"})
        );
        let output = executor
            .execute_script(
                runtime.agent.clone(),
                open,
                json!({"arguments":{"bg":"upstream"},"bg":true}),
                None,
            )
            .await
            .unwrap();
        assert!(output.background);
        let envelope = runtime.jobs.wait(output.job, None, true).await.unwrap();
        assert_eq!(
            envelope.output.unwrap()["structuredContent"]["arguments"],
            json!({"bg":"upstream"})
        );
        for request in policy.0.lock().unwrap().iter() {
            assert_eq!(request.permissions.len(), 2);
            assert!(
                request
                    .permissions
                    .iter()
                    .all(|permission| permission.resource.namespace == "mcp"
                        && permission.resource.segments.len() == 2
                        && permission.resource.segments[0] == "fixture"
                        && matches!(permission.resource.segments[1].as_str(), "native" | "open"))
            );
        }
        manager.shutdown().await;
    }
}
