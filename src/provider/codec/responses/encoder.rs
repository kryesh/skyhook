//! Encode request options and the input array as a native Responses body.
use super::*;
use crate::provider::{
    codec::{
        Cache, CacheControl, CodecName,
        common::{PreparedImages, check_tool, system_text},
        placement::breakpoint,
        set_each,
    },
    protocol::{HistoryLifetime, ModelRequest},
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
    "access_programs",
    "cache_control",
    "instructions",
    "tools",
    "text",
    "prompt_cache_options",
];

pub(crate) fn encode(
    request: &ModelRequest,
    images: PreparedImages<'_>,
    dialect: &Dialect,
) -> Result<Map<String, Value>, ProviderError> {
    let input = super::input::encode(request, images, dialect)?;
    let mut settings = serde_json::Map::from_iter([
        ("model".into(), json!(request.model)),
        ("stream".into(), json!(true)),
        ("input".into(), Value::Array(input)),
        ("store".into(), json!(false)),
        ("include".into(), json!(["reasoning.encrypted_content"])),
    ]);
    if let Some(options) = dialect.cache.options() {
        settings.insert("prompt_cache_options".into(), options);
    }
    // The server places this breakpoint on the last cacheable block itself, so it is
    // sent only for automatic caching, and a detached history is never reused.
    if let (CacheControl::Sent { ttl }, Cache::Automatic) = (dialect.cache_control, dialect.cache)
        && request.history_lifetime == HistoryLifetime::Extends
    {
        settings.insert("cache_control".into(), breakpoint(ttl));
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
        let mut format = Map::from_iter([("type".into(), json!("json_schema"))]);
        format.extend(dialect.schema.format(schema));
        settings.insert("text".into(), json!({"format": format}));
    }
    if let Some(reasoning) = &request.reasoning {
        CodecName::Responses.place_effort(&dialect.effort, &mut settings, reasoning)?;
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
        codec::{CacheTtl, common::tests::request},
        protocol::{ResponseSchema, SystemSegment, ToolDefinition},
    };

    #[test]
    fn access_programs_are_optional() {
        let req = request("gpt-6-astra");
        for cyber in [
            None,
            Some(CyberAccessProgram::Standard),
            Some(CyberAccessProgram::DaybreakRed),
        ] {
            let dialect = Dialect {
                cyber_access_program: cyber,
                ..Dialect::stateless()
            };
            let body = encode(&req, Default::default(), &dialect).unwrap();
            assert_eq!(
                body.get("access_programs"),
                cyber.map(|cyber| json!({"cyber": cyber})).as_ref()
            );
        }
    }

    #[test]
    fn summary_and_effort_are_placed_by_the_dialect() {
        let mut req = request("gpt-5");
        let summarized = Dialect {
            reasoning_summary: Some(ReasoningSummary::Auto),
            ..Dialect::stateless()
        };
        for effort in [None, Some("high".into())] {
            let expected = effort.as_ref().map(|e| json!(e));
            req.reasoning = effort;
            let body = encode(&req, Default::default(), &summarized).unwrap();
            assert_eq!(body["reasoning"]["summary"], "auto");
            assert_eq!(body["reasoning"].get("effort"), expected.as_ref());
            assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        }
        req.reasoning = Some("low".into());
        let silent = Dialect::stateless();
        let body = encode(&req, Default::default(), &silent).unwrap();
        assert_eq!(body["reasoning"], json!({"effort":"low"}));
        req.reasoning = Some("adaptive".into());
        assert!(encode(&req, Default::default(), &silent).is_err());
    }

    #[test]
    fn empty_history_is_explicit_and_http_envelope_is_unchanged() {
        let mut req = request("gpt-5");
        req.context.clear();
        req.max_output_tokens = None;
        let encoded = crate::provider::codec::Codec::Responses(Dialect::stateless())
            .encode(&req, Default::default(), &"session".parse().unwrap())
            .unwrap();
        assert!(encoded.headers.is_empty());
        assert_eq!(
            Value::Object(encoded.body),
            json!({
                "model":"gpt-5", "input":[], "stream":true, "store":false,
                "include":["reasoning.encrypted_content"], "prompt_cache_key":"session"
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
        req.response_schema = Some(ResponseSchema::new("answer", schema.clone()).unwrap());
        req.reasoning = Some("high".into());
        let body = encode(&req, Default::default(), &Dialect::stateless()).unwrap();
        assert_eq!(body["instructions"], "system");
        assert_eq!(body["tools"][0]["name"], "search");
        let format = json!({"type":"json_schema", "name":"answer", "schema":schema});
        assert_eq!(body["text"]["format"], format);
        assert_eq!(body["reasoning"], json!({"effort":"high"}));
        // A strict dialect flags the schema and sends it as written, keywords and all.
        let strict = Dialect {
            schema: crate::provider::codec::SchemaConstraint::Strict,
            ..Dialect::stateless()
        };
        let schema = json!({"type":"object", "properties":{"n":{"type":"integer", "minimum":1}}});
        req.response_schema = Some(ResponseSchema::new("answer", schema.clone()).unwrap());
        let format = json!({"type":"json_schema", "name":"answer", "strict":true, "schema":schema});
        assert_eq!(
            encode(&req, Default::default(), &strict).unwrap()["text"]["format"],
            format
        );
    }

    #[test]
    fn cache_mode_and_lifetime_select_the_request_wide_cache_fields() {
        let mut req = request("gpt-5");
        req.context.clear();
        req.system.push(SystemSegment {
            text: "instructions".into(),
            cache: true,
        });
        assert!(
            !encode(&req, Default::default(), &Dialect::stateless())
                .unwrap()
                .contains_key("prompt_cache_options")
        );
        let cache_control = CacheControl::Sent {
            ttl: Some(CacheTtl::OneHour),
        };
        let automatic = Dialect {
            cache_control,
            ..Dialect::stateless()
        };
        let explicit = Dialect {
            cache: Cache::Explicit,
            cache_control,
            ..Dialect::stateless()
        };
        for lifetime in [HistoryLifetime::Extends, HistoryLifetime::Detached] {
            req.history_lifetime = lifetime;
            let body = encode(&req, Default::default(), &explicit).unwrap();
            assert_eq!(body["prompt_cache_options"], json!({"mode":"explicit"}));
            assert_eq!(body["instructions"], "instructions");
            assert_eq!(body["input"], json!([]));
            assert!(!body.contains_key("cache_control"));
            let control = (lifetime == HistoryLifetime::Extends)
                .then(|| json!({"type":"ephemeral", "ttl":"1h"}));
            assert_eq!(
                encode(&req, Default::default(), &automatic)
                    .unwrap()
                    .get("cache_control"),
                control.as_ref()
            );
        }
    }
}
