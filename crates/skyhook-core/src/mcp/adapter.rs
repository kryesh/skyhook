//! Adapt the startup MCP catalog to the ordinary, policy-gated tool registry.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use base64::Engine as _;
use rmcp::model::CallToolResult;
use serde_json::{Map, Value, json};

use crate::{
    media::{MAX_IMAGE_BYTES, MAX_IMAGE_BYTES_PER_SUBMISSION, MAX_IMAGES_PER_SUBMISSION},
    session::SessionStore,
    tool::{
        ToolError, ToolOptions, ToolOutput, ToolPlacement, ToolRegistryBuilder, policy::ResourceId,
    },
};

use super::{
    config::McpServerConfig,
    manager::{DiscoveredTool, McpError, McpManager},
};

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

/// Prefer readable names. Allocate against the whole catalog before registration
/// so ambiguous server/tool boundaries never depend on discovery order. Natural
/// names take precedence over generated suffixes, even if discovered later.
fn tool_names(catalog: &[DiscoveredTool], is_registered: impl Fn(&str) -> bool) -> Vec<String> {
    let raw: Vec<_> = catalog
        .iter()
        .map(|item| format!("mcp_{}_{}", item.server, item.tool.name))
        .collect();
    let safe = |name: &str| {
        name.len() <= 64
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    };
    let mut counts = BTreeMap::new();
    for name in &raw {
        *counts.entry(name.as_str()).or_insert(0_usize) += 1;
    }
    let reserved: BTreeSet<_> = raw
        .iter()
        .filter(|name| safe(name))
        .map(String::as_str)
        .collect();
    let mut order: Vec<_> = (0..catalog.len()).collect();
    order.sort_by(|&left, &right| {
        (&catalog[left].server, &catalog[left].tool.name)
            .cmp(&(&catalog[right].server, &catalog[right].tool.name))
    });
    let mut names = vec![String::new(); catalog.len()];
    let mut used = BTreeSet::new();
    for index in order {
        let name = &raw[index];
        if safe(name) && counts[name.as_str()] == 1 && !is_registered(name) {
            names[index] = name.clone();
            used.insert(name.clone());
            continue;
        }
        let item = &catalog[index];
        let server = &item.server;
        let tool = item.tool.name.as_ref();
        // Length-delimited originals distinguish identities such as a_b/c and
        // a/b_c, as well as names that normalize to the same ASCII spelling.
        let hash = crate::sha256_hex(format!("{}:{server}{}:{tool}", server.len(), tool.len()));
        let readable: String = name
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                    character
                } else {
                    '_'
                }
            })
            .collect();
        for attempt in 0_usize.. {
            let suffix = if attempt == 0 {
                hash[..8].to_owned()
            } else {
                format!("{}_{attempt}", &hash[..8])
            };
            let prefix = &readable[..readable.len().min(64 - 1 - suffix.len())];
            let candidate = format!("{prefix}_{suffix}");
            if !reserved.contains(candidate.as_str())
                && !used.contains(&candidate)
                && !is_registered(&candidate)
            {
                used.insert(candidate.clone());
                names[index] = candidate;
                break;
            }
        }
    }
    names
}

struct NoExternalSchemas;

impl jsonschema::Retrieve for NoExternalSchemas {
    fn retrieve(
        &self,
        _uri: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("external MCP schema references are not permitted".into())
    }
}

struct Arguments {
    schema: Value,
    validator: jsonschema::Validator,
    wrapped: bool,
}

impl Arguments {
    fn new(original: Value) -> Result<Self, String> {
        if original["type"] != "object" {
            return Err("input schema root type must be object".to_owned());
        }
        // Never load server-supplied references from the host filesystem or
        // network, even if feature unification later enables a default resolver.
        let validator = jsonschema::options()
            .with_retriever(NoExternalSchemas)
            .build(&original)
            .map_err(|error| error.to_string())?;
        // The registry consumes bg before validation. Unless the root explicitly
        // excludes it, preserve the entire upstream input under an envelope.
        // Pattern schemas also need wrapping because the registry's basic
        // unknown-property check only understands named properties.
        let wrapped = original["additionalProperties"] != false
            || original.get("$ref").is_some()
            || original["properties"].get("bg").is_some()
            || original.get("patternProperties").is_some()
            // These root constraints may count, reject, or otherwise interpret
            // the synthetic bg property. Keep them inside the envelope too.
            || [
                "allOf", "anyOf", "oneOf", "not", "if", "then", "else",
                "dependencies", "dependentSchemas", "propertyNames",
                "minProperties", "maxProperties", "unevaluatedProperties",
                "$dynamicRef", "$recursiveRef", "enum", "const",
            ]
            .iter()
            .any(|keyword| original.get(*keyword).is_some())
            || original["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|name| name == "bg"));
        let draft = jsonschema::Draft::default().detect(&original);
        let schema = if wrapped {
            let mut embedded = if original.get("$ref").is_some()
                && matches!(
                    draft,
                    jsonschema::Draft::Draft4
                        | jsonschema::Draft::Draft6
                        | jsonschema::Draft::Draft7
                ) {
                // Older drafts ignore every $ref sibling, including an injected
                // resource ID. Normalize the root alias before giving it scope.
                let canonical = jsonschema::canonical::options()
                    .with_retriever(NoExternalSchemas)
                    .canonicalize(&original)
                    .map_err(|error| error.to_string())?;
                if canonical.kind() == jsonschema::canonical::CanonicalKind::Raw {
                    return Err("legacy root-reference schema cannot be safely embedded".to_owned());
                }
                let mut schema = canonical.to_json_schema();
                if let Some(object) = schema.as_object_mut()
                    && let Some(reference) = object.remove("$ref")
                {
                    // The normalized document contains only the effective alias
                    // plus its definitions, not the ignored assertion siblings.
                    object.insert("allOf".to_owned(), json!([{"$ref": reference}]));
                }
                schema
            } else {
                original.clone()
            };
            if embedded.is_boolean() {
                embedded = json!({"allOf": [embedded]});
            }
            // A subschema's local fragment references must still refer to that
            // schema, not to our new envelope. A resource ID creates that scope.
            let id_keyword = if draft == jsonschema::Draft::Draft4 {
                "id"
            } else {
                "$id"
            };
            if embedded.get(id_keyword).is_none() {
                let hash = crate::sha256_hex(original.to_string().as_bytes());
                embedded[id_keyword] = Value::String(format!("urn:skyhook:mcp-schema:{hash}"));
            }
            let mut envelope = json!({
                "type": "object",
                "properties": {"arguments": embedded},
                "required": ["arguments"],
                "additionalProperties": false,
            });
            // Dialects are document-scoped in older drafts. Keep their keyword
            // semantics (including draft 4's `id`) after introducing a root.
            if let Some(dialect) = original.get("$schema") {
                envelope["$schema"] = dialect.clone();
            }
            // Do not publish an envelope with dangling references even when
            // the original document was valid in its unwrapped scope.
            jsonschema::options()
                .with_retriever(NoExternalSchemas)
                .build(&envelope)
                .map_err(|error| format!("cannot embed input schema: {error}"))?;
            envelope
        } else {
            original
        };
        Ok(Self {
            schema,
            validator,
            wrapped,
        })
    }

    fn extract<'a>(&self, value: &'a Value) -> Result<&'a Map<String, Value>, ToolError> {
        let object = value.as_object().ok_or(ToolError::ArgumentsMustBeObject)?;
        if self.wrapped {
            if let Some(name) = object.keys().find(|name| name.as_str() != "arguments") {
                return Err(ToolError::InvalidArguments(format!(
                    "unknown argument `{name}`"
                )));
            }
            object
                .get("arguments")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    ToolError::InvalidArguments("`arguments` must be an object".to_owned())
                })
        } else {
            Ok(object)
        }
    }

    fn validate(&self, value: &Value) -> Result<(), ToolError> {
        let object = self.extract(value)?;
        self.validator
            .validate(&Value::Object(object.clone()))
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))
    }
}

fn map_error(error: McpError) -> ToolError {
    match error {
        McpError::Cancelled => ToolError::Cancelled,
        other => ToolError::Failed(other.to_string()),
    }
}

/// Keep the MCP result envelope (including structured content, error flag and
/// unknown resource/audio content). Image payloads use normal session blobs and
/// attachments instead of leaking large base64 strings into textual output.
async fn map_result(result: CallToolResult, store: &SessionStore) -> Result<ToolOutput, ToolError> {
    let is_error = result.is_error == Some(true);
    let mut value = serde_json::to_value(&result)?;
    let mut images = Vec::new();
    let mut total_bytes = 0_u64;
    // Strip every payload before importing any of them. A failed import must
    // not return the rejected (possibly oversized) image or later images as
    // base64 text through FailedWithOutput.
    for (index, block) in result.content.iter().enumerate() {
        if block.as_image().is_some()
            && let Some(block) = value["content"][index].as_object_mut()
        {
            block.remove("data");
            block.insert("imageError".to_owned(), json!("image not imported"));
        }
    }
    for (index, block) in result.content.iter().enumerate() {
        let Some(image) = block.as_image() else {
            continue;
        };
        let reference = match import_image(image, index, images.len(), total_bytes, store).await {
            Ok(reference) => reference,
            Err(error) => {
                value["content"][index]["imageError"] = json!(error.to_string());
                return Err(ToolError::with_output(
                    error.to_string(),
                    ToolOutput::new(value).with_images(images),
                ));
            }
        };
        total_bytes += reference.bytes;
        if let Some(block) = value["content"][index].as_object_mut() {
            block.remove("imageError");
            block.insert("image".to_owned(), serde_json::to_value(&reference)?);
        }
        images.push(reference);
    }
    let output = ToolOutput::new(value).with_images(images);
    if is_error {
        Err(ToolError::with_output("MCP tool reported an error", output))
    } else {
        Ok(output)
    }
}

async fn import_image(
    image: &rmcp::model::ImageContent,
    index: usize,
    image_count: usize,
    total_bytes: u64,
    store: &SessionStore,
) -> Result<crate::media::ImageReference, ToolError> {
    let limit = usize::try_from(MAX_IMAGE_BYTES).expect("image limit fits usize");
    if image.data.len() > limit.div_ceil(3) * 4 {
        return Err(ToolError::Failed(
            "MCP image exceeds the image byte limit".to_owned(),
        ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&image.data)
        .map_err(|error| ToolError::Failed(format!("invalid MCP image encoding: {error}")))?;
    let total_bytes = total_bytes + bytes.len() as u64;
    if bytes.len() > limit
        || image_count >= MAX_IMAGES_PER_SUBMISSION
        || total_bytes > MAX_IMAGE_BYTES_PER_SUBMISSION
    {
        return Err(ToolError::Failed(
            "MCP images exceed attachment limits".to_owned(),
        ));
    }
    let extension = match image.mime_type.as_str() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => {
            return Err(ToolError::Failed(format!(
                "unsupported MCP image type: {}",
                image.mime_type
            )));
        }
    };
    store
        .import_blob(
            &bytes,
            format!("mcp-image-{index}.{extension}"),
            image.mime_type.clone(),
        )
        .await
        .map_err(|error| ToolError::Failed(error.to_string()))
}

#[cfg(test)]
#[path = "adapter_tests.rs"]
mod tests;
