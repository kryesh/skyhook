//! Target tools as the host runs them: each builds one operation from its admitted
//! input, and renders that operation's events and end into its result, with what its
//! call has on the host.
use std::future::Future;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use skyhook_worker::{op::Running, protocol::Wire};

use crate::{
    error::AdmissionError,
    error::CallError,
    output::{
        CaptureKind, FieldPointer, OutputContext, PendingOutput, ProducedOutput, TextCaptureField,
    },
    path::PathArgument,
    target::ExecutionLocation,
};

/// The workspace itself, as a path relative to it: what an omitted path
/// argument names.
pub const DEFAULT_PATH: &str = ".";

/// The `{path, target?}` argument a source-reading tool declares.
pub const SOURCE_ARGUMENT: &str = "source";

pub trait TargetTool: Send + Sync + 'static {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    type Input: DeserializeOwned + JsonSchema + 'static;
    type Admitted: Send + 'static;
    type Op: Wire;
    /// What rendering the operation needs of the admitted input.
    type Rendering: Send + 'static;

    /// The result's schema; a tool without one completes without a result.
    fn result_schema() -> Option<serde_json::Value>;

    /// Validate `input` before approval.
    fn admit(input: Self::Input) -> Result<Self::Admitted, AdmissionError>;

    /// The paths to resolve where the call runs, rewritten in place to their resolved
    /// spellings.
    fn paths(admitted: &mut Self::Admitted) -> Vec<PathArgument<'_>>;

    /// The operation the call runs, taking from `admitted` what it needs but its paths,
    /// which stay there to take their resolutions.
    fn op(admitted: &mut Self::Admitted, call: &Call) -> Result<Self::Op, CallError>;

    /// What rendering the operation keeps of the admitted input, its paths resolved.
    fn rendering(admitted: Self::Admitted) -> Self::Rendering;

    /// The call's result, from its running operation's events and end. Dropping
    /// `running` stops the operation.
    fn render<U: Into<CallError> + Send + 'static>(
        rendering: Self::Rendering,
        running: Running<Self::Op, U>,
        call: &Call,
    ) -> impl Future<Output = Result<ProducedOutput, CallError>> + Send;
}

/// What a call's target keeps in files of its own instead of sending back: output
/// past `inline` bytes of one body or stream, in its spill directory for the session.
/// It reports the last `tail` bytes of a stream it kept, and while the stream runs,
/// at most every `live`.
#[derive(Clone, Debug)]
pub struct Spills {
    session: String,
    job: skyhook_worker::job::JobId,
    pub inline: u64,
    pub tail: u64,
    pub live: std::time::Duration,
}

impl Spills {
    pub fn new(
        session: &impl std::fmt::Display,
        job: skyhook_worker::job::JobId,
        (inline, tail, live): (u64, u64, std::time::Duration),
    ) -> Self {
        let session = session.to_string();
        Self {
            session,
            job,
            inline,
            tail,
            live,
        }
    }

    /// A file no other call names, for `part` of this call's output.
    pub(crate) fn name(&self, part: &str) -> skyhook_worker::fs::spill::SpillName {
        let stem = format!("job{}-{part}", self.job);
        skyhook_worker::fs::spill::SpillName::unique(&self.session, &stem)
    }
}

/// What a target tool's call has on the host: where it runs, its environment,
/// its cancellation, its opened source, its output and what its target spills.
#[derive(Clone)]
pub struct Call {
    location: ExecutionLocation,
    spills: Spills,
    pub(crate) process_environment: skyhook_worker::origin::ProcessEnvironment,
    cancellation: tokio_util::sync::CancellationToken,
    output: OutputContext,
    source: Option<crate::source::Source>,
}

impl Call {
    pub fn new(
        location: ExecutionLocation,
        process_environment: skyhook_worker::origin::ProcessEnvironment,
        cancellation: tokio_util::sync::CancellationToken,
        output: OutputContext,
        spills: Spills,
    ) -> Self {
        Self {
            location,
            spills,
            process_environment,
            cancellation,
            output,
            source: None,
        }
    }

    /// Attach the opened source argument.
    pub fn with_source(mut self, source: Option<crate::source::Source>) -> Self {
        self.source = source;
        self
    }

    /// The source argument; the executor opens it for every call naming one.
    pub(crate) fn source(&self) -> Result<&crate::source::Source, CallError> {
        self.source
            .as_ref()
            .ok_or_else(|| CallError::failed("source was not opened"))
    }

    pub(crate) fn execution_location(&self) -> &ExecutionLocation {
        &self.location
    }
    pub(crate) fn spills(&self) -> &Spills {
        &self.spills
    }
    pub(crate) fn cancellation_token(&self) -> tokio_util::sync::CancellationToken {
        self.cancellation.clone()
    }
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
    pub(crate) async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }
    pub(crate) async fn pending_stream_capture(
        &self,
        field: FieldPointer,
        kind: CaptureKind,
    ) -> Result<PendingOutput, CallError> {
        Ok(self.output.pending_stream_capture(field, kind).await?)
    }
    pub(crate) async fn text_capture(
        &self,
        field: TextCaptureField,
    ) -> Result<PendingOutput, CallError> {
        Ok(self.output.text_capture(field).await?)
    }
    pub(crate) async fn store_image(
        &self,
        path: Option<String>,
        image: &skyhook_media::Image,
    ) -> Result<skyhook_media::ImageRef, CallError> {
        Ok(self.output.store_image(path, image).await?)
    }
}

/// How a running operation ended, its target's unreachable failures reported as a
/// call's.
pub(crate) async fn ended<O: skyhook_worker::op::Operation, U: Into<CallError>>(
    running: skyhook_worker::op::Running<O, U>,
) -> Result<Result<O::End, O::Error>, CallError> {
    running.end().await.map_err(Into::into)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::diagnostic::PartialContext;
    use crate::error::OperationError;
    use crate::output::{CaptureEvent, CaptureId, OutputEvent, OutputSink};
    use serde_json::Value;
    use skyhook_worker::diagnostic::{Operation, Subject};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tokio::sync::watch;

    /// A capture as its sink received it.
    struct Captured {
        field: FieldPointer,
        kind: CaptureKind,
        bytes: Vec<u8>,
        latest: Option<(skyhook_worker::op::Extent, String)>,
    }

    #[derive(Default)]
    pub(crate) struct CapturedOutput(watch::Sender<BTreeMap<CaptureId, Captured>>);

    fn joined(captures: &BTreeMap<CaptureId, Captured>) -> Vec<u8> {
        (captures.values())
            .flat_map(|captured| captured.bytes.iter().copied())
            .collect()
    }

    impl CapturedOutput {
        pub(crate) fn bytes(&self) -> Vec<u8> {
            joined(&self.0.borrow())
        }

        /// Once this sink has received output that `ready` accepts, which it returns.
        pub(crate) async fn written(&self, ready: impl Fn(&[u8]) -> bool) -> Vec<u8> {
            let mut captures = self.0.subscribe();
            let wanted = captures.wait_for(|captures| ready(&joined(captures)));
            joined(&crate::tests::bounded(wanted).await.unwrap())
        }

        /// Once a capture's target told of the latest of its stream, as `ready`
        /// accepts it: that report.
        pub(crate) async fn latest(
            &self,
            ready: impl Fn(&(skyhook_worker::op::Extent, String)) -> bool,
        ) -> (skyhook_worker::op::Extent, String) {
            let mut captures = self.0.subscribe();
            let told = |captures: &BTreeMap<CaptureId, Captured>| {
                let mut latest = captures
                    .values()
                    .filter_map(|captured| captured.latest.as_ref());
                latest.find(|latest| ready(latest)).cloned()
            };
            let wanted = captures.wait_for(|captures| told(captures).is_some());
            told(&crate::tests::bounded(wanted).await.unwrap()).unwrap()
        }

        /// `output`'s value with each capture it finished placed at its field.
        pub(crate) fn fold(&self, output: ProducedOutput) -> Value {
            let captures = self.0.borrow();
            let mut value = output.value;
            for finished in &output.captures {
                let Captured {
                    field, kind, bytes, ..
                } = &captures[&finished.id()];
                let captured = match kind {
                    CaptureKind::Text => Value::String(String::from_utf8(bytes.clone()).unwrap()),
                    CaptureKind::Json => serde_json::from_slice(bytes).unwrap(),
                };
                let field = field.relative_to(&FieldPointer::result()).unwrap();
                let segments: Vec<_> = field.segments().collect();
                let Some((last, parents)) = segments.split_last() else {
                    value = captured;
                    continue;
                };
                let parent =
                    (parents.iter()).fold(&mut value, |value, key| &mut value[key.as_str()]);
                parent[last.as_str()] = captured;
            }
            value
        }
    }

    impl OutputSink for CapturedOutput {
        fn send(&self, event: OutputEvent) -> std::io::Result<()> {
            let OutputEvent::Capture(event) = event else {
                return Ok(());
            };
            self.0.send_modify(|captures| match event {
                CaptureEvent::Open { id, field, kind } => {
                    let (bytes, latest) = (Vec::new(), None);
                    let captured = Captured {
                        field,
                        kind,
                        bytes,
                        latest,
                    };
                    captures.insert(id, captured);
                }
                CaptureEvent::Latest { id, extent, text } => {
                    captures.get_mut(&id).unwrap().latest = Some((extent, text));
                }
                CaptureEvent::Write { id, data } => {
                    captures.get_mut(&id).unwrap().bytes.extend(data)
                }
                CaptureEvent::Discard { id } => {
                    captures.remove(&id);
                }
                CaptureEvent::Finish { .. } => {}
            });
            Ok(())
        }
    }

    /// Run target tool `S` on `local` as a host runs it, every permission allowed:
    /// its arguments admitted against its schema, its paths resolved, then its
    /// operation run and rendered. Inputs only the registry offers, such as one a
    /// capability gates, pass the schema; deserializing still refuses unknown ones.
    pub(crate) async fn run_spec<S: crate::spec::TargetTool>(
        local: &skyhook_worker::local::Local,
        arguments: Value,
        call: Call,
    ) -> Result<ProducedOutput, CallError> {
        use skyhook_worker::target::Target as _;
        let fallback = PartialContext::new(Operation::Execute, Subject::Tool(S::NAME.to_owned()));
        let run = async {
            let schema = crate::schema::input_schema_for::<S::Input>();
            let mut open = schema.clone();
            open.as_object_mut().unwrap().remove("additionalProperties");
            let validator =
                crate::arguments::argument_validator(&open).map_err(CallError::failed)?;
            let arguments = crate::arguments::admit_arguments(
                &open,
                &validator,
                crate::arguments::Author::Program,
                arguments,
            )?;
            let input = crate::arguments::deserialize_arguments::<S::Input>(&schema, &arguments)?;
            let mut admitted = S::admit(input)?;
            let location = call.execution_location().clone();
            let arguments = S::paths(&mut admitted);
            let mut facts = Vec::new();
            if let Some(request) = crate::path::request(&arguments, &location) {
                let Ok(Ok(resolved)) = local.call(request).await;
                let admitted = crate::path::admit(arguments, resolved, &location)?;
                if let Some(unresolved) = admitted.unresolved {
                    return Err(unresolved.into());
                }
                facts = admitted.facts;
            }
            let fallback = PartialContext::default().paths(facts);
            let rendered = async {
                let op = S::op(&mut admitted, &call)?;
                let Ok(running) = local.run(op).await;
                S::render(S::rendering(admitted), running, &call).await
            };
            rendered.await.map_err(|error| error.or(fallback))
        };
        run.await.map_err(|error| error.or(fallback))
    }

    /// Run the target tool `name` names on `local`, as [`run_spec`] does.
    pub(crate) async fn run_named(
        local: &skyhook_worker::local::Local,
        name: &str,
        arguments: Value,
        call: Call,
    ) -> Result<ProducedOutput, CallError> {
        use crate::{builtins::*, spec::TargetTool as _};
        match name {
            Read::NAME => run_spec::<Read>(local, arguments, call).await,
            WriteFile::NAME => run_spec::<WriteFile>(local, arguments, call).await,
            ReplaceText::NAME => run_spec::<ReplaceText>(local, arguments, call).await,
            RemoveEntry::NAME => run_spec::<RemoveEntry>(local, arguments, call).await,
            Search::NAME => run_spec::<Search>(local, arguments, call).await,
            Glob::NAME => run_spec::<Glob>(local, arguments, call).await,
            RunCommand::NAME => run_spec::<RunCommand>(local, arguments, call).await,
            Fetch::NAME => run_spec::<Fetch>(local, arguments, call).await,
            _ => Err(CallError::unknown_tool(name)),
        }
    }

    /// A call in `root`, whose target spills what passes the saved-field limit.
    pub(crate) fn call(
        root: &std::path::Path,
        output: OutputContext,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Call {
        let inline = skyhook_base::limits::SAVED_FIELD_BYTES;
        spilling_call(root, output, cancellation, inline)
    }

    /// A call in `root`, whose target spills what passes `inline` bytes.
    fn spilling_call(
        root: &std::path::Path,
        output: OutputContext,
        cancellation: tokio_util::sync::CancellationToken,
        inline: u64,
    ) -> Call {
        let job = skyhook_worker::job::JobId::new(1).unwrap();
        Call::new(
            ExecutionLocation::root(crate::tests::text(root)),
            Default::default(),
            cancellation,
            output,
            Spills::new(&"session", job, (inline, 64, std::time::Duration::ZERO)),
        )
    }

    /// A workspace the builtins run in, with the default capabilities, every
    /// authorization allowed and their captures folded into their values. Its
    /// machine keeps its spills in `spills`, beside the workspace's files.
    pub(crate) struct Workspace {
        pub(crate) root: tempfile::TempDir,
        local: skyhook_worker::local::Local,
        inline: u64,
    }

    impl Workspace {
        pub(crate) fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let local = skyhook_worker::local::Local::new(root.path().to_owned());
            let local = local.spilling_in(Some(root.path().join("spills")));
            let inline = skyhook_base::limits::SAVED_FIELD_BYTES;
            Self {
                root,
                local,
                inline,
            }
        }

        /// The workspace, with deadlines that elapse when `sleep` does.
        pub(crate) fn with_clock(self, sleep: skyhook_worker::local::Sleep) -> Self {
            let local = skyhook_worker::local::Local::with_clock(self.path().to_owned(), sleep);
            let local = local.spilling_in(Some(self.path().join("spills")));
            Self { local, ..self }
        }

        /// The workspace, its calls spilling what passes `inline` bytes.
        pub(crate) fn spilling_past(self, inline: u64) -> Self {
            Self { inline, ..self }
        }

        /// The session's spill files, by name.
        pub(crate) fn spilled(&self) -> Vec<std::path::PathBuf> {
            let files = std::fs::read_dir(self.path().join("spills/session"));
            let mut files: Vec<_> = (files.into_iter().flatten().flatten())
                .map(|entry| entry.path())
                .collect();
            files.sort();
            files
        }

        pub(crate) fn path(&self) -> &std::path::Path {
            self.root.path()
        }

        pub(crate) async fn run(
            &self,
            tool: &str,
            arguments: Value,
        ) -> Result<Value, OperationError<Value>> {
            let cancellation = tokio_util::sync::CancellationToken::new();
            self.run_with(tool, arguments, &Arc::default(), cancellation)
                .await
        }

        /// Run `tool` with `sink` receiving its output as it runs, until `cancellation`.
        pub(crate) async fn run_with(
            &self,
            tool: &str,
            arguments: Value,
            sink: &Arc<CapturedOutput>,
            cancellation: tokio_util::sync::CancellationToken,
        ) -> Result<Value, OperationError<Value>> {
            let result = self.produce(tool, arguments, sink, cancellation).await;
            let fold = |output| Ok::<_, std::convert::Infallible>(sink.fold(output));
            match result {
                Ok(output) => Ok(sink.fold(output)),
                Err(error) => Err(error.try_map_output(fold).unwrap()),
            }
        }

        /// [`run_with`](Self::run_with), the output as produced: its captures in `sink`.
        pub(crate) async fn produce(
            &self,
            tool: &str,
            arguments: Value,
            sink: &Arc<CapturedOutput>,
            cancellation: tokio_util::sync::CancellationToken,
        ) -> Result<ProducedOutput, CallError> {
            let output = OutputContext::new(sink.clone());
            let call = spilling_call(self.path(), output.clone(), cancellation, self.inline);
            let result = run_named(&self.local, tool, arguments, call).await;
            crate::tests::bounded(output.settle()).await.unwrap();
            result
        }
    }
}
