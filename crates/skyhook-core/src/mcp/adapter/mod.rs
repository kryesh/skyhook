//! Adapt the startup MCP catalog to the ordinary, policy-gated tool registry.

mod arguments;
mod naming;
mod result;

use super::manager::McpManager;
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
use std::sync::Arc;

/// Register valid discovered tools, reporting individual failures without losing
/// the rest of the catalog. The root and its children share the same manager.
/// MCP calls always execute on the host, even from a remote workspace.
pub fn register(
    builder: &mut ToolRegistryBuilder,
    manager: Arc<McpManager>,
    store: SessionStore,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let generated_names = tool_names(manager.catalog(), |name| builder.contains_name(name));
    for (discovered, name) in manager.catalog().iter().zip(generated_names) {
        let server = discovered.server.as_str();
        let tool = &discovered.tool;
        let adapted = match Arguments::new(Value::Object((*tool.input_schema).clone())) {
            Ok(arguments) => Arc::new(arguments),
            Err(error) => {
                warnings.push(format!("MCP tool {server}/{} skipped: {error}", tool.name));
                continue;
            }
        };
        let validator = adapted.clone();
        let options = ToolOptions::new(discovered.capabilities.clone())
            .requires(Capability::Mcp)
            .preserve_required()
            .preserve_schema_dialect()
            .background()
            .placement(ToolPlacement::Host)
            .permission_resource(ResourceId::mcp(server, tool.name.as_ref()))
            .argument_validator(move |value| validator.validate(value));
        let manager = manager.clone();
        let store = store.clone();
        let server = server.to_owned();
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
                let server = server.to_owned();
                let upstream_name = upstream_name.clone();
                let adapted = adapted.clone();
                async move {
                    let arguments = adapted.extract(&value)?.clone();
                    let result = manager
                        .call(
                            &server,
                            &upstream_name,
                            arguments,
                            context.cancellation_token(),
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
        tests::{RecordingPolicy, TestRuntime},
        tool::{executor::ToolExecutor, policy::CapabilitySet},
    };
    use serde_json::json;

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

    fn without(capability: Capability) -> CapabilitySet {
        let mut capabilities = CapabilitySet::default();
        capabilities.remove(capability);
        capabilities
    }

    #[tokio::test]
    async fn registration_dispatch_capabilities_permissions_and_preapproval_validation() {
        let runtime = TestRuntime::new().await;
        let config = serde_json::from_value(json!({
            "transport":"stdio", "start_command":["python3","-u","-c",FIXTURE],
            "capabilities":["read","exec"], "startup_timeout_secs":5
        }))
        .unwrap();
        let mut configs = std::collections::BTreeMap::from([("fixture".to_owned(), config)]);
        let defaults = CapabilitySet::default();
        let manager = McpManager::connect(&configs, &defaults, CancellationToken::new()).await;
        let manager = Arc::new(manager);
        assert_eq!(manager.catalog().len(), 3, "{:?}", manager.warnings());
        // Registration must use admitted catalog policy, not a later config join.
        configs.clear();
        assert_eq!(
            manager.catalog()[0].capabilities,
            [Capability::Read, Capability::Exec]
        );
        let mut builder = ToolRegistryBuilder::default();
        let warnings = register(&mut builder, manager.clone(), runtime.store.clone());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("invalid"));
        let registry = builder.build();
        let (native, open) = ("mcp_fixture_native", "mcp_fixture_open");
        assert!(
            registry
                .tools()
                .all(|tool| tool.placement() == ToolPlacement::Host)
        );
        for missing in [Capability::Exec, Capability::Mcp] {
            assert!(registry.surface(&without(missing)).get(native).is_none());
        }
        let policy = RecordingPolicy::allowing();
        let root = runtime.root.path().to_path_buf();
        let executor = ToolExecutor::new(registry, policy.clone(), runtime.jobs.clone(), root);
        let agent = || runtime.agent.clone();
        let disabled = executor.clone().with_capabilities(without(Capability::Mcp));
        let count = json!({"count":1});
        assert!(
            disabled
                .execute(agent(), native, count.clone(), None)
                .await
                .is_err()
        );
        assert!(
            disabled
                .execute_script(agent(), native, count, None)
                .await
                .is_err()
        );
        assert!(policy.requests.lock().unwrap().is_empty());
        for invalid in [json!({"count":"bad"}), json!({})] {
            assert!(
                executor
                    .execute(agent(), native, invalid, None)
                    .await
                    .is_err()
            );
            assert!(
                policy.requests.lock().unwrap().is_empty(),
                "invalid or defaulted required input must not request approval"
            );
        }
        let arguments = |output: &Value| output["structuredContent"]["arguments"].clone();
        let native_input =
            json!({"count":2,"title":"user title","format":"user format","$schema":"user data"});
        let output = executor
            .execute(agent(), native, native_input.clone(), None)
            .await;
        assert_eq!(arguments(&output.unwrap().output.value), native_input);
        let wrapped = json!({"bg":"upstream","target":"upstream"});
        let input = json!({ "arguments": wrapped });
        let output = executor.execute(agent(), open, input, None).await;
        assert_eq!(arguments(&output.unwrap().output.value), wrapped);
        let input = json!({"arguments":{"bg":"upstream"},"bg":true});
        let output = executor
            .execute_script(agent(), open, input, None)
            .await
            .unwrap();
        assert!(output.background);
        let envelope = runtime.jobs.wait(output.job, None, true).await.unwrap();
        assert_eq!(
            arguments(&envelope.output.unwrap()),
            json!({"bg":"upstream"})
        );
        for request in policy.requests.lock().unwrap().iter() {
            assert_eq!(request.permissions.len(), 2);
            assert!(request.permissions.iter().all(|permission| {
                ["native", "open"]
                    .into_iter()
                    .any(|tool| permission.resource == ResourceId::mcp("fixture", tool))
            }));
        }
        manager.shutdown().await;
    }
}
