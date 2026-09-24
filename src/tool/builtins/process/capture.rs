use crate::{
    tool::diagnostic::{Operation, Subject},
    tool::invocation::{LocalContext, LocalError},
    tool::output::{AsyncOutput, FinishedOutput, PendingOutput, TextCaptureField},
};

/// The decoder tail belongs to the same owner as the text writer, not the
/// cancellable stream pump. The shared writer retains partial-write progress.
pub(super) struct Capture {
    writer: AsyncOutput,
    pending: Vec<u8>,
    field: TextCaptureField,
}

impl Capture {
    pub(super) fn new(writer: PendingOutput, field: TextCaptureField) -> Self {
        Self {
            writer: writer.open_async(),
            pending: Vec::new(),
            field,
        }
    }

    pub(super) async fn create(
        context: &LocalContext,
        field: TextCaptureField,
    ) -> Result<Self, LocalError> {
        let writer = context.text_capture(field).await.map_err(|error| {
            LocalError::from(error)
                .context(super::started(
                    Operation::CreateCapture,
                    Subject::Label(field.pointer().into()),
                ))
                .opaque_io()
        })?;
        Ok(Self::new(writer, field))
    }

    /// Storage errors may echo captured output, so only their classification is kept.
    fn failed(&self, operation: Operation) -> impl FnOnce(std::io::Error) -> LocalError + use<> {
        let context = super::started(operation, Subject::Label(self.field.pointer().into()));
        move |error| LocalError::io(error).context(context).opaque_io()
    }

    pub(super) async fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), LocalError> {
        self.pending.extend_from_slice(bytes);
        let mut text = String::new();
        let mut consumed = 0;
        while consumed < self.pending.len() {
            match std::str::from_utf8(&self.pending[consumed..]) {
                Ok(valid) => {
                    text.push_str(valid);
                    consumed = self.pending.len();
                }
                Err(error) => {
                    let end = consumed + error.valid_up_to();
                    text.push_str(&String::from_utf8_lossy(&self.pending[consumed..end]));
                    consumed = end;
                    if let Some(length) = error.error_len() {
                        text.push('�');
                        consumed += length;
                    } else {
                        break;
                    }
                }
            }
        }
        self.pending.drain(..consumed);
        // This await first transfers all decoded text to the shared writer,
        // before that writer can suspend on storage IO.
        self.writer
            .write_text(&text)
            .await
            .map_err(self.failed(Operation::WriteCapture))
    }

    pub(super) async fn finish(mut self) -> Result<Option<FinishedOutput>, LocalError> {
        // EOF, deadline and cancellation deliberately share lossy-tail policy.
        // No recovery after arbitrary IO errors or a dropped finalizer is claimed.
        self.writer
            .write_text(&String::from_utf8_lossy(&self.pending))
            .await
            .map_err(self.failed(Operation::WriteCapture))?;
        let failed = self.failed(Operation::FinishCapture);
        self.writer.finish_nonempty().await.map_err(failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        job::output::HostOutput,
        tests::TestRuntime,
        tool::output::{ProducedOutput, TextCaptureField},
    };

    /// Writes `chunks` into a fresh stdout capture; returns the stored text, if published.
    async fn written(runtime: &TestRuntime, chunks: &[&[u8]]) -> Option<String> {
        let spec = crate::job::JobSpec::test(runtime.agent.clone(), "capture");
        let job = runtime.jobs.test_create(spec).await;
        let output = runtime.jobs.output(job);
        let host = HostOutput::new(runtime.store.clone(), job);
        let context = host.context();
        let writer = context
            .text_capture(TextCaptureField::Stdout)
            .await
            .unwrap();
        let mut capture = Capture::new(writer, TextCaptureField::Stdout);
        for chunk in chunks {
            capture.write_bytes(chunk).await.unwrap();
        }
        let finished = capture.finish().await.unwrap();
        context.settle().await.unwrap();
        let published = !host
            .finish(
                ProducedOutput::new(serde_json::Value::Null)
                    .with_captures(finished.into_iter().collect()),
            )
            .unwrap()
            .captures
            .is_empty();
        let bytes = output.test_bytes("/result/stdout");
        assert_eq!(published, bytes.is_some());
        bytes.map(|bytes| String::from_utf8(bytes).unwrap())
    }

    #[tokio::test]
    async fn decoder_matches_lossy_utf8_at_every_chunk_boundary() {
        let runtime = TestRuntime::new().await;
        let corpus: &[&[u8]] = &[
            b"",
            b"plain\ntext",
            "aé€🦀z".as_bytes(),
            b"\xff\xc0\xaf\xed\xa0\x80\xf4\x90\x80\x80",
            b"a\xe2x\x82b\xf0\x9f",
            b"\xc2",
            b"\xe2\x82",
            b"\xf0\x9f\xa6",
        ];
        for bytes in corpus {
            let expected = (!bytes.is_empty()).then(|| String::from_utf8_lossy(bytes).into_owned());
            for split in 0..=bytes.len() {
                let (head, tail) = bytes.split_at(split);
                assert_eq!(
                    written(&runtime, &[head, tail]).await,
                    expected,
                    "split {split} of {bytes:?}"
                );
            }
            let single: Vec<&[u8]> = bytes.chunks(1).collect();
            assert_eq!(written(&runtime, &single).await, expected);
        }
    }
}
