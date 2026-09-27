//! Saved output presentation, independent of model providers and execution transports.
mod args;
mod captures;
mod elements;
mod finalization;
mod json;
mod preview;
mod products;
mod projection;
mod query;
mod reader;
mod render;
mod saved;
mod shape;
pub use args::OutputArgs;
use args::Selection;
pub(crate) use args::{DEFAULT_LIMIT, MAX_CONTEXT, MAX_LIMIT, line_matcher};
pub(crate) use captures::{
    CaptureCollector, CaptureWriter, CompletedCapture, HostOutput, PendingCapture, TextCaptureField,
};
pub use captures::{CaptureDescriptor, CaptureKind};
pub(crate) use finalization::save_completed;
pub(crate) use json::{Detection, parse_text as parse_json_text};
pub use products::{
    Continuation, ElementPage, LinePage, Match, MatchPage, MemberPage, NumberedLine, OutputFields,
    OutputPreview, OutputSelection, OutputTruncation, PageLines, PresentedOutput,
};
pub(crate) use projection::complete_fields;
pub(crate) use reader::Source;
use render::disk_rendering;
pub(super) use saved::blocking;
pub use saved::diagnostic_slot;
pub(crate) use saved::{DiagnosticSlot, Output, Saved, diagnostic_slot_in, presented_size};
use saved::{Stored, database, hydrate, save_document};

use super::{JobError, JobManager, JobState, OutputPresentation, views};
pub use crate::tool::output::FieldPointer;
use crate::{
    identity::JobId,
    session::{CaptureRow, SharedDb},
    tool::{ToolError, diagnostic::DiagnosticViewer, policy::CapabilitySet},
};
use base64::Engine as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Read, Write},
};

// Leave room for a typical 100-line page, plus its JobView envelope.
pub(crate) const CONTENT_BYTES: usize = 32 * 1024;
pub(crate) const PAGE_BYTES: usize = CONTENT_BYTES + 2048;
const IO_BUFFER_BYTES: usize = 64 * 1024;
/// Capture pages host inspection reads at once while hydrating a presentation.
const HYDRATED_PAGES: usize = 4;

/// Omit null-valued object fields from human-only UI display copies.
/// Never use for model, history, saved output, or JavaScript response data.
/// Null array entries and literal strings stay intact to preserve indices and text.
pub fn omit_null_fields(value: &mut Value) {
    match value {
        Value::Object(fields) => fields.retain(|_, value| {
            omit_null_fields(value);
            !value.is_null()
        }),
        Value::Array(values) => values.iter_mut().for_each(omit_null_fields),
        _ => {}
    }
}

/// Who reads a presentation. Model-facing reads acknowledge the job's pending
/// notification; host inspection never does and always shows details.
#[derive(Clone, Copy)]
pub(crate) enum OutputOptions {
    Host,
    Model { presentation: OutputPresentation },
}

impl JobManager {
    /// Record the fields of a script's saved return value its presentation never shortens.
    pub(crate) async fn save_complete_fields(
        &self,
        job: JobId,
        complete: BTreeSet<FieldPointer>,
    ) -> Result<(), ToolError> {
        if complete.is_empty() {
            return Ok(());
        }
        let output = self.output(job);
        let job = output.job.get();
        let fields: Vec<_> = complete.into_iter().collect();
        blocking(move || {
            output
                .db
                .save_complete_fields(job, &fields)
                .map_err(database)
        })
        .await
    }

    pub(crate) fn output(&self, id: JobId) -> Output {
        Output {
            db: self.inner.store.outputs(),
            job: id,
        }
    }

    #[cfg(test)]
    pub(crate) async fn present_output(
        &self,
        args: OutputArgs,
        capabilities: &CapabilitySet,
    ) -> Result<Value, ToolError> {
        self.present_output_with(
            args,
            super::CancellationToken::new(),
            capabilities,
            OutputOptions::Model {
                presentation: crate::job::OutputPresentation::Full,
            },
        )
        .await
        .map(PresentedOutput::into_view)
    }

    /// Host inspection never acknowledges an agent's pending notification.
    /// Stored captures are listed as `captures: [{field, kind, complete}]`; an
    /// incomplete capture is exposed only as raw pages, never as a result.
    pub async fn inspect_output(
        &self,
        args: OutputArgs,
        cancellation: super::CancellationToken,
        capabilities: &CapabilitySet,
    ) -> Result<Value, ToolError> {
        self.present_output_with(args, cancellation, capabilities, OutputOptions::Host)
            .await
            .map(PresentedOutput::into_view)
    }

    /// Saved pointers one level below `parent`, for host UI field discovery: the
    /// first members or elements of a JSON field, and at the result, its captures
    /// too. Large JSON is listed a level at a time rather than enumerated.
    pub async fn inspect_output_fields(
        &self,
        job: JobId,
        parent: FieldPointer,
        index: usize,
    ) -> Result<OutputFields, ToolError> {
        let terminal = self.metadata(job).await?.state.is_terminal();
        let output = self.output(job);
        blocking(move || {
            let saved = Saved::load(&output)?;
            let cancellation = super::CancellationToken::new();
            let mut listed = OutputFields {
                fields: Vec::new(),
                next_index: None,
            };
            if terminal
                && saved.product.is_some()
                && let Some(render::Resolved::Json {
                    mut json,
                    container,
                }) = render::resolve(&saved, &parent, render::Reading::SAMPLED, &cancellation)?
            {
                listed =
                    elements::children(&mut json.reader, container, &parent, index, &cancellation)?;
            }
            if parent == FieldPointer::result() && index == 0 {
                for capture in captures::available_captures(&saved, terminal) {
                    if !listed.fields.contains(&capture.field) {
                        listed.fields.push(capture.field);
                    }
                }
            }
            Ok(listed)
        })
        .await
    }

    /// Host inspection with automatic pages for captures absent from the whole
    /// presentation. Any explicit selection (including `context: 0`) suppresses
    /// hydration, and a present JSON null is not an absent capture. A page
    /// failure is embedded as `{error}` in that capture's `output`.
    pub async fn inspect_output_with_captures(
        &self,
        args: OutputArgs,
        cancellation: super::CancellationToken,
        capabilities: &CapabilitySet,
    ) -> Result<PresentedOutput, ToolError> {
        use futures_util::{StreamExt, stream};

        let mut output = self
            .present_output_with(
                args.clone(),
                cancellation.clone(),
                capabilities,
                OutputOptions::Host,
            )
            .await?;
        let mut pages = stream::iter(std::mem::take(&mut output.capture_targets))
            .map(|(index, field)| {
                let mut query = OutputArgs::new(args.job);
                query.field = Some(field);
                let cancellation = cancellation.clone();
                async move {
                    let page = self
                        .present_output_with(query, cancellation, capabilities, OutputOptions::Host)
                        .await;
                    (index, page)
                }
            })
            .buffer_unordered(HYDRATED_PAGES);
        while let Some((index, page)) = pages.next().await {
            output.attach_capture(index, page);
        }
        Ok(output)
    }

    /// Return the state and presentation from the same job snapshot. Explicit
    /// field/page queries always retain full output, even under Automatic policy.
    pub(crate) async fn present_output_with<'a>(
        &self,
        args: OutputArgs,
        cancellation: super::CancellationToken,
        viewer: impl Into<DiagnosticViewer<'a>>,
        options: OutputOptions,
    ) -> Result<PresentedOutput, ToolError> {
        let viewer = viewer.into();
        let (acknowledge, presentation) = match options {
            OutputOptions::Host => (false, OutputPresentation::Full),
            OutputOptions::Model { presentation } => (true, presentation),
        };
        let mut query = args.admit()?;
        let output_selection = args.selection();
        let explicit = output_selection == OutputSelection::Explicit;
        let (mut envelope, images) = {
            let jobs = self.inner.jobs.lock().await;
            let entry = jobs.get(&args.job).ok_or(JobError::Unknown(args.job))?;
            (
                entry.envelope(args.job),
                match entry.finished() {
                    Some(finished) if output_selection == OutputSelection::WholeWithImages => {
                        finished.images.clone()
                    }
                    _ => Vec::new(),
                },
            )
        };
        let terminal = envelope.state.is_terminal();
        let question = envelope.state == JobState::WaitingInput;
        let live_question = envelope.question.take();
        let kind = if explicit || presentation == OutputPresentation::Full {
            views::ViewKind::Inspection
        } else {
            views::ViewKind::Response
        };
        let mut result = None;
        let mut annotations = views::Presentation::default();
        let output = self.output(args.job);
        let mut saved = blocking(move || Saved::load(&output)).await?;
        saved.present_diagnostics(envelope.output_diagnostic.as_ref(), viewer)?;
        let saved = std::sync::Arc::new(saved);
        let mut captures = captures::available_captures(&saved, terminal);
        let incomplete_capture = terminal
            && captures.iter().any(|capture| {
                !capture.complete
                    && (!explicit
                        || capture.field == query.field
                        || query.field.contains(&capture.field))
            });
        let structured = !explicit && terminal && saved.product.is_some();
        let mut presented_question = false;
        let mut question_page = None;
        if structured {
            let projected = saved.clone();
            let cancellation = cancellation.clone();
            let projected =
                blocking(move || projection::project(&projected, &cancellation)).await?;
            result = projected.result;
            annotations.shape = projected.shape;
            annotations.truncated =
                (!projected.truncated.is_empty()).then_some(projected.truncated);
            annotations.notice = projected.notice;
        } else if question && let Some(value) = live_question {
            if !explicit {
                annotations.question = Some(value);
                presented_question = true;
            } else {
                let bytes = serde_json::to_vec_pretty(&value)?;
                let key =
                    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(&bytes));
                let field = questions().property(&key);
                if args.field.is_none() {
                    query.field = field.clone();
                }
                presented_question = query.field == field;
                if presented_question {
                    question_page = Some(bytes);
                }
            }
        }
        if !structured && annotations.question.is_none() {
            if !explicit && !question && saved.product.is_some() {
                query.field = FieldPointer::root();
            }
            let paged = saved.clone();
            let selected = query.clone();
            let cancellation = cancellation.clone();
            // A whole-result query always resolves to "/result" or "" here.
            let unavailable = !terminal
                && !question
                && (query.field == FieldPointer::result() || query.field.is_root());
            let mut page = if unavailable {
                if matches!(
                    query.position,
                    args::Position::Index(_) | args::Position::Query { .. }
                ) {
                    return Err(ToolError::invalid_arguments(
                        "the result is saved when the job finishes: page or query it then",
                    ));
                }
                OutputPreview::Lines(reader::empty(&query.text(), None, false))
            } else {
                blocking(move || page(&paged, &selected, question_page, terminal, &cancellation))
                    .await?
            };
            if terminal
                && saved
                    .product
                    .as_ref()
                    .is_some_and(|product| !product.captures_complete)
            {
                annotations.notice = Some(views::Notice::OutputIncomplete);
            }
            if matches!(options, OutputOptions::Model { .. }) && args.field.as_ref() == page.field()
            {
                page.clear_field();
            }
            annotations.preview = Some(page);
        }
        if incomplete_capture {
            annotations.notice = Some(views::Notice::OutputIncomplete);
        }
        if acknowledge && ((terminal && !questions().contains(&query.field)) || presented_question)
        {
            self.claim(args.job).await?;
        }
        // A complete capture is simply a field of the result, read by pointer.
        captures.retain(|capture| !capture.complete);
        // Hydration targets come from discovered records, never decoded wire
        // descriptors.
        let capture_targets = captures
            .iter()
            .enumerate()
            .filter(|(_, capture)| {
                output_selection == OutputSelection::WholeWithImages
                    && !shows(result.as_ref(), &capture.field)
            })
            .map(|(index, capture)| (index, capture.field.clone()))
            .collect();
        annotations.captures = (!captures.is_empty()).then_some(captures);
        Ok(PresentedOutput {
            state: envelope.state,
            view: envelope.view(viewer, kind, result, annotations),
            images,
            capture_targets,
        })
    }

    pub(crate) async fn hydrate_envelope(
        &self,
        envelope: &mut super::JobEnvelope,
    ) -> Result<(), JobError> {
        if !envelope.state.is_terminal() {
            return Ok(());
        }
        let output = self.output(envelope.id);
        let product = blocking(move || {
            let saved = Saved::load(&output)?;
            (saved.product.as_ref())
                .map(|product| {
                    let mut document = hydrate(&saved, product.document())?;
                    Ok(product.result.is_some().then(|| document["result"].take()))
                })
                .transpose()
        })
        .await
        .map_err(|error| JobError::Output(Box::new(error)))?;
        if let Some(result) = product {
            envelope.output = result;
        }
        Ok(())
    }
}

/// One page of the selected field, in its own unit: lines of text, or the
/// elements or members of JSON. A selector of the other unit is rejected.
fn page(
    saved: &Saved,
    query: &args::Query,
    question: Option<Vec<u8>>,
    terminal: bool,
    cancellation: &super::CancellationToken,
) -> Result<OutputPreview, ToolError> {
    let closed = terminal || questions().contains(&query.field);
    let resolved = match question {
        Some(bytes) => {
            let bytes = Box::new(std::io::Cursor::new(bytes));
            Some(render::Resolved::json(render::JsonField::new(
                bytes,
                Default::default(),
            ))?)
        }
        None => {
            // A query reads its field whole; pages need only prefixes of stored text.
            let reading = match query.position {
                args::Position::Query { .. } => render::Reading::Whole,
                _ => render::Reading::SAMPLED,
            };
            render::resolve(saved, &query.field, reading, cancellation)?
        }
    };
    match resolved {
        None => reader::page(None, &query.text(), closed, cancellation).map(OutputPreview::Lines),
        Some(render::Resolved::Text(text)) => match (&query.position, query.lines()) {
            (args::Position::Query { .. }, _) => Err(ToolError::invalid_arguments(
                "field is text: query applies to a JSON object or array",
            )),
            (_, Some(selection)) => {
                let source = text.source(cancellation)?;
                reader::page(Some(source), &selection, closed, cancellation)
                    .map(OutputPreview::Lines)
            }
            (_, None) => Err(ToolError::invalid_arguments(
                "field is text: page it with start and offset, not index",
            )),
        },
        Some(render::Resolved::Json {
            mut json,
            container,
        }) => match (&query.position, query.index()) {
            (args::Position::Query { path, index }, _) => query::page(
                &mut json,
                &query.field,
                path,
                *index,
                query.limit,
                cancellation,
            ),
            (_, Some(index)) => elements::page(
                &mut json,
                container,
                &query.field,
                index,
                query.limit,
                cancellation,
            ),
            (_, None) => Err(ToolError::invalid_arguments(format!(
                "field is a JSON {}: page its {} with index, not start, offset or pattern",
                container.name(),
                container.unit(),
            ))),
        },
    }
}

/// The pages a waiting job's question batches are read from.
fn questions() -> FieldPointer {
    FieldPointer::root().property("questions")
}

/// Whether a presented `result` holds a value, `null` included, at `field`.
fn shows(result: Option<&Value>, field: &FieldPointer) -> bool {
    let root = FieldPointer::result();
    match result {
        Some(_) if *field == root => true,
        Some(result) if root.contains(field) => result
            .pointer(&field.as_str()[root.as_str().len()..])
            .is_some(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{CancellationToken, JobOutcome, JobRole, JobSpec, tests::runtime};

    /// A running fixture job, finished with `value` when one is given.
    pub(super) async fn fixture(value: Option<Value>) -> (tempfile::TempDir, JobManager, JobId) {
        let (root, manager, agent) = runtime().await;
        let id = manager
            .test_running(JobSpec::test(agent, "fixture"))
            .await
            .into_test_id();
        if let Some(value) = value {
            manager.test_finish(id, value).await;
        }
        (root, manager, id)
    }

    fn field_args(id: JobId, field: &str) -> OutputArgs {
        let mut args = OutputArgs::new(id);
        args.field = Some(field.parse().unwrap());
        args
    }

    async fn host(manager: &JobManager, args: OutputArgs) -> Value {
        manager
            .inspect_output(args, CancellationToken::new(), &Default::default())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn script_returned_json_is_not_replaced_by_child_output() {
        let (_root, manager, agent) = runtime().await;
        let mut script = JobSpec::test(agent.clone(), "script");
        script.role = JobRole::Script;
        let script = manager.test_running(script).await.into_test_id();

        let mut child = JobSpec::test(agent, "read");
        child.parent = Some(script);
        let child = manager.test_running(child).await.into_test_id();
        manager.test_finish(child, json!({"child":"native"})).await;

        let returned = json!({"kind":"script-value", "child_id":child.get()});
        manager
            .test_finish(script, json!({"value":returned.clone()}))
            .await;

        let view = host(&manager, OutputArgs::new(script)).await;
        assert_eq!(view["result"]["value"], returned);
    }

    async fn model(manager: &JobManager, args: OutputArgs) -> Value {
        manager
            .present_output(args, &Default::default())
            .await
            .unwrap()
    }

    fn incomplete(field: &str) -> Value {
        json!([{ "field":field, "complete":false }])
    }

    /// Any explicit selector, including an explicitly supplied default, also
    /// suppresses automatic capture hydration.
    #[tokio::test]
    async fn output_product_owns_image_eligibility_and_typed_page() {
        use crate::{
            media::{BlobRef, ImageFormat, ImageRef},
            tool::ToolOutput,
        };
        let (_root, manager, id) = fixture(None).await;
        let blob: BlobRef = manager.store().store_blob(b"image").await.unwrap();
        let images = ["first", "second"]
            .map(|name| ImageRef {
                file: Some(name.into()),
                format: ImageFormat::Png,
                blob,
            })
            .to_vec();
        let output = ToolOutput::new(json!({"text": "first\nsecond"})).with_images(images.clone());
        manager
            .finish(id, JobOutcome::Completed(output))
            .await
            .unwrap();
        manager
            .output(id)
            .test_capture("/result/raw", CaptureKind::Text, b"line");
        let whole = manager
            .present_output_with(
                OutputArgs::new(id),
                Default::default(),
                &Default::default(),
                OutputOptions::Host,
            )
            .await
            .unwrap();
        let (state, view, attached) = whole.into_parts();
        assert_eq!((state, &attached), (JobState::Completed, &images));
        assert_eq!(
            OutputArgs::new(id).selection(),
            OutputSelection::WholeWithImages
        );
        assert!(view["presentation"]["preview"].is_null());
        for field in [
            "field", "start", "limit", "pattern", "context", "offset", "index",
        ] {
            let mut args = OutputArgs::new(id);
            // Text selectors page the text field; the result itself pages by index.
            let text = Some("/result/text".parse().unwrap());
            match field {
                "field" => args.field = text,
                "start" => (args.field, args.start) = (text, Some(1)),
                "limit" => args.limit = Some(100),
                "pattern" => (args.field, args.pattern) = (text, Some(String::new())),
                "context" => args.context = Some(0),
                "offset" => (args.field, args.offset) = (text, Some(0)),
                "index" => args.index = Some(0),
                _ => unreachable!(),
            }
            let selection = args.selection();
            let (_, view, attached) = manager
                .inspect_output_with_captures(args, CancellationToken::new(), &Default::default())
                .await
                .unwrap()
                .into_parts();
            assert!(attached.is_empty(), "explicit {field} attached images");
            // `context` without a pattern selects nothing: the whole result, without images.
            let whole = field == "context";
            let expected = if whole {
                OutputSelection::Whole
            } else {
                OutputSelection::Explicit
            };
            assert_eq!(selection, expected);
            assert_eq!(
                view["presentation"]["preview"].is_object(),
                !whole,
                "{field}"
            );
            let captures = view["presentation"]["captures"].as_array().unwrap();
            assert_eq!(captures.len(), 1);
            assert!(captures[0]["output"].is_null());
        }
        assert_eq!(manager.images(id).await.unwrap(), images);
    }

    #[tokio::test]
    async fn presentation_preserves_nulls_defaults_and_saved_result_presence() {
        for raw in [
            Value::Null,
            json!({
                "error": null,
                "nested": {"absent": null, "ok": false},
                "array": [null, {"absent": null, "count": 0}]
            }),
        ] {
            let (_root, manager, id) = fixture(Some(raw.clone())).await;
            for view in [
                model(&manager, OutputArgs::new(id)).await,
                host(&manager, OutputArgs::new(id)).await,
            ] {
                assert_eq!(view.get("result"), Some(&raw));
                assert_eq!(view.get("presentation"), None);
            }
            assert!(host(&manager, OutputArgs::new(id)).await["meta"].is_object());
            assert_eq!(manager.output(id).test_document().unwrap()["result"], raw);
            let metadata = manager
                .metadata(id)
                .await
                .unwrap()
                .metadata_view(&Default::default())
                .into_value();
            assert_eq!(metadata.get("result"), None);
        }
    }

    /// Unfinished captures, whether abandoned by a successful result, a failure,
    /// or cancellation, remain discoverable, readable, and marked incomplete.
    #[tokio::test]
    async fn incomplete_captures_remain_discoverable_and_readable() {
        for outcome in ["failed", "cancelled", "unreferenced"] {
            let (_root, manager, id) = fixture(None).await;
            let field = if outcome == "unreferenced" {
                "/result/abandoned"
            } else {
                "/result/events~1custom/nested~0key"
            };
            assert!(
                host(&manager, OutputArgs::new(id)).await["presentation"]["captures"].is_null()
            );
            let bytes = "{\"partial\":"; // Deliberately unfinished JSON.
            let output = manager.output(id);
            output.test_capture(field, CaptureKind::Json, bytes.as_bytes());
            for terminal in [false, true] {
                if terminal {
                    let outcome = match outcome {
                        "cancelled" => ToolError::cancelled().into(),
                        "failed" => ToolError::failed("injected failure").into(),
                        _ => JobOutcome::Completed(crate::tool::ToolOutput::new(
                            json!({"abandoned":null}),
                        )),
                    };
                    manager.finish(id, outcome).await.unwrap();
                }
                let view = host(&manager, OutputArgs::new(id)).await;
                let page = host(&manager, field_args(id, field)).await;
                assert_eq!(view["presentation"]["captures"], incomplete(field));
                assert_eq!(
                    page["presentation"]["preview"]["lines"],
                    json!([bytes]),
                    "{outcome}"
                );
                if outcome == "unreferenced" && terminal {
                    assert_eq!(view["presentation"]["notice"], "Output incomplete.");
                    assert_eq!(page["presentation"]["notice"], "Output incomplete.");
                } else {
                    assert!(view["result"].is_null(), "{outcome}");
                }
            }
            assert_eq!(output.test_bytes(field).unwrap(), bytes.as_bytes());
        }
    }

    #[tokio::test]
    async fn persisted_presence_distinguishes_null_results_from_missing_failure_output() {
        for (state, available) in [
            (JobState::Completed, true),
            (JobState::Failed, true),
            (JobState::Failed, false),
            (JobState::Cancelled, false),
        ] {
            let output = available.then(|| crate::tool::ToolOutput::new(Value::Null));
            let outcome = match state {
                JobState::Completed => JobOutcome::Completed(output.unwrap()),
                JobState::Failed => JobOutcome::Failed {
                    diagnostic: ToolError::failed("failure").into_facts().0,
                    output,
                },
                _ => ToolError::cancelled().into(),
            };
            let (_root, manager, id) = fixture(None).await;
            manager.finish(id, outcome).await.unwrap();
            let view = model(&manager, OutputArgs::new(id)).await;
            assert_eq!(view.get("result"), available.then_some(&Value::Null));
            // Option<Value> in older journal decoding cannot retain Some(null).
            // Hydration must recover availability from the saved presence bit.
            let mut envelope = manager.snapshot(id).await.unwrap();
            envelope.output = None;
            manager.hydrate_envelope(&mut envelope).await.unwrap();
            assert_eq!(envelope.output.is_some(), available);
        }
    }

    #[tokio::test]
    async fn script_console_capture_can_be_paged_while_running_and_after_cancellation() {
        let (_root, manager, agent) = runtime().await;
        let mut spec = JobSpec::test(agent, "script");
        spec.output_schema = Some(json!({"type":"object","properties":{
            "value":{}, "console":{"type":"string"}
        }}));
        let id = manager.test_running(spec).await.into_test_id();
        let console = "console\n".repeat(150);
        manager
            .output(id)
            .test_capture("/result/console", CaptureKind::Text, console.as_bytes());
        let mut query = field_args(id, "/result/console");
        query.start = Some(101);
        let lines = |page: &Value| {
            page["presentation"]["preview"]["lines"]
                .as_array()
                .unwrap()
                .len()
        };
        assert_eq!(lines(&model(&manager, query.clone()).await), 50);
        manager
            .finish(id, ToolError::cancelled().into())
            .await
            .unwrap();
        let view = model(&manager, OutputArgs::new(id)).await;
        assert_eq!(view.get("result"), None);
        assert_eq!(
            view["presentation"]["captures"],
            incomplete("/result/console")
        );
        assert_eq!(view["presentation"]["notice"], "Output incomplete.");
        assert_eq!(lines(&model(&manager, query).await), 50);
    }

    #[tokio::test]
    async fn capture_completeness_is_internal_and_only_partial_finished_jobs_have_a_notice() {
        for complete in [true, false] {
            let (_root, manager, job) = fixture(None).await;
            let mut output = crate::tool::ToolOutput::new(json!({"stdout":"line\n".repeat(120)}));
            if !complete {
                output.streams = crate::tool::StreamEnd::Cut;
            }
            manager
                .finish(job, JobOutcome::Completed(output))
                .await
                .unwrap();
            for field in [None, Some("/result/stdout"), Some("")] {
                let mut query = OutputArgs::new(job);
                query.field = field.map(|field| field.parse().unwrap());
                let view = model(&manager, query).await;
                assert!(!view.to_string().contains("capture_complete"));
                if complete {
                    assert!(view["presentation"]["notice"].is_null());
                } else {
                    assert_eq!(view["presentation"]["notice"], "Output incomplete.");
                }
            }
        }
    }

    /// A producer can offload the diagnostic slot itself, not only an ancestor.
    /// Presenting the diagnostic replaces a long slot stored on its own too, so a
    /// whole-result read never chases the capture it replaced.
    #[tokio::test]
    async fn presented_diagnostic_replaces_a_stored_slot() {
        let message = "m".repeat(5000);
        let (_root, manager, id) = fixture(Some(json!({"error":{"message": message}}))).await;
        let output = manager.output(id);
        assert_eq!(output.test_fields(), [diagnostic_slot()]);
        let mut saved = Saved::load(&output).unwrap();
        let diagnostic = ToolError::failed("boom").diagnostic();
        let capabilities = CapabilitySet::default();
        let viewer = DiagnosticViewer::from(&capabilities);
        saved
            .present_diagnostics(Some(&diagnostic), viewer)
            .unwrap();
        assert!(saved.fields.is_empty());
        let document = saved.product.as_ref().unwrap().document();
        let root = FieldPointer::root();
        let mut source =
            render::json_text(&saved, &root, &document, render::Reading::Whole).unwrap();
        let mut whole = String::new();
        source.read_to_string(&mut whole).unwrap();
        assert!(whole.contains("boom") && !whole.contains("mmmm"), "{whole}");
    }

    #[tokio::test]
    async fn unicode_continuations_fit_the_complete_job_envelope() {
        // Exhaustive reconstruction and replay live in reader tests. Here retain
        // the integration boundary: correct offsets and the entire view budget.
        let expected = format!("{}\nlast", "🦀\"\\".repeat(CONTENT_BYTES));
        let (_root, manager, id) = fixture(Some(json!({"content":expected}))).await;
        let mut args = field_args(id, "/result/content");
        for _ in 0..2 {
            let offset = args.offset.unwrap_or(0);
            let view = model(&manager, args.clone()).await;
            assert!(serde_json::to_vec(&view).unwrap().len() <= PAGE_BYTES);
            let page = &view["presentation"]["preview"];
            assert_eq!(page["next_start"], 1);
            let next = page["next_offset"].as_u64().unwrap() as usize;
            assert!(next > offset);
            assert_eq!(page["lines"], json!([&expected[offset..next]]));
            args.start = Some(1);
            args.offset = Some(next);
        }

        // The whole document pages as members, streamed from its captures, so a
        // presented diagnostic is each viewer's own and nothing shared is saved.
        let output = manager.output(id);
        let mut saved = Saved::load(&output).unwrap();
        saved.diagnostic_fields.insert(diagnostic_slot());
        let root = FieldPointer::root();
        for message in [
            "privileged viewer-rendering",
            "restricted viewer-rendering",
            "privileged viewer-rendering",
        ] {
            let result = saved.product.as_mut().unwrap().result.as_mut().unwrap();
            result["error"] = json!({"message": message});
            let query = OutputArgs::new(id).admit().unwrap();
            let query = args::Query {
                field: root.clone(),
                ..query
            };
            let page = page(&saved, &query, None, true, &CancellationToken::default()).unwrap();
            let OutputPreview::Members(page) = page else {
                panic!("{page:?}")
            };
            assert_eq!(page.members["result"]["error"]["message"], message);
        }
        assert_eq!(output.db.rendering(id.get(), &root).unwrap(), None);
    }

    /// Selectors follow the selected field's unit, queries apply to JSON, and a
    /// field inside a stored container is streamed to rather than loaded with it.
    #[tokio::test]
    async fn selectors_follow_the_field_unit_and_reach_into_stored_json() {
        let items: Vec<_> = (0..400)
            .map(|id| json!({"id": id, "name": format!("pod-{id}")}))
            .collect();
        let (_root, manager, id) = fixture(Some(json!({"items": items, "log": "a\nb"}))).await;
        // The whole result is one stored JSON container.
        assert_eq!(manager.output(id).test_fields(), [FieldPointer::result()]);
        let page = |field: &str, view: Value| {
            assert!(view["presentation"]["preview"]["field"] == field, "{view}");
            view["presentation"]["preview"].clone()
        };
        let name = "/result/items/250/name";
        let preview = page(name, host(&manager, field_args(id, name)).await);
        assert_eq!(preview["lines"], json!(["pod-250"]));
        let item = "/result/items/250";
        let preview = page(item, host(&manager, field_args(id, item)).await);
        assert_eq!(preview["members"], items[250]);
        let mut args = field_args(id, "/result/items");
        args.query = Some("$[?@.id > 397].name".into());
        let preview = page("/result/items", host(&manager, args).await);
        let at: Vec<_> = (preview["matches"].as_array().unwrap().iter())
            .map(|found| found["at"].as_str().unwrap())
            .collect();
        assert_eq!(at, ["/result/items/398/name", "/result/items/399/name"]);
        for (field, query, expected) in [
            ("/result/log", "$", "query applies to a JSON"),
            ("/result/items", "$[", "not a JSONPath"),
        ] {
            let mut args = field_args(id, field);
            args.query = Some(query.into());
            let error = manager
                .inspect_output(args, CancellationToken::new(), &Default::default())
                .await
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }
        // Selectors restating their defaults select nothing.
        let mut args = field_args(id, "/result/items");
        (args.index, args.start, args.offset, args.context) =
            (Some(399), Some(1), Some(0), Some(0));
        let preview = page("/result/items", host(&manager, args).await);
        assert_eq!(preview["elements"], json!([items[399]]));
        let mut args = field_args(id, "/result/log");
        (args.index, args.start) = (Some(0), Some(2));
        let preview = page("/result/log", host(&manager, args).await);
        assert_eq!(preview["lines"], json!(["b"]));
        for (field, text, expected) in [
            ("/result/items", true, "field is a JSON array"),
            ("/result/log", false, "field is text"),
        ] {
            let mut args = field_args(id, field);
            if text {
                args.start = Some(2);
            } else {
                args.index = Some(1);
            }
            let error = manager
                .inspect_output(args, CancellationToken::new(), &Default::default())
                .await
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[tokio::test]
    async fn json_selectors_wait_for_the_saved_result() {
        let (_root, manager, id) = fixture(None).await;
        let mut args = field_args(id, "/result");
        args.index = Some(1);
        let error = manager
            .inspect_output(args, CancellationToken::new(), &Default::default())
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("when the job finishes"),
            "{error}"
        );
    }

    /// A stored string is read only as far as a page can show it, which is more
    /// than a preview's text field keeps.
    #[tokio::test]
    async fn pages_show_stored_strings_within_the_page_budget() {
        let result = json!({"log": "x".repeat(20_000), "code": 1});
        let (_root, manager, id) = fixture(Some(json!(result))).await;
        let view = host(&manager, field_args(id, "/result")).await;
        let preview = &view["presentation"]["preview"];
        assert_eq!(preview["members"], result);
        assert!(preview["truncated"].is_null());
    }

    #[tokio::test]
    async fn search_reaches_beyond_preview_and_merges_context_across_pages() {
        let text: String = (1..=500)
            .map(|n| {
                format!(
                    "{} {n}\n",
                    if n == 450 || n == 452 { "ERROR" } else { "ok" }
                )
            })
            .collect();
        let (_root, manager, id) = fixture(Some(json!({"stdout":text}))).await;
        let (mut start, mut offset, mut found) = (None, None, Vec::new());
        loop {
            let mut args = field_args(id, "/result/stdout");
            (args.start, args.offset) = (start, offset);
            (args.pattern, args.context, args.limit) = (Some("(?i)error".into()), Some(2), Some(2));
            let view = model(&manager, args).await;
            let preview = &view["presentation"]["preview"];
            // The page is of the requested field, so it does not repeat it.
            assert_eq!(preview.get("field"), None);
            found.extend(preview["lines"].as_array().unwrap().iter().cloned());
            let Some(next) = preview["next_start"].as_u64() else {
                break;
            };
            start = Some(next as usize);
            offset = preview["next_offset"]
                .as_u64()
                .map(|offset| offset as usize);
        }
        let expected: Vec<_> = (448..=454)
            .zip(text.lines().skip(447))
            .map(|(line, text)| json!({"line": line, "text": text}))
            .collect();
        assert_eq!(found, expected);
    }

    // Automatic capture assembly contracts; capture ownership lives in captures/.

    fn capture(manager: &JobManager, id: JobId, field: &str, kind: CaptureKind, bytes: &[u8]) {
        manager.output(id).test_capture(field, kind, bytes);
    }

    async fn hydrated(manager: &JobManager, job: JobId) -> (JobState, Value) {
        let args = OutputArgs::new(job);
        let (state, view, _) = manager
            .inspect_output_with_captures(args, CancellationToken::new(), &Default::default())
            .await
            .unwrap()
            .into_parts();
        let schema = &crate::job::JOB_VIEW_SCHEMAS.one;
        assert!(jsonschema::is_valid(schema, &view), "{view}");
        (state, view)
    }

    #[tokio::test]
    async fn automatic_hydration_reads_live_and_terminal_captures_in_descriptor_order() {
        let (_root, manager, job) = fixture(None).await;
        // More than the concurrency bound, with raw/incomplete JSON and escaped pointers.
        let fields: Vec<_> = (0..7)
            .map(|i| format!("/result/events~1custom/{i}~0key"))
            .collect();
        for terminal in [false, true] {
            let bytes: &[u8] = if terminal {
                b"{\"partial\":\n42"
            } else {
                b"{\"partial\":"
            };
            for field in &fields {
                capture(&manager, job, field, CaptureKind::Json, bytes);
            }
            if terminal {
                manager.test_finish(job, json!({"status":"done"})).await;
            }
            let (state, view) = hydrated(&manager, job).await;
            assert_eq!(state.is_terminal(), terminal);
            let captures = view["presentation"]["captures"].as_array().unwrap();
            assert_eq!(captures.len(), fields.len());
            for (capture, field) in captures.iter().zip(&fields) {
                assert_eq!(
                    (&capture["field"], &capture["complete"]),
                    (&json!(field), &json!(false))
                );
                let preview = &capture["output"]["presentation"]["preview"];
                assert_eq!(
                    (&preview["field"], &preview["lines"][0]),
                    (&json!(field), &json!("{\"partial\":"))
                );
                assert_eq!(
                    preview["lines"].as_array().unwrap().len(),
                    if terminal { 2 } else { 1 }
                );
                // Capture pages are not recursively hydrated.
                for nested in capture["output"]["presentation"]["captures"]
                    .as_array()
                    .unwrap()
                {
                    assert!(nested["output"].is_null());
                }
            }
        }
    }

    #[tokio::test]
    async fn automatic_hydration_distinguishes_absent_from_present_and_null() {
        let (_root, manager, job) = fixture(None).await;
        for field in ["/result/absent", "/result/null", "/result/present"] {
            capture(&manager, job, field, CaptureKind::Text, b"raw capture");
        }
        manager
            .test_finish(job, json!({"null":null,"present":"structured value"}))
            .await;
        let (_, view) = hydrated(&manager, job).await;
        assert_eq!(view["result"]["present"], "structured value");
        // An explicit null remains present without turning into permission to
        // hydrate an abandoned capture at the same pointer.
        assert!(view.pointer("/result/null").unwrap().is_null());
        for capture in view["presentation"]["captures"].as_array().unwrap() {
            assert_eq!(
                !capture["output"].is_null(),
                capture["field"] == "/result/absent"
            );
        }
    }

    #[tokio::test]
    async fn automatic_hydration_retains_nested_page_continuations() {
        let (_root, manager, job) = fixture(None).await;
        capture(
            &manager,
            job,
            "/result/log",
            CaptureKind::Text,
            "line\n".repeat(150).as_bytes(),
        );
        let (_, view) = hydrated(&manager, job).await;
        // The live whole-result page is empty but retains its polling position;
        // the capture has an independent source-page continuation.
        assert_eq!(
            view["presentation"]["preview"],
            json!({"field": "/result", "lines": [], "next_start": 1})
        );
        let preview = &view["presentation"]["captures"][0]["output"]["presentation"]["preview"];
        assert_eq!(preview["field"], "/result/log");
        assert_eq!(preview["lines"].as_array().unwrap().len(), 100);
        assert_eq!(preview["next_start"], 101);
        assert_eq!(preview.get("next_offset"), None);
    }

    #[tokio::test]
    async fn automatic_hydration_embeds_page_errors_but_propagates_initial_errors() {
        let (_root, manager, job) = fixture(None).await;
        // Discovery succeeds, but this page cannot decode its capture.
        capture(&manager, job, "/result/bad", CaptureKind::Text, b"\xffbad");
        capture(&manager, job, "/result/good", CaptureKind::Text, b"good");
        let (_, view) = hydrated(&manager, job).await;
        let captures = &view["presentation"]["captures"];
        assert!(!captures[0]["output"]["error"].as_str().unwrap().is_empty());
        assert_eq!(
            captures[1]["output"]["presentation"]["preview"]["lines"],
            json!(["good"])
        );
        let mut invalid = OutputArgs::new(job);
        invalid.start = Some(0);
        let result = manager
            .inspect_output_with_captures(invalid, CancellationToken::new(), &Default::default())
            .await;
        assert!(matches!(
            result,
            Err(error) if matches!(error.diagnostic().cause, crate::tool::diagnostic::Cause::InvalidArguments(_))
        ));
        let unknown = OutputArgs::new(JobId::new(job.get() + 100).unwrap());
        assert!(
            manager
                .inspect_output_with_captures(
                    unknown,
                    CancellationToken::new(),
                    &Default::default()
                )
                .await
                .is_err()
        );
    }

    /// Images a job produced are retrieved with its whole output, directly or
    /// through a script, and survive a resume.
    mod images {
        use super::*;
        use crate::{
            identity::AgentId,
            session::SessionStore,
            tests::{IMAGE, TestRuntime, assert_loaded, image_runtime, script, tool_executor},
            tool::{
                ToolError, ToolOutput,
                policy::{Capability, CapabilitySet},
            },
        };
        use serde_json::json;

        #[tokio::test]
        async fn image_output_text_selections_do_not_attach_and_invalid_queries_fail() {
            let (runtime, executor, _slot) = image_runtime().await;
            let agent = &runtime.agent;
            let read = executor
                .run_model(agent, "read", json!({"path":"image.png"}))
                .await
                .unwrap();
            assert_loaded(&runtime.store, read.output.images).await;
            // Selection semantics are tested with the output product; this proves
            // both entry points forward the selector.
            let query = json!({"job":read.job, "field":"/result"});
            let output = executor
                .run_model(agent, "jobs", query.clone())
                .await
                .unwrap();
            assert!(output.output.images.is_empty());
            let source = format!("return await tool.jobs({query});");
            let output = script(&executor, agent, source, false).await;
            assert!(output.output.images.is_empty());
            for query in [
                json!({"job":read.job,"limit":0}),
                json!({"job":read.job,"pattern":"["}),
                json!({"job":999999}),
            ] {
                let response = executor.run_model(agent, "jobs", query).await.unwrap();
                assert_eq!(response.output.value["state"], "failed");
            }
            // A denied image-producing call never creates a retrievable attachment.
            let mut capabilities = CapabilitySet::default();
            capabilities.remove(Capability::Read);
            let denied = executor.clone().with_capabilities(capabilities);
            let before = runtime.jobs.list(agent).await.len();
            let arguments = json!({"path":"image.png"});
            // This is a pre-admission error: public dispatch converts it to an
            // id:null failure envelope, while the internal executor returns Err.
            assert!(denied.run_model(agent, "read", arguments).await.is_err());
            for job in runtime.jobs.list(agent).await.into_iter().skip(before) {
                assert!(runtime.jobs.images(job.id).await.unwrap().is_empty());
            }
        }

        #[tokio::test]
        async fn retrieved_images_survive_resume_including_failed_tool_output() {
            let runtime = TestRuntime::on_disk().await;
            let image = crate::media::Image::new(IMAGE.to_vec()).unwrap();
            let image = runtime.store.store_image(Some("image.png".into()), &image);
            let image = image.await.unwrap();
            let spec = JobSpec::test(runtime.agent.clone(), "partial-image");
            let lease = runtime.jobs.create(spec).await.unwrap();
            let job = lease.id();
            // Consume the lease before reopening; it retains the manager and journal lock.
            let output = ToolOutput::new(json!({"image":image})).with_images(vec![image]);
            lease
                .fail(
                    ToolError::failed("failed after producing an image")
                        .with_result(output)
                        .into(),
                )
                .await;
            let id = runtime.store.id();
            drop((runtime.jobs, runtime.store));
            let sessions = runtime.root.path().join("sessions");
            let (store, records) = SessionStore::open(&sessions, id).await.unwrap();
            let jobs = JobManager::restore(store.clone(), &records).await.unwrap();
            let agent = AgentId::root(id);
            let (executor, _slot) = tool_executor(jobs, runtime.root.path());
            let output = executor.run_model(&agent, "jobs", json!({"job":job}));
            let output = output.await.unwrap().output;
            assert_eq!(output.value["state"], "failed");
            assert_loaded(&store, output.images).await;
            let source = format!("return await tool.jobs({{job:{job}}});");
            let output = script(&executor, &agent, source, false).await;
            assert_loaded(&store, output.output.images).await;
        }
    }
}
