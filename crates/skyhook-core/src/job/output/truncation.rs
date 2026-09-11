//! Only schema-annotated fields may be shortened in automatic result presentation.
use super::*;

pub(super) const FIELD_BYTES: usize = 2 * 1024;
pub(super) const FIELD_LINES: usize = 100;
const ANNOTATION: &str = "x-skyhook-truncatable";

pub(super) fn project(
    directory: &Path,
    schema: &Value,
    cancellation: &super::super::CancellationToken,
    annotated: &BTreeSet<String>,
    replacements: &BTreeMap<String, Value>,
) -> Result<serde_json::Map<String, Value>, ToolError> {
    let mut document: Value = serde_json::from_reader(BufReader::new(std::fs::File::open(
        directory.join("document.json"),
    )?))?;
    let mut projection = Projection {
        directory,
        stored_fields: fields(directory)?,
        truncated: Vec::new(),
        cancellation,
        annotated,
        replacements,
    };
    projection.visit(&mut document["result"], "/result", &[schema], schema)?;
    let mut output = serde_json::Map::new();
    output.insert("result".into(), document["result"].take());
    capture_notice(&mut output, document["capture_complete"].as_bool());
    if !projection.truncated.is_empty() {
        output.insert("truncated".into(), Value::Array(projection.truncated));
    }
    Ok(output)
}

struct Projection<'a> {
    directory: &'a Path,
    stored_fields: Vec<String>,
    truncated: Vec<Value>,
    cancellation: &'a super::super::CancellationToken,
    annotated: &'a BTreeSet<String>,
    replacements: &'a BTreeMap<String, Value>,
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
        if let Some(replacement) = self.replacements.get(field) {
            *value = replacement.clone();
            return Ok(());
        }
        let applicable = ApplicableSchemas::new(schemas, root, value);
        if (self.annotated.contains(field) || applicable.is_annotated())
            && (value.is_string() || value.is_array() || value.is_object())
        {
            let path = materialize_field(self.directory, field, value, self.cancellation)?;
            let mut bytes = Vec::new();
            std::fs::File::open(&path)?
                .take((FIELD_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
            let (prefix, end, shortened) = if value.is_string() {
                string_prefix(&bytes)?
            } else if value.is_array() {
                array_prefix(&bytes)?
            } else {
                object_prefix(&bytes)?
            };
            *value = prefix;
            if shortened {
                let index = reader::LineIndex::load(&path, self.cancellation)?;
                let line = 1 + bytes[..end].iter().filter(|&&b| b == b'\n').count();
                let offset = bytes[..end]
                    .iter()
                    .rposition(|&b| b == b'\n')
                    .map_or(end, |last| end - last - 1);
                let mut marker =
                    json!({"field":field,"total_lines":index.total_lines(),"next_start":line});
                if offset != 0 {
                    marker["next_offset"] = json!(offset);
                }
                self.truncated.push(marker);
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
                    let children = applicable.property(key);
                    self.visit(child, &property_field(field, key), &children, root)?;
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter_mut().enumerate() {
                    let children = applicable.item(index);
                    self.visit(child, &format!("{field}/{index}"), &children, root)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Resolve the same annotations used by native projection, before crossing the JS
/// bridge. Keep descendant annotations as well, since scripts can extract them.
pub(crate) fn annotated_fields(value: &Value, schema: &Value) -> BTreeSet<String> {
    fn visit(
        value: &Value,
        field: &str,
        schemas: &[&Value],
        root: &Value,
        fields: &mut BTreeSet<String>,
    ) {
        let applicable = ApplicableSchemas::new(schemas, root, value);
        if (value.is_string() || value.is_array() || value.is_object()) && applicable.is_annotated()
        {
            fields.insert(field.to_owned());
        }
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let schemas = applicable.property(key);
                    visit(child, &property_field(field, key), &schemas, root, fields);
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    let schemas = applicable.item(index);
                    visit(child, &format!("{field}/{index}"), &schemas, root, fields);
                }
            }
            _ => {}
        }
    }
    let mut fields = BTreeSet::new();
    visit(value, "", &[schema], schema, &mut fields);
    fields
}

// Share schema navigation, not value traversal: projection hydrates stored values
// and stops at shortened parents, while JS discovery must retain descendants.
struct ApplicableSchemas<'a>(Vec<&'a Value>);

impl<'a> ApplicableSchemas<'a> {
    fn new(schemas: &[&'a Value], root: &'a Value, value: &Value) -> Self {
        let mut applicable = Vec::new();
        for schema in schemas {
            expand(schema, root, value, &mut applicable, &mut Vec::new());
        }
        Self(applicable)
    }

    fn is_annotated(&self) -> bool {
        self.0.iter().any(|schema| schema[ANNOTATION] == true)
    }

    fn property(&self, key: &str) -> Vec<&'a Value> {
        self.0
            .iter()
            .filter_map(|schema| schema.get("properties")?.get(key))
            .collect()
    }

    fn item(&self, index: usize) -> Vec<&'a Value> {
        self.0
            .iter()
            .filter_map(|schema| {
                schema
                    .get("prefixItems")
                    .and_then(|items| items.get(index))
                    .or_else(|| schema.get("items"))
            })
            .collect()
    }
}

fn property_field(field: &str, key: &str) -> String {
    format!("{field}/{}", key.replace('~', "~0").replace('/', "~1"))
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
    if end < bytes.len()
        && let Some(last) = bytes[..end].iter().rposition(|&b| b == b'\n')
    {
        end = last + 1;
    }
    if end > 0 && bytes.get(end) == Some(&b'\n') && bytes[end - 1] == b'\r' {
        end -= 1;
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

// Grouped maps share one budget across keys and array entries. Parse only the
// bounded prefix and retain complete items, including directory names and sizes.
fn object_prefix(bytes: &[u8]) -> Result<(Value, usize, bool), ToolError> {
    let maximum = FIELD_BYTES.min(line_end(bytes));
    let mut position = 1;
    let mut map = serde_json::Map::new();
    fn skip(bytes: &[u8], position: &mut usize) {
        while bytes
            .get(*position)
            .is_some_and(|b| b.is_ascii_whitespace() || *b == b',')
        {
            *position += 1;
        }
    }
    fn parse(bytes: &[u8], position: usize, maximum: usize) -> Option<(Value, usize)> {
        if position >= maximum {
            return None;
        }
        let mut stream =
            serde_json::Deserializer::from_slice(&bytes[position..maximum]).into_iter::<Value>();
        let value = stream.next()?.ok()?;
        Some((value, position + stream.byte_offset()))
    }
    loop {
        skip(bytes, &mut position);
        if bytes.get(position) == Some(&b'}') {
            return Ok((Value::Object(map), position + 1, false));
        }
        let key_start = position;
        let Some((Value::String(key), end)) = parse(bytes, position, maximum) else {
            return Ok((Value::Object(map), position.min(bytes.len()), true));
        };
        position = end;
        skip(bytes, &mut position);
        if bytes.get(position) != Some(&b':') {
            return Ok((Value::Object(map), key_start, true));
        }
        position += 1;
        skip(bytes, &mut position);
        if bytes.get(position) == Some(&b'[') {
            position += 1;
            let mut items = Vec::new();
            loop {
                skip(bytes, &mut position);
                if bytes.get(position) == Some(&b']') {
                    map.insert(key.clone(), Value::Array(items));
                    position += 1;
                    break;
                }
                let Some((item, end)) = parse(bytes, position, maximum) else {
                    if !items.is_empty() {
                        map.insert(key, Value::Array(items));
                    }
                    return Ok((Value::Object(map), position.min(bytes.len()), true));
                };
                items.push(item);
                // Measure the candidate in place instead of cloning every item
                // and earlier group. Restore even a duplicate key before rollback.
                let previous = map.insert(key.clone(), Value::Array(items));
                let oversized = serde_json::to_vec(&map)?.len() > FIELD_BYTES;
                let Some(Value::Array(candidate)) = map.remove(&key) else {
                    unreachable!("inserted array candidate");
                };
                items = candidate;
                if let Some(previous) = previous {
                    map.insert(key.clone(), previous);
                }
                if oversized {
                    items.pop();
                    if !items.is_empty() {
                        map.insert(key, Value::Array(items));
                    }
                    return Ok((Value::Object(map), position, true));
                }
                position = end;
            }
        } else {
            let Some((value, end)) = parse(bytes, position, maximum) else {
                return Ok((Value::Object(map), key_start, true));
            };
            map.insert(key.clone(), value);
            if serde_json::to_vec(&map)?.len() > FIELD_BYTES {
                map.remove(&key);
                return Ok((Value::Object(map), key_start, true));
            }
            position = end;
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
        let id = manager.test_create(spec).await;
        let output = ToolOutput::new(value);
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
        assert!(view.get("console").is_none());
        assert_eq!(view["truncated"].as_array().unwrap().len(), 3);
        assert!(view.get("preview").is_none());
        assert_eq!(manager.snapshot(id).await.unwrap().output.unwrap(), value);
        let session = manager.store().id();
        drop(manager);
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
            args.field = Some(entry["field"].as_str().unwrap().into());
            args.start = Some(entry["next_start"].as_u64().unwrap() as usize);
            args.offset = Some(entry["next_offset"].as_u64().unwrap_or(0) as usize);
            let page = restored
                .present_output(args, &Default::default())
                .await
                .unwrap();
            let first = &page["preview"]["lines"][0];
            assert_eq!(entry["next_start"], line);
            assert_eq!(entry["next_offset"].as_u64().unwrap_or(0) as usize, offset);
            assert!(first.as_str().unwrap().starts_with(expected));
        }
    }

    #[tokio::test]
    async fn initial_positions_read_remaining_service_and_container_lines_without_gaps() {
        let schema = json!({"properties":{"stdout":{ANNOTATION:true}}});
        for (width, count) in [(94, 80), (223, 18), (2047, 2)] {
            let text = (1..=count)
                .map(|line| format!("{line:0width$}\r\n"))
                .collect::<String>();
            let (_root, manager, id) = fixture(json!({"stdout":text}), schema.clone(), None).await;
            let view = manager
                .present_output(OutputArgs::new(id), &Default::default())
                .await
                .unwrap();
            let position = view["truncated"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["field"] == "/result/stdout")
                .unwrap();
            assert_eq!(position["total_lines"], count);
            let prefix = view["result"]["stdout"].as_str().unwrap();
            assert!(text.starts_with(prefix));
            let mut query = OutputArgs::new(id);
            query.field = Some("/result/stdout".into());
            query.start = Some(position["next_start"].as_u64().unwrap() as usize);
            query.offset = Some(position["next_offset"].as_u64().unwrap_or(0) as usize);
            let mut remaining = String::new();
            loop {
                let page = manager
                    .present_output(query.clone(), &Default::default())
                    .await
                    .unwrap();
                for row in page["preview"]["lines"].as_array().unwrap() {
                    remaining.push_str(row.as_str().unwrap());
                    remaining.push_str("\r\n");
                }
                let Some(start) = page["preview"]["next_start"].as_u64() else {
                    break;
                };
                query.start = Some(start as usize);
                query.offset = Some(page["preview"]["next_offset"].as_u64().unwrap_or(0) as usize);
            }
            assert_eq!(format!("{prefix}{remaining}"), text);
        }
    }
}
