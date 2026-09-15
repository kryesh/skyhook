use crate::{
    job::output::{AsyncCapture, CompletedCapture, PendingCapture},
    tool::ToolError,
};

/// The decoder tail belongs to the same owner as the text writer, not the
/// cancellable stream pump. The shared writer retains partial-write progress.
pub(super) struct Capture {
    writer: AsyncCapture,
    pending: Vec<u8>,
}

impl Capture {
    pub(super) fn new(writer: PendingCapture) -> Self {
        Self {
            writer: writer.open_async(),
            pending: Vec::new(),
        }
    }

    pub(super) async fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), ToolError> {
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
        // before that writer can suspend on filesystem IO.
        self.writer.write_text(&text).await?;
        Ok(())
    }

    pub(super) async fn finish(mut self) -> Result<Option<CompletedCapture>, ToolError> {
        // EOF, deadline and cancellation deliberately share lossy-tail policy.
        // No recovery after arbitrary IO errors or a dropped finalizer is claimed.
        self.writer
            .write_text(&String::from_utf8_lossy(&self.pending))
            .await?;
        Ok(self.writer.finish_nonempty().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        identity::JobId,
        job::output::{CaptureKind, TextCaptureField},
        tests::TestRuntime,
    };

    /// Writes `chunks` into a fresh stdout capture; returns the file text, if published.
    async fn written(runtime: &TestRuntime, id: u64, chunks: &[&[u8]]) -> Option<String> {
        let job = JobId::new(id).unwrap();
        let path =
            crate::job::output::field_file(&runtime.jobs.output_directory(job), "/result/stdout");
        let writer = runtime
            .jobs
            .pending_capture(
                job,
                TextCaptureField::Stdout.pointer(),
                CaptureKind::Text,
                true,
            )
            .await
            .unwrap();
        let mut capture = Capture::new(writer);
        for chunk in chunks {
            capture.write_bytes(chunk).await.unwrap();
        }
        let published = capture.finish().await.unwrap().is_some();
        assert_eq!(published, path.exists());
        published.then(|| std::fs::read_to_string(&path).unwrap())
    }

    #[tokio::test]
    async fn decoder_matches_lossy_utf8_at_every_chunk_boundary() {
        let runtime = TestRuntime::new().await;
        let mut id = 0;
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
                id += 1;
                let (head, tail) = bytes.split_at(split);
                assert_eq!(
                    written(&runtime, id, &[head, tail]).await,
                    expected,
                    "split {split} of {bytes:?}"
                );
            }
            id += 1;
            let single: Vec<&[u8]> = bytes.chunks(1).collect();
            assert_eq!(written(&runtime, id, &single).await, expected);
        }
    }
}
