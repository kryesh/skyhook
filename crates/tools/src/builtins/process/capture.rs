use crate::{
    error::CallError,
    output::{AsyncOutput, CaptureFile, FinishedOutput, PendingOutput, TextCaptureField},
    spec::Call,
};
use skyhook_worker::diagnostic::{Failure, Operation, Subject};
use skyhook_worker::op::{self, StreamRest};

/// One stream's text as its target sends it. The shared writer retains
/// partial-write progress.
pub(super) struct Capture {
    writer: AsyncOutput,
    field: TextCaptureField,
}

impl Capture {
    pub(super) fn new(writer: PendingOutput, field: TextCaptureField) -> Self {
        Self {
            writer: writer.open_async(),
            field,
        }
    }

    pub(super) async fn create(context: &Call, field: TextCaptureField) -> Result<Self, CallError> {
        let writer = context.text_capture(field).await.map_err(|error| {
            CallError::from(error)
                .context(super::started(
                    Operation::CreateCapture,
                    Subject::Label(field.pointer().into()),
                ))
                .opaque_io()
        })?;
        Ok(Self::new(writer, field))
    }

    /// Storage errors may echo captured output, so only their classification is kept.
    fn failed(&self, operation: Operation) -> impl FnOnce(std::io::Error) -> CallError + use<> {
        let context = super::started(operation, Subject::Label(self.field.pointer().into()));
        move |error| CallError::io(error).or(context).opaque_io()
    }

    pub(super) async fn write(&mut self, text: &str) -> Result<(), CallError> {
        let written = self.writer.write_text(text).await;
        written.map_err(self.failed(Operation::WriteCapture))
    }

    /// The latest of the stream, which its target is keeping in a file.
    pub(super) async fn latest(
        &mut self,
        extent: op::Extent,
        text: String,
    ) -> Result<(), CallError> {
        let told = self.writer.latest(extent, text).await;
        told.map_err(self.failed(Operation::WriteCapture))
    }

    /// The capture, when the stream had output: all of it, or its first bytes with
    /// the file its target kept the whole stream in. A stream whose file was lost
    /// keeps what was sent, with why the rest is gone.
    pub(super) async fn finish(
        self,
        rest: StreamRest,
    ) -> Result<(Option<FinishedOutput>, Option<Failure>), CallError> {
        let failed = self.failed(Operation::FinishCapture);
        let stream = Subject::Label(self.field.pointer().into());
        let finished = self.writer.finish_nonempty().await.map_err(failed)?;
        Ok(match rest {
            StreamRest::None => (finished, None),
            StreamRest::Spilled(stream) => {
                let file = CaptureFile::Written(stream);
                (finished.map(|capture| capture.in_file(file)), None)
            }
            StreamRest::Lost(fault) => {
                let lost = Failure::at(Operation::WriteCapture, stream, fault);
                (finished, Some(lost))
            }
        })
    }
}
