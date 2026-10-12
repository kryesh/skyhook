//! Encode request options and the input array as a native Responses body.
use super::*;
use crate::provider::{
    ProviderErrorKind::InvalidRequest,
    codec::{
        CodecName,
        common::{check_tool, system_text},
        set_each,
    },
    protocol::ModelRequest,
};
use serde_json::Map;

/// The body fields this codec writes.
pub(crate) const BODY_FIELDS: &[&str] = &[
    "model",
    "input",
    "stream",
    "store",
    "include",
    "reasoning.summary",
    "service_tier",
    "access_programs",
    "instructions",
    "tools",
    "text",
    "prompt_cache_options",
];

pub(crate) fn encode(
    request: &ModelRequest,
    dialect: &Dialect,
) -> Result<Map<String, Value>, ProviderError> {
    let input = super::input::encode(request, dialect)?;
    let mut settings = serde_json::Map::from_iter([
        ("model".into(), json!(request.model)),
        ("stream".into(), json!(true)),
    ]);
    // Gateways can inspect routing fields before reading a large input array.
    if let Some(tier) = &dialect.service_tier {
        settings.insert("service_tier".into(), json!(tier));
    }
    settings.extend([
        ("input".into(), Value::Array(input)),
        ("store".into(), json!(false)),
        ("include".into(), json!(["reasoning.encrypted_content"])),
    ]);
    if dialect.cache == Cache::Explicit {
        settings.insert("prompt_cache_options".into(), json!({"mode":"explicit"}));
    }
    if let Some(cyber) = dialect.cyber_access_program {
        settings.insert("access_programs".into(), json!({"cyber": cyber}));
    }
    if let Some(summary) = dialect.reasoning_summary {
        settings.insert("reasoning".into(), json!({"summary": summary}));
    }
    match (system_text(request), dialect.instructions) {
        (Some(system), _) => {
            settings.insert("instructions".into(), Value::String(system));
        }
        (None, Instructions::RequiredEvenWhenEmpty) => {
            settings.insert("instructions".into(), Value::String(String::new()));
        }
        (None, Instructions::WhenPresent) => {}
    }
    if !request.tools.is_empty() {
        let mut tools = Vec::new();
        for tool in &request.tools {
            check_tool(tool, dialect.tool_names, CodecName::Responses)?;
            tools.push(json!({"type":"function", "name":tool.name,
                "description":tool.description, "parameters":tool.input_schema, "strict":false}));
        }
        settings.insert("tools".into(), Value::Array(tools));
    }
    if let Some(schema) = &request.response_schema {
        if schema.name.is_empty() || !schema.schema.is_object() {
            return Err(InvalidRequest
                .error("Responses structured output requires a name and object JSON Schema"));
        }
        settings.insert(
            "text".into(),
            json!({"format":{"type":"json_schema", "name":schema.name,
            "schema":schema.schema, "strict":true}}),
        );
    }
    if let Some(reasoning) = &request.reasoning {
        dialect.effort.place(&mut settings, reasoning)?;
    }
    if let Some(max) = request.max_output_tokens {
        set_each(dialect.output_limit.iter(), &mut settings, json!(max))?;
    }
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{
        codec::common::tests::request,
        protocol::{ResponseSchema, SystemSegment, ToolDefinition},
    };

    #[test]
    fn request_selectors_are_optional_and_routing_precedes_input() {
        let req = request("gpt-6-astra");
        for (tier, cyber) in [
            (None, None),
            (Some("priority"), None),
            (None, Some(CyberAccessProgram::Standard)),
            (Some("ultrafast"), Some(CyberAccessProgram::DaybreakBlue)),
            (
                Some("future-catalog-tier"),
                Some(CyberAccessProgram::DaybreakRed),
            ),
        ] {
            let dialect = Dialect {
                service_tier: tier.map(|tier| tier.parse().unwrap()),
                cyber_access_program: cyber,
                ..Dialect::stateless()
            };
            let body = encode(&req, &dialect).unwrap();
            assert_eq!(
                body.get("service_tier"),
                tier.map(|tier| json!(tier)).as_ref()
            );
            assert_eq!(
                body.get("access_programs"),
                cyber.map(|cyber| json!({"cyber": cyber})).as_ref()
            );
            if tier.is_some() {
                let routing = body.keys().position(|key| key == "service_tier").unwrap();
                let input = body.keys().position(|key| key == "input").unwrap();
                assert!(routing < input);
            }
        }
    }

    #[test]
    fn summary_and_effort_are_placed_by_the_dialect() {
        let mut req = request("gpt-5");
        for effort in [None, Some("high".into())] {
            let expected = effort.as_ref().map(|e| json!(e));
            req.reasoning = effort;
            let body = encode(&req, &Dialect::stateless()).unwrap();
            assert_eq!(body["reasoning"]["summary"], "auto");
            assert_eq!(body["reasoning"].get("effort"), expected.as_ref());
            assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        }
        req.reasoning = Some("low".into());
        let silent = Dialect {
            reasoning_summary: None,
            ..Dialect::stateless()
        };
        let body = encode(&req, &silent).unwrap();
        assert_eq!(body["reasoning"], json!({"effort":"low"}));
        req.reasoning = Some("adaptive".into());
        assert!(encode(&req, &silent).is_err());
    }

    #[test]
    fn empty_history_is_explicit_and_http_envelope_is_unchanged() {
        let mut req = request("gpt-5");
        req.context.clear();
        req.max_output_tokens = None;
        let encoded = crate::provider::codec::Codec::Responses(Dialect::stateless())
            .encode(&req, &"session".parse().unwrap())
            .unwrap();
        assert!(encoded.headers.is_empty());
        assert_eq!(
            encoded.body,
            json!({
                "model":"gpt-5", "input":[], "stream":true, "store":false,
                "include":["reasoning.encrypted_content"],
                "reasoning":{"summary":"auto"}, "prompt_cache_key":"session"
            })
        );
    }

    #[test]
    fn request_transmits_native_tools_schema_and_instructions() {
        let mut req = request("gpt-5");
        req.system = vec![SystemSegment {
            text: "system".into(),
            cache: false,
        }];
        req.tools = vec![ToolDefinition {
            name: "search".into(),
            description: "Search".into(),
            input_schema: json!({"type":"object"}),
        }];
        let schema = json!({"type":"object", "properties":{}, "additionalProperties":false});
        req.response_schema = Some(ResponseSchema {
            name: "answer".into(),
            schema: schema.clone(),
        });
        req.reasoning = Some("high".into());
        let body = encode(&req, &Dialect::stateless()).unwrap();
        assert_eq!(body["instructions"], "system");
        assert_eq!(body["tools"][0]["name"], "search");
        assert_eq!(body["text"]["format"]["schema"], schema);
        assert_eq!(
            body["reasoning"],
            json!({"effort":"high", "summary":"auto"})
        );
    }

    #[test]
    fn explicit_cache_mode_is_opt_in_even_without_checkpoints() {
        use crate::provider::protocol::HistoryLifetime;
        let mut req = request("gpt-5");
        req.context.clear();
        req.system.push(SystemSegment {
            text: "instructions".into(),
            cache: true,
        });
        assert!(
            !encode(&req, &Dialect::stateless())
                .unwrap()
                .contains_key("prompt_cache_options")
        );
        let dialect = Dialect {
            cache: Cache::Explicit,
            ..Dialect::stateless()
        };
        for lifetime in [HistoryLifetime::Extends, HistoryLifetime::Detached] {
            req.history_lifetime = lifetime;
            let body = encode(&req, &dialect).unwrap();
            assert_eq!(body["prompt_cache_options"], json!({"mode":"explicit"}));
            assert_eq!(body["instructions"], "instructions");
            assert_eq!(body["input"], json!([]));
        }
    }
}
