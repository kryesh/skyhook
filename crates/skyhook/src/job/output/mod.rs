//! Saved output presentation, independent of model providers and execution transports.
mod args;
mod captures;
mod elements;
mod files;
mod finalization;
mod json;
mod preview;
mod products;
mod projection;
mod query;
mod reader;
mod saved;
mod shape;
mod stream;
use args::Selection;
pub(crate) use args::{DEFAULT_LIMIT, MAX_CONTEXT, Selectors, count};
pub use args::{OutputArgs, PageLimit, PageUnit};
#[cfg(test)]
pub(crate) use captures::create_capture;
pub use captures::{CaptureDescriptor, FieldFile};
pub(crate) use captures::{CompletedCapture, HostOutput};
pub(crate) use files::{HeldFile, Reach, TargetFiles};
pub(crate) use finalization::prepare_completed;
pub use products::{
    Continuation, ElementPage, LinePage, Match, MatchPage, MemberPage, NumberedLine, OutputPreview,
    OutputSelection, OutputTruncation, PageLines, PresentedOutput,
};
pub(crate) use projection::{fill_absent, presented_fields};
use reader::Source;
use saved::{Held, Stored, hydrate, prepare_document};
pub(crate) use saved::{Output, Saved, presented_size};

use super::{JobError, JobManager, ViewKind, views};

use crate::tool::policy::CapabilitySet;
use crate::{
    session::{CaptureRow, OutputRows, SharedDb, StagedCapture},
    tool::ToolError,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use skyhook_tools::{
    diagnostic::DiagnosticViewer,
    output::{CaptureKind, FieldPointer, Presented},
};
use skyhook_worker::job::JobId;
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
/// The most lines host inspection shows of a running job's capture: its latest.
const LIVE_LINES: PageLimit = PageLimit::new::<20>();
/// The bytes of a stream's end a target reports with a stream it kept in a file: what
/// a preview of both ends shows of them.
pub(crate) const STREAM_TAIL_BYTES: u64 = preview::TEXT_BYTES as u64;
/// How often at most a target tells of the latest of a stream it is keeping.
pub(crate) const STREAM_LATEST_EVERY: std::time::Duration = std::time::Duration::from_millis(250);

/// Where the session host keeps saved output that is too large for its database: its
/// spill directory, when its user has a cache directory, and the bytes of one field
/// past which a file holds it.
#[derive(Clone, Debug)]
pub(crate) struct Spill {
    pub root: Option<std::path::PathBuf>,
    pub bytes: u64,
}

impl Default for Spill {
    fn default() -> Self {
        Self {
            root: skyhook_worker::fs::spill::root(),
            bytes: skyhook_base::limits::SAVED_FIELD_BYTES,
        }
    }
}

/// Who reads a presentation. A model-facing read presents the job's pending
/// notification, for its reader to acknowledge; host inspection never does and
/// always shows details.
#[derive(Clone, Copy)]
pub(crate) enum OutputOptions<'a> {
    Host,
    /// Job `reader`, when a job reads it, holds what it presents from the read on.
    /// What the job's target holds of its output is read there through `reach`, by
    /// a call that has one.
    Model {
        kind: ViewKind,
        reader: Option<super::JobId>,
        fit: Fit,
        reach: Option<&'a dyn Reach>,
    },
}

/// How much of a finished result a whole read presents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fit {
    /// Within the presentation budget, as a model reads it.
    Preview,
    /// All of it, as a script holds it: what a script returns is presented again.
    Whole,
}

impl JobManager {
    /// The output rows of job `id`'s current run.
    pub(crate) async fn output(&self, id: JobId) -> Output {
        let invocation = self.entry(id, |entry| entry.invocation).await;
        let invocation = invocation.unwrap_or_default();
        self.run_output(id, invocation)
    }

    #[cfg(test)]
    pub(crate) async fn present_output(
        &self,
        args: OutputArgs,
        capabilities: &CapabilitySet,
    ) -> Result<Value, ToolError> {
        let presented = self.present_output_with(
            args,
            super::CancellationToken::new(),
            capabilities,
            OutputOptions::Model {
                kind: ViewKind::Inspection,
                reader: None,
                fit: Fit::Preview,
                reach: None,
            },
        );
        let presented = presented.await?;
        self.acknowledge(presented.acknowledges).await?;
        Ok(presented.into_view())
    }

    #[cfg(test)]
    pub(crate) async fn inspect_output(
        &self,
        args: OutputArgs,
        cancellation: super::CancellationToken,
        capabilities: &CapabilitySet,
    ) -> Result<Value, ToolError> {
        self.present_output_with(args, cancellation, capabilities, OutputOptions::Host)
            .await
            .map(PresentedOutput::into_view)
    }

    /// Host inspection with automatic pages for captures absent from the whole
    /// presentation: a running job's from their last page, so that repeated
    /// inspection follows them. Any explicit selection (including `context: 0`)
    /// suppresses hydration, and a present JSON null is not an absent capture. A
    /// page failure is embedded as `{error}` in that capture's `output`.
    ///
    /// Host inspection never acknowledges an agent's pending notification.
    /// Stored captures are listed as `captures: [{field, complete}]`; an
    /// incomplete capture is exposed only as raw pages, never as a result.
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
        let live = !output.state.is_terminal();
        let mut pages = stream::iter(std::mem::take(&mut output.capture_targets))
            .map(|target| {
                let mut query = OutputArgs::new(args.job);
                query.field = Some(target.field);
                let cancellation = cancellation.clone();
                let reads = self.store().reads();
                async move {
                    let page = async {
                        if live {
                            let held = reads.clone();
                            let latest = self.store().output_changes().following();
                            let start = move || {
                                let reader = reader::CaptureReader::new(reads, target.capture)
                                    .following(latest.get(&target.capture).cloned());
                                reader::last_page_start(reader, args::count(LIVE_LINES))
                            };
                            query.start = Some(held.hold(start).await?);
                            query.unit = Some(PageUnit::Lines);
                            query.limit = Some(LIVE_LINES);
                        }
                        self.present_output_with(
                            query,
                            cancellation,
                            capabilities,
                            OutputOptions::Host,
                        )
                        .await
                    };
                    (target.index, page.await)
                }
            })
            .buffer_unordered(HYDRATED_PAGES);
        while let Some((index, page)) = pages.next().await {
            output.attach_capture(index, page);
        }
        Ok(output)
    }

    /// Return the state and presentation from the same job snapshot. Explicit
    /// field/page queries always retain full output.
    pub(crate) async fn present_output_with<'a>(
        &self,
        args: OutputArgs,
        cancellation: super::CancellationToken,
        viewer: impl Into<DiagnosticViewer<'a>>,
        options: OutputOptions<'_>,
    ) -> Result<PresentedOutput, ToolError> {
        let viewer = viewer.into();
        let (model, kind, reader, fit, reach) = match options {
            OutputOptions::Host => (false, ViewKind::Inspection, None, Fit::Preview, None),
            OutputOptions::Model {
                kind,
                reader,
                fit,
                reach,
            } => (true, kind, reader, fit, reach),
        };
        let query = args.admit()?;
        let output_selection = args.selection();
        let explicit = output_selection == OutputSelection::Explicit;
        let job = args.job;
        let envelope = self.presenting(job, reader, move |entry| {
            (
                entry.envelope(job),
                match entry.finished() {
                    Some(finished) if output_selection == OutputSelection::WholeWithImages => {
                        finished.images.clone()
                    }
                    _ => Vec::new(),
                },
                entry.asking().map(super::Asking::output),
            )
        });
        let ((envelope, images, asked), delivery) = envelope.await?;
        let terminal = envelope.state.is_terminal();
        let question = asked.is_some();
        let kind = if explicit { ViewKind::Inspection } else { kind };
        let mut result = None;
        let mut annotations = views::Presentation::default();
        // The run the envelope shows: a later run's output never presents it.
        let output = self.run_output(args.job, envelope.invocation);
        let wanted = Wanted {
            query: query.clone(),
            explicit,
            terminal,
            question,
            fit,
            reaches: reach.is_some(),
        };
        // What the job's target holds of the output is read there with the session's
        // readers let go: a page of one field as it is asked for, and fields a read
        // needs whole before it reads on with them held.
        let (ran, mut texts) = (&envelope.location, Vec::new());
        let (saved, shown) = loop {
            let (rows, wanted, cancelled) = (output.clone(), wanted.clone(), cancellation.clone());
            let show = move || {
                let saved = Saved::load(&rows)?.holding(texts);
                saved.reading(|saved| wanted.clone().show(saved, &cancelled))
            };
            match output.reads.hold(show).await? {
                (_, Shown::Held(held, limit)) => {
                    texts = files::texts(reach, ran, held, limit, &cancellation).await?;
                }
                (saved, Shown::File(file)) => {
                    let page = files::page(reach, ran, file, query.text(), terminal, &cancellation);
                    break (saved, Shown::Page(OutputPreview::Lines(page.await?)));
                }
                shown => break shown,
            }
        };
        // A whole result holds raw data, to show as its saved fields are shown;
        // anything else was fitted already.
        let presented = match &shown {
            Shown::Whole(_) => saved.presented.clone(),
            _ => Presented::presentation(FieldPointer::root()),
        };
        let mut captures = captures::available_captures(&saved, terminal);
        let incomplete_capture = terminal
            && captures.iter().any(|capture| {
                !capture.complete
                    && (!explicit
                        || capture.field == query.field
                        || query.field.contains(&capture.field))
            });
        match shown {
            Shown::Whole(whole) => result = whole,
            Shown::Projected(projected) => {
                result = projected.result;
                annotations.shape = projected.shape;
                annotations.truncated =
                    (!projected.truncated.is_empty()).then_some(projected.truncated);
                annotations.notice = projected.notice;
            }
            Shown::Question => annotations.question = asked,
            Shown::File(_) | Shown::Held(..) => unreachable!("read on the job's target above"),
            Shown::Page(mut page) => {
                if matches!(options, OutputOptions::Model { .. })
                    && args.field.as_ref() == page.field()
                {
                    page.clear_field();
                }
                annotations.preview = Some(page);
            }
        }
        // However it is shown, a finished job whose output was cut says so.
        let cut = (saved.product.as_ref()).is_some_and(|product| !product.captures_complete);
        if incomplete_capture || terminal && cut {
            annotations.notice = Some(views::Notice::OutputIncomplete);
        }
        let acknowledges =
            delivery.filter(|_| model && (terminal || annotations.question.is_some()));
        // A complete capture is simply a field of the result, read by pointer.
        captures.retain(|capture| !capture.complete);
        let capture_targets = captures
            .iter()
            .enumerate()
            .filter(|(_, capture)| {
                output_selection == OutputSelection::WholeWithImages
                    && !capture.complete
                    && !shows(result.as_ref(), &capture.field)
            })
            .filter_map(|(index, capture)| {
                Some(products::HydrationTarget {
                    index,
                    field: capture.field.clone(),
                    capture: saved.captures.get(&capture.field)?.id,
                })
            })
            .collect();
        annotations.captures = (!captures.is_empty()).then_some(captures);
        // Where a field's bytes are kept is the session's user's to see.
        let files = match options {
            OutputOptions::Host => captures::field_files(&saved, &envelope.location),
            OutputOptions::Model { .. } => Vec::new(),
        };
        Ok(PresentedOutput {
            files,
            state: envelope.state,
            view: envelope.view(viewer, kind, result, annotations),
            images,
            presented,
            capture_targets,
            acknowledges,
        })
    }

    /// The result `envelope`'s run saved, once it finished, with the
    /// presentations of its fields saved with it. The fields its target holds in
    /// files are read whole through `reach`, with the session's readers let go.
    pub(crate) async fn saved_result(
        &self,
        envelope: &super::JobEnvelope,
        reach: Option<&dyn Reach>,
    ) -> Result<Option<crate::tool::ToolOutput>, JobError> {
        if !envelope.state.is_terminal() {
            return Ok(None);
        }
        let output = self.run_output(envelope.id, envelope.invocation);
        let (root, cancellation) = (FieldPointer::root(), super::CancellationToken::new());
        let read = async {
            let mut texts = Vec::new();
            loop {
                let (rows, root) = (output.clone(), root.clone());
                let saved = move || {
                    let saved = Saved::load(&rows)?.holding(texts);
                    let held = saved.on_target(&root);
                    let whole = |saved: &Saved| held.is_empty().then(|| saved.result()).transpose();
                    let (saved, result) = saved.reading(whole)?;
                    Ok::<_, ToolError>((result, held, saved.presented))
                };
                let (result, held, presented) = output.reads.hold(saved).await?;
                let Some(result) = result else {
                    let ran = &envelope.location;
                    texts = files::texts(reach, ran, held, None, &cancellation).await?;
                    continue;
                };
                let output = |value| crate::tool::ToolOutput::new(value).with_presented(presented);
                return Ok(result.map(output));
            }
        };
        read.await
            .map_err(|error: ToolError| JobError::Output(Box::new(error)))
    }
}

/// What a read wants shown of a run's saved output.
#[derive(Clone)]
struct Wanted {
    query: args::Query,
    /// Whether the read selects what it shows itself.
    explicit: bool,
    /// Whether the job has finished.
    terminal: bool,
    /// Whether the job is asking a question.
    question: bool,
    fit: Fit,
    /// Whether the reader reaches what the job's target holds.
    reaches: bool,
}

impl Wanted {
    /// What to show of `saved`, or what the reader reads on the job's target first.
    fn show(
        mut self,
        saved: &Saved,
        cancellation: &super::CancellationToken,
    ) -> Result<Shown, ToolError> {
        let (explicit, terminal, question) = (self.explicit, self.terminal, self.question);
        if !explicit && terminal && saved.product.is_some() {
            let held = match self.fit {
                Fit::Whole => saved.on_target(&FieldPointer::root()),
                Fit::Preview => {
                    let projected = projection::project(saved, cancellation)?;
                    return Ok(Shown::Projected(projected));
                }
            };
            return Ok(match held.is_empty() {
                true => Shown::Whole(saved.result()?),
                false => Shown::Held(held, None),
            });
        }
        if question && !explicit {
            return Ok(Shown::Question);
        }
        let query = &mut self.query;
        if !explicit && !question && saved.product.is_some() {
            query.field = FieldPointer::root();
        }
        // A whole-result query always resolves to "/result" or "" here.
        if !terminal && (query.field == FieldPointer::result() || query.field.is_root()) {
            if matches!(query.position, args::Position::Query { .. }) {
                return Err(ToolError::invalid_arguments(
                    "the result is saved when the job finishes: query it then",
                ));
            }
            let empty = reader::empty(&query.text(), None, false);
            return Ok(Shown::Page(OutputPreview::Lines(empty)));
        }
        page(saved, query, (terminal, self.reaches), cancellation)
    }
}

/// What a presentation shows of its saved output besides the envelope.
enum Shown {
    Whole(Option<Value>),
    Projected(projection::Projected),
    Question,
    Page(OutputPreview),
    /// A page of the file that holds the selected field on the job's target.
    File(crate::session::TargetFile),
    /// What the read shows once it holds these fields whole, which the job's target
    /// holds: each within this many bytes, when the read is bounded.
    Held(Held, Option<usize>),
}

fn check_cancelled(cancellation: &super::CancellationToken) -> Result<(), ToolError> {
    if cancellation.is_cancelled() {
        Err(ToolError::cancelled())
    } else {
        Ok(())
    }
}

/// One page of the selected field, in its own unit: lines of text, or the
/// elements or members of JSON, which a pattern filters like lines. What the job's
/// target keeps in files is left to its reader to read there: the text the page
/// selects, for a reader that `reaches` it, and the fields a filter reads whole.
/// A reader that does not reach the target pages what the session holds of the text.
fn page(
    saved: &Saved,
    query: &args::Query,
    (terminal, reaches): (bool, bool),
    cancellation: &super::CancellationToken,
) -> Result<Shown, ToolError> {
    use args::{Page, Position};
    // Filters read their field whole; pages need only prefixes of stored text.
    let reading = match &query.position {
        Position::Query { .. }
        | Position::Page(Page {
            matcher: Some(_), ..
        }) => stream::Reading::Whole,
        Position::Page(_) => stream::Reading::SAMPLED,
    };
    let text = |text| reader::page(text, &query.text(), terminal, cancellation);
    let lines = || match query.position {
        Position::Query { .. } => Err(ToolError::invalid_arguments(
            "field is text: query applies to a JSON object or array",
        )),
        Position::Page(_) => query.starts_in(args::PageUnit::Lines),
    };
    let selected = saved.captures.get(&query.field);
    if let Some(crate::session::CaptureBytes::Target(file)) = selected.map(|capture| &capture.bytes)
        && reaches
    {
        return lines().map(|()| Shown::File(file.clone()));
    }
    if matches!(reading, stream::Reading::Whole) {
        let held = saved.on_target(&query.field);
        if !held.is_empty() {
            let limit = skyhook_base::limits::WHOLE_VALUE_BYTES;
            return Ok(Shown::Held(held, Some(limit)));
        }
    }
    let page = match stream::resolve(saved, &query.field, reading, cancellation)? {
        None => text(None).map(OutputPreview::Lines),
        Some(stream::Resolved::Text(field)) => {
            lines().and_then(|()| text(Some(field)).map(OutputPreview::Lines))
        }
        Some(stream::Resolved::Json {
            mut json,
            container,
        }) => {
            let (filter, start) = match &query.position {
                Position::Query { path, start } => (Some(query::Filter::Path(path)), *start),
                Position::Page(page) if page.offset > 0 || page.context > 0 => {
                    return Err(ToolError::invalid_arguments(format!(
                        "field is a JSON {}: start pages its {} and pattern filters them; offset and context apply to text",
                        container.name(),
                        container.unit(),
                    )));
                }
                Position::Page(page) => (
                    page.matcher.as_deref().map(query::Filter::Pattern),
                    page.start,
                ),
            };
            query.starts_in(match filter {
                Some(_) => args::PageUnit::Matches,
                None => container.unit(),
            })?;
            let (field, first, limit) = (&query.field, start - 1, query.limit);
            match filter {
                Some(filter) => query::page(&mut json, field, filter, first, limit, cancellation),
                None => elements::page(&mut json, container, field, first, limit, cancellation),
            }
        }
    };
    page.map(Shown::Page)
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
    use crate::job::{AgentId, CancellationToken, JobOutcome, JobSpec, JobState, tests::runtime};
    use std::assert_matches;

    /// A running job of `manager` that saves its output under `schema`, finished
    /// with `outcome` when one is given.
    pub(super) async fn fixture_with(
        manager: &JobManager,
        agent: &AgentId,
        schema: Option<Value>,
        outcome: Option<JobOutcome>,
    ) -> JobId {
        let mut spec = JobSpec::test(agent.clone(), "fixture");
        spec.output_schema = schema;
        let id = manager.test_running(spec).await.into_test_id();
        if let Some(outcome) = outcome {
            manager.finish(id, outcome).await.unwrap();
        }
        id
    }

    /// A runtime's running fixture job, finished with `value` when one is given.
    pub(super) async fn fixture(value: Option<Value>) -> (tempfile::TempDir, JobManager, JobId) {
        let (root, manager, agent) = runtime().await;
        let completed = |value| JobOutcome::Completed(crate::tool::ToolOutput::new(value).into());
        let id = fixture_with(&manager, &agent, None, value.map(completed)).await;
        (root, manager, id)
    }

    /// A read presents the run its envelope shows, however the job has run since.
    #[tokio::test]
    async fn a_read_presents_the_run_its_envelope_shows() {
        let (_root, manager, agent) = runtime().await;
        let id = manager
            .test_resumable(agent, "rerun", json!("second"))
            .await;
        manager.test_finish(id, json!("first")).await;
        let first = manager.snapshot(id).await.unwrap();
        manager.send(id, json!("again")).await.unwrap();
        let second = crate::tests::bounded(manager.wait(id, true)).await;
        assert_eq!(second.unwrap().output, Some(json!("second")));
        let shown = manager.saved_result(&first, None).await.unwrap();
        assert_eq!(shown.map(|output| output.value), Some(json!("first")));
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
        use crate::tool::ToolOutput;
        use skyhook_media::{BlobRef, ImageFormat, ImageRef};
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
        manager.test_complete(id, output).await;
        manager
            .output(id)
            .await
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
        for field in ["field", "start", "limit", "pattern", "context", "offset"] {
            let mut args = OutputArgs::new(id);
            // Text selectors page the text field.
            let text = Some("/result/text".parse().unwrap());
            match field {
                "field" => args.field = text,
                "start" => (args.field, args.start) = (text, Some(1)),
                "limit" => args.limit = Some(DEFAULT_LIMIT),
                "pattern" => (args.field, args.pattern) = (text, Some(String::new())),
                "context" => args.context = Some(0),
                "offset" => (args.field, args.offset) = (text, Some(0)),
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
        assert_eq!(manager.snapshot(id).await.unwrap().images, images);
    }

    #[tokio::test]
    async fn presentation_preserves_nulls_defaults_and_saved_result_presence() {
        let (_root, manager, agent) = runtime().await;
        let completed = |value| JobOutcome::Completed(crate::tool::ToolOutput::new(value).into());
        for raw in [
            Value::Null,
            json!({
                "error": null,
                "nested": {"absent": null, "ok": false},
                "array": [null, {"absent": null, "count": 0}]
            }),
        ] {
            let id = fixture_with(&manager, &agent, None, Some(completed(raw.clone()))).await;
            for view in [
                model(&manager, OutputArgs::new(id)).await,
                host(&manager, OutputArgs::new(id)).await,
            ] {
                assert_eq!(view.get("result"), Some(&raw));
                assert_eq!(view.get("presentation"), None);
            }
            assert!(host(&manager, OutputArgs::new(id)).await["meta"].is_object());
            assert_eq!(
                manager.output(id).await.test_document().unwrap()["result"],
                raw
            );
        }
        // A null result stays present after a failure too; hydration recovers its
        // availability from the saved presence bit.
        let failed = |output| JobOutcome::Failed {
            diagnostic: ToolError::failed("failure").into_facts().0,
            output,
        };
        for (outcome, available) in [
            (completed(Value::Null), true),
            (
                failed(Some(crate::tool::ToolOutput::new(Value::Null))),
                true,
            ),
            (failed(None), false),
            (ToolError::cancelled().into(), false),
        ] {
            let id = fixture_with(&manager, &agent, None, Some(outcome)).await;
            let view = model(&manager, OutputArgs::new(id)).await;
            assert_eq!(view.get("result"), available.then_some(&Value::Null));
            let envelope = manager.snapshot(id).await.unwrap();
            let result = manager.saved_result(&envelope, None).await.unwrap();
            assert_eq!(result.is_some(), available);
        }
    }

    /// Unfinished captures, whether abandoned by a successful result, a failure,
    /// or cancellation, remain discoverable, readable, and marked incomplete.
    #[tokio::test]
    async fn incomplete_captures_remain_discoverable_and_readable() {
        let (_root, manager, agent) = runtime().await;
        for outcome in ["failed", "cancelled", "unreferenced"] {
            let id = fixture_with(&manager, &agent, None, None).await;
            let field = if outcome == "unreferenced" {
                "/result/abandoned"
            } else {
                "/result/events~1custom/nested~0key"
            };
            let bytes = "{\"partial\":"; // Deliberately unfinished JSON.
            let output = manager.output(id).await;
            output.test_capture(field, CaptureKind::Json, bytes.as_bytes());
            for terminal in [false, true] {
                if terminal {
                    let outcome = match outcome {
                        "cancelled" => ToolError::cancelled().into(),
                        "failed" => ToolError::failed("injected failure").into(),
                        _ => JobOutcome::Completed(
                            crate::tool::ToolOutput::new(json!({"abandoned":null})).into(),
                        ),
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
                if outcome != "failed" && terminal {
                    assert_eq!(
                        view["presentation"]["notice"],
                        crate::job::Notice::OutputIncomplete.as_str()
                    );
                    assert_eq!(
                        page["presentation"]["notice"],
                        crate::job::Notice::OutputIncomplete.as_str()
                    );
                }
                if outcome != "unreferenced" || !terminal {
                    assert!(view["result"].is_null(), "{outcome}");
                }
            }
            assert_eq!(output.test_bytes(field).unwrap(), bytes.as_bytes());
        }
    }

    #[tokio::test]
    async fn capture_completeness_is_internal_and_only_partial_finished_jobs_have_a_notice() {
        let (_root, manager, agent) = runtime().await;
        for complete in [true, false] {
            let job = fixture_with(&manager, &agent, None, None).await;
            let mut output = crate::tool::ToolOutput::new(json!({"stdout":"line\n".repeat(120)}));
            if !complete {
                output.streams = skyhook_tools::output::StreamEnd::Cut;
            }
            manager.test_complete(job, output).await;
            for field in [None, Some("/result/stdout"), Some("")] {
                let mut query = OutputArgs::new(job);
                query.field = field.map(|field| field.parse().unwrap());
                let view = model(&manager, query.clone()).await;
                // A script's whole read says so as the model's preview does.
                let whole = host(&manager, query).await;
                for view in [view, whole] {
                    if complete {
                        assert!(view["presentation"]["notice"].is_null());
                    } else {
                        assert_eq!(
                            view["presentation"]["notice"],
                            crate::job::Notice::OutputIncomplete.as_str()
                        );
                    }
                }
            }
        }
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
        assert_eq!(
            manager.output(id).await.test_fields(),
            [FieldPointer::result()]
        );
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
            (
                "/result/items",
                "$[?(@.t =~ /a/)]",
                "$[?search(@.title, '[Ee]ncoder')]",
            ),
        ] {
            let mut args = field_args(id, field);
            args.query = Some(query.into());
            let error = manager
                .inspect_output(args, CancellationToken::new(), &Default::default())
                .await
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }
        // JSON pages by start in their unit, and restated text defaults select nothing.
        let mut args = field_args(id, "/result/items");
        (args.start, args.unit) = (Some(400), Some(PageUnit::Elements));
        (args.offset, args.context) = (Some(0), Some(0));
        let preview = page("/result/items", host(&manager, args).await);
        assert_eq!(preview["elements"], json!([items[399]]));
        // A pattern keeps the elements whose JSON text matches.
        let mut args = field_args(id, "/result/items");
        args.pattern = Some("pod-39[89]".into());
        let preview = page("/result/items", host(&manager, args).await);
        let at: Vec<_> = (preview["matches"].as_array().unwrap().iter())
            .map(|found| found["at"].as_str().unwrap())
            .collect();
        assert_eq!(at, ["/result/items/398", "/result/items/399"]);
        for (field, offset, expected) in [
            ("/result/items", true, "field is a JSON array"),
            ("/result/items", false, "query selects JSON matches"),
        ] {
            let mut args = field_args(id, field);
            if offset {
                args.offset = Some(2);
            } else {
                (args.query, args.pattern) = (Some("$".into()), Some("pod".into()));
            }
            let error = manager
                .inspect_output(args, CancellationToken::new(), &Default::default())
                .await
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    /// A position holds only in the unit of the page it continues: a capture
    /// paged as lines while its job ran is restarted, not skipped into, once
    /// it is JSON.
    #[tokio::test]
    async fn a_line_position_does_not_continue_a_field_that_became_json() {
        use std::io::Write as _;
        let (_root, manager, agent) = runtime().await;
        let schema = json!({"type":"object","properties":{
            "stdout":{"type":"string","contentMediaType":"application/json"}
        }});
        let id = fixture_with(&manager, &agent, Some(schema), None).await;
        let stdout: FieldPointer = "/result/stdout".parse().unwrap();
        let mut writer = crate::job::output::create_capture(
            &manager.output(id).await,
            &stdout,
            CaptureKind::Text,
        )
        .unwrap()
        .open();
        writer.write_all(b"[\n").unwrap();
        writer.flush().unwrap();
        let view = host(&manager, field_args(id, "/result/stdout")).await;
        let preview = &view["presentation"]["preview"];
        assert_eq!(
            (&preview["lines"], &preview["next_start"]),
            (&json!(["["]), &json!(2))
        );
        writer.write_all(b"1, 2]").unwrap();
        let receipt = writer.finish().unwrap();
        let completed = crate::tool::ToolOutput::new(json!({})).with_captures(vec![receipt]);
        manager.test_complete(id, completed).await;
        for unit in [Some(PageUnit::Lines), None] {
            let mut args = field_args(id, "/result/stdout");
            (args.start, args.unit) = (Some(2), unit);
            let error = manager
                .inspect_output(args, CancellationToken::new(), &Default::default())
                .await
                .unwrap_err();
            assert!(error.to_string().contains("from start 1"), "{error}");
        }
        let view = host(&manager, field_args(id, "/result/stdout")).await;
        assert_eq!(view["presentation"]["preview"]["elements"], json!([1, 2]));
    }

    #[tokio::test]
    async fn json_selectors_wait_for_the_saved_result() {
        let (_root, manager, id) = fixture(None).await;
        let mut args = field_args(id, "/result");
        args.query = Some("$".into());
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
            (args.pattern, args.context, args.limit) = (
                Some("(?i)error".into()),
                Some(2),
                Some(PageLimit::new::<2>()),
            );
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

    async fn capture(
        manager: &JobManager,
        id: JobId,
        field: &str,
        kind: CaptureKind,
        bytes: &[u8],
    ) {
        manager.output(id).await.test_capture(field, kind, bytes);
    }

    async fn hydrated(manager: &JobManager, job: JobId) -> (JobState, Value) {
        let args = OutputArgs::new(job);
        let (state, view, _) = manager
            .inspect_output_with_captures(args, CancellationToken::new(), &Default::default())
            .await
            .unwrap()
            .into_parts();
        let schema = &crate::job::JOB_VIEW_SCHEMAS.view;
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
                capture(&manager, job, field, CaptureKind::Json, bytes).await;
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
            }
        }
    }

    #[tokio::test]
    async fn automatic_hydration_distinguishes_absent_from_present_and_null() {
        let (_root, manager, job) = fixture(None).await;
        for field in ["/result/absent", "/result/null", "/result/present"] {
            capture(&manager, job, field, CaptureKind::Text, b"raw capture").await;
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
    async fn automatic_hydration_follows_a_live_captures_last_page() {
        let (_root, manager, job) = fixture(None).await;
        capture(
            &manager,
            job,
            "/result/log",
            CaptureKind::Text,
            (1..=150)
                .map(|line| format!("line {line}\n"))
                .collect::<String>()
                .as_bytes(),
        )
        .await;
        let (_, view) = hydrated(&manager, job).await;
        // The live whole-result page is empty but retains its polling position;
        // the capture has an independent source-page continuation.
        assert_eq!(
            view["presentation"]["preview"],
            json!({"field": "/result", "lines": [], "next_start": 1})
        );
        let preview = &view["presentation"]["captures"][0]["output"]["presentation"]["preview"];
        assert_eq!(preview["field"], "/result/log");
        let last: Vec<_> = (131..=150).map(|line| format!("line {line}")).collect();
        assert_eq!(preview["lines"], json!(last));
        assert_eq!(preview["next_start"], 151);
        assert_eq!(preview.get("next_offset"), None);
        // Lines over a page's bytes leave fewer of them on the last page.
        let wide = format!("{}\n", "w".repeat(CONTENT_BYTES / 4));
        capture(
            &manager,
            job,
            "/result/wide",
            CaptureKind::Text,
            wide.repeat(10).as_bytes(),
        )
        .await;
        let (_, view) = hydrated(&manager, job).await;
        let preview = &view["presentation"]["captures"][1]["output"]["presentation"]["preview"];
        assert_eq!(preview["lines"].as_array().unwrap().len(), 3);
        assert_eq!(preview["next_start"], 11);
        // The last page is measured as pages are, in JSON-encoded line content.
        let quoted = format!("{}\n", "\"".repeat(CONTENT_BYTES / 8));
        capture(
            &manager,
            job,
            "/result/quoted",
            CaptureKind::Text,
            quoted.repeat(10).as_bytes(),
        )
        .await;
        let (_, view) = hydrated(&manager, job).await;
        let preview = &view["presentation"]["captures"][1]["output"]["presentation"]["preview"];
        assert_eq!(preview["field"], "/result/quoted");
        assert_eq!(preview["lines"].as_array().unwrap().len(), 3);
        assert_eq!(preview["next_start"], 11);
    }

    #[tokio::test]
    async fn automatic_hydration_embeds_page_errors_but_propagates_initial_errors() {
        let (_root, manager, job) = fixture(None).await;
        // Discovery succeeds, but this page cannot decode its capture.
        capture(&manager, job, "/result/bad", CaptureKind::Text, b"\xffbad").await;
        capture(&manager, job, "/result/good", CaptureKind::Text, b"good").await;
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
        assert_matches!(
            result,
            Err(error) if matches!(error.diagnostic().cause, skyhook_tools::diagnostic::Cause::InvalidArguments(_)));
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
}
