use super::*;
use crate::{
    job::CancellationToken,
    test_support::TestRuntime,
    tool::{
        ScriptBinding, ToolExposure,
        executor::ToolExecutor,
        policy::{
            AuthorizationRequest, Capability, CapabilitySet, Policy, PolicyDecision, PolicyFuture,
        },
    },
};
use std::sync::Mutex;

fn discovered(server: &str, tool: &str) -> DiscoveredTool {
    DiscoveredTool {
        server: server.to_owned(),
        tool: serde_json::from_value(json!({"name":tool,"inputSchema":{"type":"object"}})).unwrap(),
    }
}

fn tool_name(item: &DiscoveredTool) -> String {
    tool_names(std::slice::from_ref(item), |_| false).remove(0)
}

#[test]
fn ordinary_names_remain_readable_without_hashes() {
    assert_eq!(
        tool_name(&discovered("filesystem", "write_file")),
        "mcp_filesystem_write_file"
    );
    assert_eq!(
        tool_name(&discovered("filesystem", "read_text_file")),
        "mcp_filesystem_read_text_file"
    );
    assert_eq!(
        tool_name(&discovered("docs-v2", "Search")),
        "mcp_docs-v2_Search"
    );
    let boundary = "x".repeat(57);
    let name = tool_name(&discovered("s", &boundary));
    assert_eq!(name, format!("mcp_s_{boundary}"));
    assert_eq!(name.len(), 63);
    assert_eq!(tool_name(&discovered("s", &"x".repeat(58))).len(), 64);
}

#[test]
fn names_are_stable_safe_bounded_and_collision_resistant() {
    let long = "x".repeat(500);
    let identities = [
        ("a-b", "c"),
        ("a_b", "c"),
        ("a", "b_c"),
        ("a_b", "c_"),
        ("👋", "工具"),
        ("", ""),
        ("server", "background"),
        (long.as_str(), "first"),
        (long.as_str(), "second"),
    ];
    let mut catalog: Vec<_> = identities
        .iter()
        .map(|(server, tool)| discovered(server, tool))
        .collect();
    let names = tool_names(&catalog, |_| false);
    assert_eq!(names, tool_names(&catalog, |_| false));
    assert_eq!(names.iter().collect::<BTreeSet<_>>().len(), names.len());
    for name in &names {
        assert!(name.starts_with("mcp_"));
        assert!(name.len() <= 64);
        assert!(
            name.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        );
    }
    // Both ambiguous identities receive suffixes, rather than first-one-wins.
    assert!(names[1].starts_with("mcp_a_b_c_"));
    assert!(names[2].starts_with("mcp_a_b_c_"));
    assert_ne!(names[1], names[2]);
    assert_eq!(names[7].len(), 64);
    assert_eq!(names[8].len(), 64);
    assert_eq!(names[7].rsplit('_').next().unwrap().len(), 8);
    catalog.reverse();
    let mut reversed = tool_names(&catalog, |_| false);
    reversed.reverse();
    assert_eq!(names, reversed);
    // Adding unrelated names does not rename existing tools.
    catalog.reverse();
    catalog.push(discovered("unrelated", "new_tool"));
    assert_eq!(&tool_names(&catalog, |_| false)[..names.len()], names);
}

#[test]
fn sanitization_and_truncation_only_add_short_hashes_when_needed() {
    for item in [
        discovered("bad.server", "write"),
        discovered("s", &"x".repeat(59)),
    ] {
        let name = tool_name(&item);
        let suffix = name.rsplit('_').next().unwrap();
        assert_eq!(suffix.len(), 8);
        assert!(suffix.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(name.len() <= 64);
    }
    assert_ne!(
        tool_name(&discovered("bad.server", "write")),
        tool_name(&discovered("bad/server", "write"))
    );
}

#[test]
fn natural_names_take_precedence_over_generated_suffixes() {
    let invalid = discovered("bad.server", "write");
    let generated = tool_name(&invalid);
    let natural = discovered(
        "bad_server",
        generated.strip_prefix("mcp_bad_server_").unwrap(),
    );
    let names = tool_names(&[invalid.clone(), natural.clone()], |_| false);
    assert_eq!(names[1], generated);
    assert_ne!(names[0], names[1]);
    assert_eq!(names[0], format!("{generated}_1"));
    let reverse = tool_names(&[natural, invalid], |_| false);
    assert_eq!(names, vec![reverse[1].clone(), reverse[0].clone()]);
}

#[test]
fn registered_tool_names_are_never_taken_by_mcp() {
    let item = discovered("filesystem", "write_file");
    let raw = tool_name(&item);
    let fallback = tool_names(std::slice::from_ref(&item), |name| name == raw).remove(0);
    let mut builder = ToolRegistryBuilder::default();
    for name in [&raw, &fallback] {
        builder
            .register_dynamic(
                name.as_str(),
                "host tool",
                json!({"type":"object"}),
                ToolOptions::default(),
                |_, _| async { Ok(ToolOutput::new(Value::Null)) },
            )
            .unwrap();
    }
    let names = tool_names(&[item], |name| builder.contains_name(name));
    assert_eq!(names[0], format!("{fallback}_1"));
    assert!(builder.contains_name(&raw));
    assert!(builder.contains_name(&fallback));
    assert!(!builder.contains_name(&names[0]));
}

#[test]
fn native_schema_validates_required_nested_types_enums_and_unknown_properties() {
    let schema = json!({
        "type":"object", "additionalProperties":false,
        "properties":{"nested":{"type":"object", "properties":{"choice":{"enum":["yes"]}}, "required":["choice"], "additionalProperties":false}},
        "required":["nested"]
    });
    let arguments = Arguments::new(schema.clone()).unwrap();
    assert!(!arguments.wrapped);
    assert_eq!(arguments.schema, schema);
    assert!(
        arguments
            .validate(&json!({"nested":{"choice":"yes"}}))
            .is_ok()
    );
    for invalid in [
        json!({}),
        json!({"nested":3}),
        json!({"nested":{}}),
        json!({"nested":{"choice":"no"}}),
        json!({"nested":{"choice":"yes","extra":true}}),
        json!({"nested":{"choice":"yes"},"extra":true}),
    ] {
        assert!(arguments.validate(&invalid).is_err(), "accepted {invalid}");
    }
}

#[test]
fn reserved_and_open_ended_inputs_are_wrapped_without_losing_bg() {
    for schema in [
        json!({"type":"object"}),
        json!({"type":"object","additionalProperties":true}),
        json!({"type":"object","additionalProperties":{"type":"string"}}),
        json!({"type":"object","properties":{"bg":{"type":"string"}},"additionalProperties":false}),
        json!({"type":"object","patternProperties":{"^b":{"type":"string"}},"additionalProperties":false}),
    ] {
        let arguments = Arguments::new(schema).unwrap();
        assert!(arguments.wrapped);
        let value = json!({"arguments":{"bg":"upstream"}});
        arguments.validate(&value).unwrap();
        assert_eq!(arguments.extract(&value).unwrap()["bg"], "upstream");
        assert!(arguments.validate(&json!({})).is_err());
        assert!(arguments.validate(&json!({"arguments":null})).is_err());
        assert!(
            arguments
                .validate(&json!({"arguments":{},"other":1}))
                .is_err()
        );
    }
}

#[test]
fn object_wide_constraints_do_not_interpret_the_job_bg_property() {
    for constraint in [
        json!({"maxProperties":1}),
        json!({"propertyNames":{"enum":["count"]}}),
        json!({"allOf":[{"properties":{"count":{"minimum":1}},"additionalProperties":false}]}),
        json!({"const":{"count":1}}),
    ] {
        let mut original = json!({
            "type":"object", "properties":{"count":{"type":"integer"}},
            "required":["count"], "additionalProperties":false
        });
        original
            .as_object_mut()
            .unwrap()
            .extend(constraint.as_object().unwrap().clone());
        let arguments = Arguments::new(original).unwrap();
        assert!(arguments.wrapped);
        arguments
            .validate(&json!({"arguments":{"count":1}}))
            .unwrap();
        let mut schema = arguments.schema.clone();
        schema["properties"]["bg"] = json!({"type":"boolean"});
        let surface = jsonschema::options()
            .with_retriever(NoExternalSchemas)
            .build(&schema)
            .unwrap();
        assert!(surface.is_valid(&json!({"arguments":{"count":1},"bg":true})));
    }
}

#[test]
fn local_references_keep_their_meaning_in_wrapped_schema() {
    for dialect in [
        "http://json-schema.org/draft-04/schema#",
        "http://json-schema.org/draft-06/schema#",
        "http://json-schema.org/draft-07/schema#",
        "https://json-schema.org/draft/2019-09/schema",
        "https://json-schema.org/draft/2020-12/schema",
    ] {
        let arguments = Arguments::new(json!({
            "$schema":dialect,
            "type":"object", "definitions":{"number":{"type":"integer","minimum":1}},
            "properties":{"count":{"$ref":"#/definitions/number"}},"required":["count"]
        }))
        .unwrap();
        let surface_validator = jsonschema::options()
            .with_retriever(NoExternalSchemas)
            .build(&arguments.schema)
            .unwrap_or_else(|error| panic!("{dialect}: {error}"));
        let valid = json!({"arguments":{"count":2}});
        let invalid = json!({"arguments":{"count":0}});
        assert!(arguments.validate(&valid).is_ok(), "{dialect}");
        assert!(surface_validator.is_valid(&valid), "{dialect}");
        assert!(arguments.validate(&invalid).is_err(), "{dialect}");
        assert!(!surface_validator.is_valid(&invalid), "{dialect}");
    }
}

#[test]
fn reference_siblings_cannot_hide_upstream_bg() {
    let arguments = Arguments::new(json!({
        "$schema":"http://json-schema.org/draft-07/schema#",
        "type":"object", "additionalProperties":false,
        "definitions":{"open":{"type":"object"}}, "$ref":"#/definitions/open"
    }))
    .unwrap();
    assert!(arguments.wrapped);
    let input = json!({"arguments":{"bg":"upstream"}});
    arguments.validate(&input).unwrap();
    let surface = jsonschema::options()
        .with_retriever(NoExternalSchemas)
        .build(&arguments.schema)
        .unwrap_or_else(|error| panic!("{error}: {}", arguments.schema));
    assert!(surface.is_valid(&input));
}

#[test]
fn legacy_recursive_root_aliases_retain_their_constraints() {
    for dialect in [
        "http://json-schema.org/draft-04/schema#",
        "http://json-schema.org/draft-06/schema#",
        "http://json-schema.org/draft-07/schema#",
    ] {
        let arguments = Arguments::new(json!({
            "$schema":dialect, "type":"object", "additionalProperties":false,
            "$ref":"#/definitions/node", "definitions":{"node":{
                "type":"object", "required":["count"],
                "properties":{"count":{"type":"integer","minimum":1},
                              "child":{"$ref":"#/definitions/node"}}
            }}
        }))
        .unwrap();
        let surface = jsonschema::options()
            .with_retriever(NoExternalSchemas)
            .build(&arguments.schema)
            .unwrap();
        let valid = json!({"arguments":{"count":1,"child":{"count":2},"bg":"upstream"}});
        let invalid = json!({"arguments":{"count":1,"child":{"count":0}}});
        assert!(arguments.validate(&valid).is_ok(), "{dialect}");
        assert!(surface.is_valid(&valid), "{dialect}");
        assert!(arguments.validate(&invalid).is_err(), "{dialect}");
        assert!(!surface.is_valid(&invalid), "{dialect}");
    }
}

#[test]
fn invalid_schemas_and_external_references_are_rejected() {
    for schema in [
        json!({"type":"array"}),
        json!({"type":"object","properties":{"x":{"type":"not-a-type"}}}),
        json!({"type":"object","$ref":"https://example.invalid/schema.json"}),
        json!({"type":"object","$ref":"file:///etc/passwd"}),
        json!({"type":"object","properties":{"secret":{"$ref":"file:///etc/passwd"}}}),
        json!({"type":"object","$schema":"https://example.invalid/metaschema.json"}),
        json!({"type":"object","$id":"https://example.invalid/root","$ref":"child.json"}),
    ] {
        assert!(Arguments::new(schema.clone()).is_err(), "accepted {schema}");
    }
}

#[tokio::test]
async fn result_envelope_and_errors_preserve_upstream_content() {
    let runtime = TestRuntime::new().await;
    for flag in [None, Some(false), Some(true)] {
        let mut value = json!({
            "content":[{"type":"text","text":"details"},{"type":"resource","resource":{"uri":"test:///item","text":"body"}},{"type":"audio","mimeType":"audio/wav","data":"YWJj"}],
            "structuredContent":{"answer":42},"_meta":{"example":"value"}
        });
        if let Some(flag) = flag {
            value["isError"] = json!(flag);
        }
        let result: CallToolResult = serde_json::from_value(value.clone()).unwrap();
        let mapped = map_result(result, &runtime.store).await;
        let output = if flag == Some(true) {
            let ToolError::FailedWithOutput { output, .. } = mapped.unwrap_err() else {
                panic!("MCP error must retain output")
            };
            *output
        } else {
            mapped.unwrap()
        };
        assert_eq!(output.value, value);
        assert!(output.images.is_empty());
    }
    assert!(matches!(
        map_error(McpError::Cancelled),
        ToolError::Cancelled
    ));
    assert!(matches!(map_error(McpError::Timeout), ToolError::Failed(_)));
}

#[tokio::test]
async fn images_use_persistent_blobs_even_in_failed_results() {
    let runtime = TestRuntime::new().await;
    let encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a/1cAAAAASUVORK5CYII=";
    let result = serde_json::from_value(json!({
        "content":[{"type":"text","text":"image follows"},{"type":"image","mimeType":"image/png","data":encoded}],
        "structuredContent":{"ok":false}, "isError":true
    })).unwrap();
    let ToolError::FailedWithOutput { output, .. } =
        map_result(result, &runtime.store).await.unwrap_err()
    else {
        panic!("expected failed output")
    };
    assert_eq!(output.images.len(), 1);
    assert!(output.images[0].data_base64.is_none());
    assert_eq!(
        runtime.store.read_blob(&output.images[0]).await.unwrap(),
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap()
    );
    assert_eq!(output.value["content"][0]["text"], "image follows");
    assert!(output.value["content"][1].get("data").is_none());
    assert_eq!(
        output.value["content"][1]["image"]["sha256"],
        output.images[0].sha256
    );
    assert_eq!(output.value["structuredContent"], json!({"ok":false}));
}

#[tokio::test]
async fn malformed_unsupported_and_oversized_images_fail_safely() {
    let runtime = TestRuntime::new().await;
    for (mime, data) in [
        ("image/png", "not base64!".to_owned()),
        ("image/svg+xml", "YWJj".to_owned()),
        (
            "image/png",
            "A".repeat((MAX_IMAGE_BYTES as usize).div_ceil(3) * 4 + 1),
        ),
    ] {
        let result = serde_json::from_value(json!({"content":[
            {"type":"image","mimeType":mime,"data":data},
            {"type":"image","mimeType":"image/png","data":"YWJj"},
            {"type":"text","text":"preserved"}
        ]}))
        .unwrap();
        let ToolError::FailedWithOutput { output, .. } =
            map_result(result, &runtime.store).await.unwrap_err()
        else {
            panic!("expected failed output")
        };
        assert!(output.images.is_empty());
        for index in 0..2 {
            assert!(output.value["content"][index].get("data").is_none());
            assert!(output.value["content"][index]["imageError"].is_string());
        }
        assert_eq!(output.value["content"][2]["text"], "preserved");
    }
}

// A real stdio peer ensures the adapter exercises the manager's catalog and
// dispatch rather than a second, adapter-only mock transport.
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
          {'name':'legacy-ref','inputSchema':{'$schema':'http://json-schema.org/draft-07/schema#','type':'object','additionalProperties':False,'definitions':{'open':{'type':'object'}},'$ref':'#/definitions/open'}},
          {'name':'legacy-four','inputSchema':{'$schema':'http://json-schema.org/draft-04/schema#','type':'object','definitions':{'positive':{'type':'number','minimum':0,'exclusiveMinimum':True}},'properties':{'count':{'$ref':'#/definitions/positive'}},'required':['count']}},
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
    assert_eq!(manager.catalog().len(), 5, "{:?}", manager.warnings());
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
    let native = tool_name(&discovered("fixture", "native"));
    let open = tool_name(&discovered("fixture", "open"));
    assert_eq!(native, "mcp_fixture_native");
    assert_eq!(open, "mcp_fixture_open");
    for tool in registry.tools() {
        assert_eq!(tool.placement(), ToolPlacement::Host);
        assert_eq!(
            tool.capabilities(),
            vec![Capability::Read, Capability::Exec]
        );
        let resource = tool.permission_resource().unwrap();
        assert_eq!(resource.namespace, "mcp");
        assert_eq!(resource.segments[0], "fixture");
        let spec = tool.spec(&CapabilitySet::default()).unwrap();
        assert!(matches!(spec.exposure, ToolExposure::ModelVisible));
        assert!(matches!(spec.script_binding, ScriptBinding::TopLevel));
        assert_eq!(spec.input_schema["properties"]["bg"]["type"], "boolean");
        if spec.name == native {
            assert_eq!(spec.input_schema["required"], json!(["count"]));
            for name in ["title", "format", "$schema"] {
                assert_eq!(spec.input_schema["properties"][name]["type"], "string");
            }
        }
        let surface = jsonschema::options()
            .with_retriever(NoExternalSchemas)
            .build(&spec.input_schema)
            .unwrap_or_else(|error| panic!("{}: {error}", spec.name));
        if spec.name == tool_name(&discovered("fixture", "legacy-ref")) {
            assert!(surface.is_valid(&json!({"arguments":{"bg":"upstream"},"bg":true})));
        }
        if spec.name == tool_name(&discovered("fixture", "legacy-four")) {
            assert!(surface.is_valid(&json!({"arguments":{"count":1},"bg":true})));
            assert!(!surface.is_valid(&json!({"arguments":{"count":0}})));
        }
    }
    let mut limited = CapabilitySet::default();
    limited.remove(Capability::Exec);
    assert!(registry.surface(&limited).get(&native).is_none());
    let policy = Arc::new(RecordingPolicy::default());
    let executor = ToolExecutor::new(
        registry,
        policy.clone(),
        runtime.jobs.clone(),
        runtime.root.path().to_path_buf(),
    );
    assert!(
        executor
            .execute(runtime.agent.clone(), &native, json!({"count":"bad"}), None)
            .await
            .is_err()
    );
    assert!(
        policy.0.lock().unwrap().is_empty(),
        "invalid input must not request approval"
    );
    assert!(
        executor
            .execute(runtime.agent.clone(), &native, json!({}), None)
            .await
            .is_err(),
        "JSON Schema defaults must not satisfy required input"
    );
    assert!(policy.0.lock().unwrap().is_empty());
    let native_input =
        json!({"count":2,"title":"user title","format":"user format","$schema":"user data"});
    let output = executor
        .execute(runtime.agent.clone(), &native, native_input.clone(), None)
        .await
        .unwrap();
    assert_eq!(
        output.output.value["structuredContent"]["arguments"],
        native_input
    );
    let output = executor
        .execute(
            runtime.agent.clone(),
            &open,
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
            &open,
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
