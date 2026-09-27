//! Automatic presentation of finished results, and the schema annotations it
//! reads: `x-skyhook-complete` fields are never shortened, and JSON-declared text
//! is classified when saved.
use super::*;
use crate::json_schema::{Node, Resolver, accepts, declared_types};
use crate::tool::output::COMPLETE;

/// Typed presentation assembled before the JobView is serialized.
pub(super) struct Projected {
    pub(super) result: Option<Value>,
    pub(super) shape: Option<Value>,
    pub(super) truncated: Vec<OutputTruncation>,
    pub(super) notice: Option<views::Notice>,
}

/// Preview the saved result within the budget, showing its complete fields and
/// presented diagnostics whole.
pub(super) fn project(
    saved: &Saved,
    cancellation: &super::super::CancellationToken,
) -> Result<Projected, ToolError> {
    let product = saved
        .product
        .as_ref()
        .ok_or_else(|| ToolError::failed("saved output is missing"))?;
    let notice = (!product.captures_complete).then_some(views::Notice::OutputIncomplete);
    let document = product.document();
    let root = FieldPointer::result();
    let mut complete = saved.complete.clone();
    complete.extend(saved.diagnostic_fields.iter().cloned());
    let (result, shape, truncated) = match &product.result {
        None => (None, None, Vec::new()),
        Some(_) if complete.contains(&root) => {
            let mut document = hydrate(saved, document)?;
            (Some(document["result"].take()), None, Vec::new())
        }
        Some(_) => {
            let reading = render::Reading::Sampled {
                complete: &complete,
            };
            let mut input = render::json_text(saved, &root, &document["result"], reading)?;
            let clipped = input.take_clipped();
            let preview = preview::preview(input, &root, &complete, &clipped, cancellation)?;
            (Some(preview.value), preview.shape, preview.truncated)
        }
    };
    Ok(Projected {
        result,
        shape,
        truncated,
        notice,
    })
}

/// Fields of `value` at `field` the schema declares complete, descendants
/// included, since scripts can extract them.
pub(crate) fn complete_fields(
    value: &Value,
    field: &FieldPointer,
    schema: &Value,
) -> BTreeSet<FieldPointer> {
    schema_fields(value, field, schema, |applicable, _| {
        applicable.is_complete()
    })
}

/// String fields whose schema declares JSON content, which finalization classifies.
pub(crate) fn json_text_fields(
    value: &Value,
    field: &FieldPointer,
    schema: &Value,
) -> BTreeSet<FieldPointer> {
    schema_fields(value, field, schema, |applicable, value| {
        value.is_string() && applicable.declares_json()
    })
}

fn schema_fields(
    value: &Value,
    field: &FieldPointer,
    schema: &Value,
    selects: impl Fn(&ApplicableSchemas<'_>, &Value) -> bool + Copy,
) -> BTreeSet<FieldPointer> {
    fn visit(
        value: &Value,
        field: &FieldPointer,
        schemas: &[Node<'_>],
        root: &Value,
        selects: impl Fn(&ApplicableSchemas<'_>, &Value) -> bool + Copy,
        fields: &mut BTreeSet<FieldPointer>,
    ) {
        let applicable = ApplicableSchemas::new(schemas, root, value);
        if selects(&applicable, value) {
            fields.insert(field.clone());
        }
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let schemas = applicable.property(key);
                    visit(child, &field.property(key), &schemas, root, selects, fields);
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    let schemas = applicable.item(index);
                    visit(child, &field.index(index), &schemas, root, selects, fields);
                }
            }
            _ => {}
        }
    }
    let mut fields = BTreeSet::new();
    visit(
        value,
        field,
        &[Node::root(schema)],
        schema,
        selects,
        &mut fields,
    );
    fields
}

// Share schema navigation across annotation queries over in-memory values.
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

    fn is_complete(&self) -> bool {
        self.0.iter().any(|node| node.schema[COMPLETE] == true)
    }

    fn declares_json(&self) -> bool {
        self.0
            .iter()
            .any(|node| node.schema["contentMediaType"] == "application/json")
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
        job::{
            JobOutcome, JobSpec,
            output::preview::{SAMPLE_TEXT_BYTES, TEXT_BYTES},
        },
        session::SessionStore,
        tool::ToolOutput,
    };

    async fn fixture(
        value: Value,
        schema: Value,
        error: Option<String>,
    ) -> (tempfile::TempDir, JobManager, JobId) {
        let (root, store, agent) = crate::session::tests::on_disk().await;
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

    /// Record text fields keep their own limits outside the JSON budget; strings
    /// inside samples are clipped. Errors and complete fields stay whole, and views
    /// replay unchanged after resume.
    #[tokio::test]
    async fn text_fields_keep_their_limits_and_views_survive_resume() {
        let value = json!({"content":"line\n".repeat(150), "stdout":"é\\\"".repeat(TEXT_BYTES),
            "stderr":"e".repeat(2 * TEXT_BYTES), "extra":["z".repeat(10000)],
            "instructions":"i".repeat(2 * TEXT_BYTES), "exit_code":7});
        let schema = json!({"type":"object","properties":{"instructions":{COMPLETE:true}}});
        let error = "failure details ".repeat(2000);
        let (root, manager, id) = fixture(value.clone(), schema, Some(error.clone())).await;
        let view = manager
            .present_output(OutputArgs::new(id), &Default::default())
            .await
            .unwrap();
        let result = &view["result"];
        assert_eq!(result["content"], "line\n".repeat(100));
        for field in ["stdout", "stderr"] {
            assert_eq!(result[field].as_str().unwrap().len(), TEXT_BYTES);
        }
        // A string inside an array keeps a sample's prefix once it exceeds the budget.
        assert_eq!(
            result["extra"][0].as_str().unwrap().len(),
            SAMPLE_TEXT_BYTES[0]
        );
        for field in ["instructions", "exit_code"] {
            assert_eq!(result[field], value[field]);
        }
        let rendered_error = ToolError::failed(error)
            .diagnostic()
            .render(&Default::default());
        assert_eq!(view["error"], rendered_error);
        // Only strings were cut, so there is no shape. What was cut is read first.
        assert!(view["presentation"]["shape"].is_null());
        let text = view.to_string();
        assert!(text.find("\"presentation\"").unwrap() < text.find("\"result\"").unwrap());
        let cut: Vec<_> = view["presentation"]["truncated"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["field"].as_str().unwrap())
            .collect();
        assert_eq!(
            cut,
            [
                "/result/content",
                "/result/stdout",
                "/result/stderr",
                "/result/extra/0"
            ]
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
            ("/result/stderr", "e", 1, TEXT_BYTES),
            ("/result/extra/0", "z", 1, SAMPLE_TEXT_BYTES[0]),
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
        let schema = json!(true);
        for (width, count) in [
            (94, 150),
            (223, 150),
            (TEXT_BYTES / 2 + 1, 3),
            (TEXT_BYTES - 1, 2),
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
