//! Only schema-annotated fields may be shortened in automatic result presentation.
use super::*;
use crate::json_schema::{Node, Resolver, accepts, declared_types};

pub(super) const FIELD_BYTES: usize = CONTENT_BYTES;
pub(super) const FIELD_LINES: usize = 100;
const ANNOTATION: &str = "x-skyhook-truncatable";

/// Typed presentation assembled before the JobView is serialized.
pub(super) struct Projected {
    pub(super) result: Value,
    pub(super) truncated: Vec<OutputTruncation>,
    pub(super) notice: Option<String>,
}

pub(super) fn project(
    saved: &Saved,
    schema: &Value,
    cancellation: &super::super::CancellationToken,
    annotated: &BTreeSet<FieldPointer>,
) -> Result<Projected, ToolError> {
    let product = saved
        .product
        .as_ref()
        .ok_or_else(|| ToolError::failed("saved output is missing"))?;
    let mut document = product.document();
    let mut projection = Projection {
        saved,
        truncated: Vec::new(),
        cancellation,
        annotated,
    };
    projection.visit(
        &mut document["result"],
        &FieldPointer::result(),
        &[Node::root(schema)],
        schema,
    )?;
    Ok(Projected {
        result: document["result"].take(),
        truncated: projection.truncated,
        notice: (!product.captures_complete).then(|| "Output incomplete.".into()),
    })
}

struct Projection<'a> {
    saved: &'a Saved,
    truncated: Vec<OutputTruncation>,
    cancellation: &'a super::super::CancellationToken,
    annotated: &'a BTreeSet<FieldPointer>,
}

impl Projection<'_> {
    fn visit(
        &mut self,
        value: &mut Value,
        field: &FieldPointer,
        schemas: &[Node<'_>],
        root: &Value,
    ) -> Result<(), ToolError> {
        if self.cancellation.is_cancelled() {
            return Err(ToolError::cancelled());
        }
        let applicable = ApplicableSchemas::new(schemas, root, value);
        if (self.annotated.contains(field) || applicable.is_annotated())
            && (value.is_string() || value.is_array() || value.is_object())
        {
            let mut source = materialize_field(self.saved, field, value, self.cancellation)?;
            let mut bytes = Vec::new();
            (&mut source)
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
                let index = source.index(self.cancellation)?;
                let line = 1 + bytes[..end].iter().filter(|&&b| b == b'\n').count();
                let offset = bytes[..end]
                    .iter()
                    .rposition(|&b| b == b'\n')
                    .map_or(end, |last| end - last - 1);
                self.truncated.push(OutputTruncation {
                    field: field.to_string(),
                    total_lines: index.total_lines,
                    next_start: line,
                    next_offset: offset,
                });
            }
            return Ok(());
        }
        // Storage offloading does not grant permission to truncate a field.
        // Stored fields were validated as UTF-8 or JSON when saved.
        if self.saved.fields.contains(field) {
            load_field(self.saved, value, field)?;
        }
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let children = applicable.property(key);
                    self.visit(child, &field.property(key), &children, root)?;
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter_mut().enumerate() {
                    let children = applicable.item(index);
                    self.visit(child, &field.index(index), &children, root)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Resolve the same annotations used by native projection, before crossing the JS
/// bridge. Keep descendant annotations as well, since scripts can extract them.
pub(crate) fn annotated_fields(value: &Value, schema: &Value) -> BTreeSet<FieldPointer> {
    fn visit(
        value: &Value,
        field: &FieldPointer,
        schemas: &[Node<'_>],
        root: &Value,
        fields: &mut BTreeSet<FieldPointer>,
    ) {
        let applicable = ApplicableSchemas::new(schemas, root, value);
        if (value.is_string() || value.is_array() || value.is_object()) && applicable.is_annotated()
        {
            fields.insert(field.clone());
        }
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let schemas = applicable.property(key);
                    visit(child, &field.property(key), &schemas, root, fields);
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    let schemas = applicable.item(index);
                    visit(child, &field.index(index), &schemas, root, fields);
                }
            }
            _ => {}
        }
    }
    let mut fields = BTreeSet::new();
    visit(
        value,
        &FieldPointer::root(),
        &[Node::root(schema)],
        schema,
        &mut fields,
    );
    fields
}

// Share schema navigation, not value traversal: projection hydrates stored values
// and stops at shortened parents, while JS discovery must retain descendants.
struct ApplicableSchemas<'a>(Vec<Node<'a>>);

impl<'a> ApplicableSchemas<'a> {
    fn new(schemas: &[Node<'a>], root: &'a Value, value: &Value) -> Self {
        let resolver = Resolver::new(root);
        let mut applicable = Vec::new();
        for schema in schemas {
            expand(&resolver, *schema, value, &mut applicable, &mut Vec::new());
        }
        Self(applicable)
    }

    fn is_annotated(&self) -> bool {
        self.0.iter().any(|node| node.schema[ANNOTATION] == true)
    }

    fn property(&self, key: &str) -> Vec<Node<'a>> {
        self.0
            .iter()
            .filter_map(|node| Some(node.child(node.schema.get("properties")?.get(key)?)))
            .collect()
    }

    fn item(&self, index: usize) -> Vec<Node<'a>> {
        self.0
            .iter()
            .filter_map(|node| {
                let schema = node.schema;
                let item = (schema.get("prefixItems").and_then(|items| items.get(index)))
                    .or_else(|| schema.get("items"))?;
                Some(node.child(item))
            })
            .collect()
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
        Err(_) => return Err(ToolError::failed("saved output is not UTF-8")),
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
    resolver: &Resolver<'a>,
    node: Node<'a>,
    value: &Value,
    out: &mut Vec<Node<'a>>,
    refs: &mut Vec<Node<'a>>,
) {
    // A partial result may omit required siblings. Their absence must not disable
    // annotations on fields that were successfully captured.
    out.push(node);
    if let Some(target) = resolver.target(node)
        && !refs.iter().any(|active| active.is(target))
    {
        refs.push(target);
        expand(resolver, target, value, out, refs);
        refs.pop();
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(branches) = node.schema.get(key).and_then(Value::as_array) {
            for branch in branches.iter().map(|branch| node.child(branch)) {
                if key == "allOf" || matches(resolver, branch, value, &mut Vec::new()) {
                    expand(resolver, branch, value, out, refs);
                }
            }
        }
    }
}

fn matches<'a>(
    resolver: &Resolver<'a>,
    node: Node<'a>,
    value: &Value,
    refs: &mut Vec<Node<'a>>,
) -> bool {
    let schema = node.schema;
    if schema == &Value::Bool(false) {
        return false;
    }
    if let Some(target) = resolver.target(node)
        && !refs.iter().any(|active| active.is(target))
    {
        refs.push(target);
        let applies = matches(resolver, target, value, refs);
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
    if declared_types(schema).is_some_and(|types| !accepts(types, value)) {
        return false;
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
                && !matches(resolver, node.child(property), child, refs)
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
        job::{JobOutcome, JobSpec},
        session::SessionStore,
        tool::ToolOutput,
    };

    async fn fixture(
        value: Value,
        schema: Value,
        error: Option<String>,
    ) -> (tempfile::TempDir, JobManager, JobId) {
        let (root, store, agent) = crate::session::fixture::on_disk().await;
        let manager = JobManager::new(store.clone());
        let mut spec = JobSpec::test(agent, "annotated");
        spec.output_schema = Some(schema);
        let id = manager.test_create(spec).await;
        let output = ToolOutput::new(value);
        let outcome = match error {
            Some(message) => ToolError::failed(message).with_result(output).into(),
            None => JobOutcome::Completed(output),
        };
        manager.finish(id, outcome).await.unwrap();
        (root, manager, id)
    }

    /// Follows a truncation marker or page continuation as a new field query.
    fn continuation(id: JobId, position: &Value) -> OutputArgs {
        let mut args = OutputArgs::new(id);
        args.field = Some(
            position["field"]
                .as_str()
                .unwrap_or("/result/stdout")
                .parse()
                .unwrap(),
        );
        args.start = Some(position["next_start"].as_u64().unwrap() as usize);
        args.offset = Some(position["next_offset"].as_u64().unwrap_or(0) as usize);
        args
    }

    fn marker<'a>(view: &'a Value, field: &str) -> &'a Value {
        view["presentation"]["truncated"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["field"] == field)
            .unwrap()
    }

    #[tokio::test]
    async fn limits_are_per_field_and_unannotated_data_and_diagnostics_survive_resume() {
        let value = json!({"content":"line\n".repeat(150), "stdout":"é\\\"".repeat(FIELD_BYTES),
            "stderr":"e".repeat(2 * FIELD_BYTES), "metadata":"m".repeat(92000), "extra":["z".repeat(10000)], "exit_code":7});
        let schema = json!({"type":"object","properties":{
            "content":{ANNOTATION:true},"stdout":{ANNOTATION:true},"stderr":{ANNOTATION:true}
        }});
        let error = "failure details ".repeat(2000);
        let (root, manager, id) = fixture(value.clone(), schema, Some(error.clone())).await;
        let view = manager
            .present_output(OutputArgs::new(id), &Default::default())
            .await
            .unwrap();
        let result = &view["result"];
        assert_eq!(result["content"], "line\n".repeat(100));
        for field in ["stdout", "stderr"] {
            assert_eq!(result[field].as_str().unwrap().len(), FIELD_BYTES);
        }
        for field in ["metadata", "extra", "exit_code"] {
            assert_eq!(result[field], value[field]);
        }
        let rendered_error = ToolError::failed(error)
            .diagnostic()
            .render(&Default::default());
        assert_eq!(view["error"], rendered_error);
        assert_eq!(
            manager
                .metadata(id)
                .await
                .unwrap()
                .rendered_error(&CapabilitySet::default())
                .as_deref(),
            Some(rendered_error.as_str())
        );
        assert!(view.get("console").is_none() && view["presentation"]["preview"].is_null());
        assert_eq!(
            view["presentation"]["truncated"].as_array().unwrap().len(),
            3
        );
        assert_eq!(manager.snapshot(id).await.unwrap().output.unwrap(), value);
        let session = manager.store().id();
        manager.drain_supervisors().await;
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
            let entry = marker(&view, field);
            assert_eq!(entry["next_start"], line);
            assert_eq!(entry["next_offset"].as_u64().unwrap_or(0) as usize, offset);
            let page = restored
                .present_output(continuation(id, entry), &Default::default())
                .await
                .unwrap();
            assert!(
                page["presentation"]["preview"]["lines"][0]
                    .as_str()
                    .unwrap()
                    .starts_with(expected)
            );
        }
    }

    #[tokio::test]
    async fn initial_positions_read_remaining_service_and_container_lines_without_gaps() {
        let schema = json!({"properties":{"stdout":{ANNOTATION:true}}});
        for (width, count) in [
            (94, 150),
            (223, 150),
            (FIELD_BYTES / 2 + 1, 3),
            (FIELD_BYTES - 1, 2),
        ] {
            let text: String = (1..=count)
                .map(|line| format!("{line:0width$}\r\n"))
                .collect();
            let (_root, manager, id) = fixture(json!({"stdout":text}), schema.clone(), None).await;
            let view = manager
                .present_output(OutputArgs::new(id), &Default::default())
                .await
                .unwrap();
            let position = marker(&view, "/result/stdout");
            assert_eq!(position["total_lines"], count);
            let prefix = view["result"]["stdout"].as_str().unwrap();
            assert!(text.starts_with(prefix));
            let mut query = continuation(id, position);
            let mut remaining = String::new();
            loop {
                let page = manager
                    .present_output(query.clone(), &Default::default())
                    .await
                    .unwrap();
                let preview = &page["presentation"]["preview"];
                for (index, row) in preview["lines"].as_array().unwrap().iter().enumerate() {
                    remaining.push_str(row.as_str().unwrap());
                    let line = query.start.unwrap() + index;
                    if preview["next_start"].as_u64() != Some(line as u64)
                        || preview["next_offset"] == 0
                    {
                        remaining.push_str("\r\n");
                    }
                }
                if preview["next_start"].is_null() {
                    break;
                }
                query = continuation(id, preview);
            }
            assert_eq!(format!("{prefix}{remaining}"), text);
        }
    }
}
