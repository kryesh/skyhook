//! Only schema-annotated fields may be shortened in automatic result presentation.
use super::*;

pub(super) const FIELD_BYTES: usize = 2 * 1024;
pub(super) const FIELD_LINES: usize = 100;
const ANNOTATION: &str = "x-skyhook-truncatable";

pub(super) fn project(
    directory: &Path,
    cursor: Cursor,
    schema: &Value,
    cancellation: &super::super::CancellationToken,
) -> Result<serde_json::Map<String, Value>, ToolError> {
    let mut document: Value = serde_json::from_reader(BufReader::new(std::fs::File::open(
        directory.join("document.json"),
    )?))?;
    let mut projection = Projection {
        directory,
        cursor,
        stored_fields: fields(directory)?,
        truncated: Vec::new(),
        cancellation,
    };
    projection.visit(&mut document["result"], "/result", &[schema], schema)?;
    // Console is a shared output field with the same explicit annotation as tool fields.
    let console_schema = json!({"type":"string", ANNOTATION:true});
    projection.visit(
        &mut document["console"],
        "/console",
        &[&console_schema],
        &console_schema,
    )?;
    let mut output = serde_json::Map::new();
    output.insert("result".into(), document["result"].take());
    output.insert(
        "capture_complete".into(),
        document["capture_complete"].take(),
    );
    if document["console"].as_str().is_some_and(|s| !s.is_empty())
        || projection
            .truncated
            .iter()
            .any(|t| t["field"] == "/console")
    {
        output.insert("console".into(), document["console"].take());
    }
    if !projection.truncated.is_empty() {
        output.insert("truncated".into(), Value::Array(projection.truncated));
    }
    Ok(output)
}

struct Projection<'a> {
    directory: &'a Path,
    cursor: Cursor,
    stored_fields: Vec<String>,
    truncated: Vec<Value>,
    cancellation: &'a super::super::CancellationToken,
}

impl Projection<'_> {
    fn visit(
        &mut self,
        value: &mut Value,
        field: &str,
        schemas: &[&Value],
        root: &Value,
    ) -> Result<(), ToolError> {
        if self.cancellation.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let mut applicable = Vec::new();
        for schema in schemas {
            expand(schema, root, value, &mut applicable, &mut Vec::new());
        }
        if applicable.iter().any(|s| s[ANNOTATION] == true)
            && (value.is_string() || value.is_array())
        {
            let path = ensure_field_file(self.directory, field, self.cancellation)?;
            let mut bytes = Vec::new();
            std::fs::File::open(&path)?
                .take((FIELD_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
            let (prefix, end, shortened) = if value.is_string() {
                string_prefix(&bytes)?
            } else {
                array_prefix(&bytes)?
            };
            *value = prefix;
            if shortened {
                let mut cursor = self.cursor.clone();
                cursor.field = field.into();
                cursor.byte = end as u64;
                cursor.line = 1 + bytes[..end].iter().filter(|&&b| b == b'\n').count();
                cursor.column = bytes[..end]
                    .iter()
                    .rposition(|&b| b == b'\n')
                    .map_or(end, |last| end - last - 1);
                self.truncated
                    .push(json!({"field":field,"next":cursor.encode(self.directory)?}));
            }
            return Ok(());
        }
        // Storage offloading does not grant permission to truncate a field.
        if self.stored_fields.iter().any(|stored| stored == field) {
            let path = field_file(self.directory, field);
            *value = if value.is_string() {
                Value::String(std::fs::read_to_string(path)?)
            } else {
                serde_json::from_reader(BufReader::new(std::fs::File::open(path)?))?
            };
        }
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let children = applicable
                        .iter()
                        .filter_map(|s| s.get("properties")?.get(key))
                        .collect::<Vec<_>>();
                    self.visit(
                        child,
                        &format!("{field}/{}", key.replace('~', "~0").replace('/', "~1")),
                        &children,
                        root,
                    )?;
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter_mut().enumerate() {
                    let children = applicable
                        .iter()
                        .filter_map(|s| {
                            s.get("prefixItems")
                                .and_then(|items| items.get(index))
                                .or_else(|| s.get("items"))
                        })
                        .collect::<Vec<_>>();
                    self.visit(child, &format!("{field}/{index}"), &children, root)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn line_end(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .enumerate()
        .filter(|(_, b)| **b == b'\n')
        .nth(FIELD_LINES - 1)
        .map_or(bytes.len(), |(index, _)| index + 1)
}

fn string_prefix(bytes: &[u8]) -> Result<(Value, usize, bool), ToolError> {
    let mut end = FIELD_BYTES.min(line_end(bytes));
    match std::str::from_utf8(&bytes[..end]) {
        Ok(_) => {}
        Err(error) if error.error_len().is_none() => end = error.valid_up_to(),
        Err(_) => return Err(ToolError::Failed("saved output is not UTF-8".into())),
    }
    let text = std::str::from_utf8(&bytes[..end]).expect("validated UTF-8");
    Ok((Value::String(text.into()), end, end < bytes.len()))
}

// Preserve whole array items. The saved JSON text determines the line/byte limits and
// the continuation offset; parsing never needs to load an oversized item.
fn array_prefix(bytes: &[u8]) -> Result<(Value, usize, bool), ToolError> {
    let maximum = FIELD_BYTES.min(line_end(bytes));
    let mut position = 1; // opening '['
    let mut items = Vec::new();
    loop {
        while bytes
            .get(position)
            .is_some_and(|b| b.is_ascii_whitespace() || *b == b',')
        {
            position += 1;
        }
        if bytes.get(position) == Some(&b']') {
            return Ok((Value::Array(items), position + 1, false));
        }
        if position >= maximum {
            return Ok((Value::Array(items), position.min(bytes.len()), true));
        }
        let mut stream =
            serde_json::Deserializer::from_slice(&bytes[position..maximum]).into_iter::<Value>();
        match stream.next() {
            Some(Ok(item)) if position + stream.byte_offset() < maximum => {
                position += stream.byte_offset();
                items.push(item);
            }
            _ => return Ok((Value::Array(items), position, true)),
        }
    }
}

// Follow schema references and applicable enum/union branches without making a
// blanket rule for similarly named fields in other variants.
fn expand<'a>(
    schema: &'a Value,
    root: &'a Value,
    value: &Value,
    out: &mut Vec<&'a Value>,
    refs: &mut Vec<&'a str>,
) {
    // A partial result may omit required siblings. Their absence must not disable
    // annotations on fields that were successfully captured.
    out.push(schema);
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str)
        && !refs.contains(&reference)
        && let Some(target) = reference.strip_prefix('#').and_then(|p| root.pointer(p))
    {
        refs.push(reference);
        expand(target, root, value, out, refs);
        refs.pop();
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(branches) = schema.get(key).and_then(Value::as_array) {
            for branch in branches {
                if key == "allOf" || matches(branch, root, value, &mut Vec::new()) {
                    expand(branch, root, value, out, refs);
                }
            }
        }
    }
}

fn matches<'a>(schema: &'a Value, root: &'a Value, value: &Value, refs: &mut Vec<&'a str>) -> bool {
    if schema == &Value::Bool(false) {
        return false;
    }
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str)
        && !refs.contains(&reference)
        && let Some(target) = reference.strip_prefix('#').and_then(|p| root.pointer(p))
    {
        refs.push(reference);
        let applies = matches(target, root, value, refs);
        refs.pop();
        if !applies {
            return false;
        }
    }
    if let Some(constant) = schema.get("const")
        && constant != value
    {
        return false;
    }
    if let Some(variants) = schema.get("enum").and_then(Value::as_array)
        && !variants.contains(value)
    {
        return false;
    }
    if let Some(kind) = schema.get("type").and_then(Value::as_str) {
        let valid = match kind {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "null" => value.is_null(),
            "boolean" => value.is_boolean(),
            "number" | "integer" => value.is_number(),
            _ => true,
        };
        if !valid {
            return false;
        }
    }
    if let Some(required) = schema.get("required").and_then(Value::as_array)
        && required
            .iter()
            .filter_map(Value::as_str)
            .any(|key| value.get(key).is_none())
    {
        return false;
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (key, property) in properties {
            if let Some(child) = value.get(key)
                && !matches(property, root, child, refs)
            {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        identity::AgentId,
        job::{JobOutcome, JobSpec},
        session::SessionStore,
        tool::ToolOutput,
    };

    async fn fixture(
        value: Value,
        schema: Value,
        error: Option<String>,
    ) -> (tempfile::TempDir, JobManager, JobId) {
        let root = tempfile::tempdir().unwrap();
        let store = SessionStore::create(root.path()).await.unwrap();
        let manager = JobManager::new(store.clone());
        let mut spec = JobSpec::test(AgentId::root(store.id()), "annotated");
        spec.output_schema = Some(schema);
        let id = manager.create(spec).await.unwrap().id;
        let mut output = ToolOutput::new(value);
        output.console_output = "console\n".repeat(150);
        let outcome = if let Some(message) = error {
            JobOutcome::Failed {
                message,
                output: Some(output),
                denial: None,
            }
        } else {
            JobOutcome::Completed(output)
        };
        manager.finish(id, outcome).await.unwrap();
        (root, manager, id)
    }

    #[tokio::test]
    async fn limits_are_per_field_and_unannotated_data_and_errors_survive_resume() {
        let value = json!({"content":"line\n".repeat(150), "stdout":"é\\\"".repeat(2000),
            "stderr":"e".repeat(10000), "metadata":"m".repeat(92000), "extra":["z".repeat(10000)], "exit_code":7});
        let schema = json!({"type":"object","properties":{
            "content":{ANNOTATION:true},"stdout":{ANNOTATION:true},"stderr":{ANNOTATION:true}
        }});
        let error = "failure details ".repeat(2000);
        let (root, manager, id) = fixture(value.clone(), schema, Some(error.clone())).await;
        let view = manager
            .present_output(OutputArgs::new(id), &Default::default())
            .await
            .unwrap();
        assert_eq!(view["result"]["content"], "line\n".repeat(100));
        assert_eq!(
            view["result"]["stdout"].as_str().unwrap().len(),
            FIELD_BYTES
        );
        assert_eq!(
            view["result"]["stderr"].as_str().unwrap().len(),
            FIELD_BYTES
        );
        assert_eq!(view["result"]["metadata"], value["metadata"]);
        assert_eq!(view["result"]["extra"], value["extra"]);
        assert_eq!(view["result"]["exit_code"], 7);
        assert_eq!(view["error"], error);
        assert_eq!(
            manager.metadata(id).await.unwrap().error.as_deref(),
            Some(error.as_str())
        );
        assert_eq!(view["console"], "console\n".repeat(100));
        assert_eq!(view["truncated"].as_array().unwrap().len(), 4);
        assert!(view.get("preview").is_none());
        assert_eq!(manager.snapshot(id).await.unwrap().output.unwrap(), value);
        let session = manager.store().id();
        manager.store().close().await.unwrap();
        let (store, records) = SessionStore::open(root.path(), session).await.unwrap();
        let restored = JobManager::restore(store, &records).await.unwrap();
        assert_eq!(
            restored
                .present_output(OutputArgs::new(id), &Default::default())
                .await
                .unwrap(),
            view
        );
        for (field, expected, line, offset) in [
            ("/result/content", "line", 101, 0),
            ("/result/stderr", "e", 1, FIELD_BYTES),
        ] {
            let entry = view["truncated"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["field"] == field)
                .unwrap();
            let mut args = OutputArgs::new(id);
            args.cursor = Some(entry["next"].as_str().unwrap().into());
            let page = restored
                .present_output(args, &Default::default())
                .await
                .unwrap();
            let first = &page["preview"]["lines"][0];
            assert_eq!(first["line"], line);
            assert_eq!(first["offset"], offset);
            assert!(first["text"].as_str().unwrap().starts_with(expected));
        }
    }

    #[tokio::test]
    async fn union_and_reference_annotations_do_not_affect_other_variants() {
        let schema = json!({"$defs":{"text":{"type":"string",ANNOTATION:true}},"oneOf":[
            {"properties":{"kind":{"const":"short"},"content":{"$ref":"#/$defs/text"}},"required":["kind","content"]},
            {"properties":{"kind":{"const":"full"},"content":{"type":"string"}},"required":["kind","content"]}
        ]});
        for kind in ["short", "full"] {
            let (_root, manager, id) = fixture(
                json!({"kind":kind,"content":"x".repeat(10000)}),
                schema.clone(),
                None,
            )
            .await;
            let view = manager
                .present_output(OutputArgs::new(id), &Default::default())
                .await
                .unwrap();
            assert_eq!(view["result"]["kind"], kind);
            assert_eq!(
                view["result"]["content"].as_str().unwrap().len(),
                if kind == "short" { FIELD_BYTES } else { 10000 }
            );
        }
    }

    #[test]
    fn string_boundaries_preserve_utf8_newlines_and_exact_limits() {
        for text in [
            "x".repeat(FIELD_BYTES),
            "a\r\n".repeat(FIELD_LINES),
            String::new(),
        ] {
            let (prefix, _, truncated) = string_prefix(text.as_bytes()).unwrap();
            assert_eq!(prefix, text);
            assert!(!truncated);
        }
        let text = "€".repeat(1000);
        let (prefix, end, truncated) = string_prefix(&text.as_bytes()[..FIELD_BYTES + 1]).unwrap();
        assert_eq!(end, 2046);
        assert_eq!(prefix, text[..end]);
        assert!(truncated);
    }

    #[tokio::test]
    async fn arrays_keep_whole_items_and_continue_at_the_next_item() {
        let items = (0..200)
            .map(|n| json!({"n":n,"text":"abc"}))
            .collect::<Vec<_>>();
        let schema = json!({"properties":{"items":{ANNOTATION:true}}});
        let (_root, manager, id) =
            fixture(json!({"items":items,"count":200}), schema.clone(), None).await;
        let view = manager
            .present_output(OutputArgs::new(id), &Default::default())
            .await
            .unwrap();
        let prefix = view["result"]["items"].as_array().unwrap();
        assert!(!prefix.is_empty() && prefix.len() < items.len());
        assert_eq!(prefix, &items[..prefix.len()]);
        assert_eq!(view["result"]["count"], 200);
        assert!(serde_json::to_vec(prefix).unwrap().len() <= FIELD_BYTES);
        let entry = view["truncated"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["field"] == "/result/items")
            .unwrap();
        let mut args = OutputArgs::new(id);
        args.cursor = Some(entry["next"].as_str().unwrap().into());
        let page = manager
            .present_output(args, &Default::default())
            .await
            .unwrap();
        let text = page["preview"]["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["text"].as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(&format!("\"n\": {}", prefix.len())));
        assert_eq!(
            manager.snapshot(id).await.unwrap().output.unwrap()["items"],
            json!(items)
        );

        let (_root, manager, id) =
            fixture(json!({"items":["x".repeat(100000)]}), schema, None).await;
        let view = manager
            .present_output(OutputArgs::new(id), &Default::default())
            .await
            .unwrap();
        assert_eq!(view["result"]["items"], json!([]));
        assert_eq!(view["truncated"][0]["field"], "/result/items");
    }
}
