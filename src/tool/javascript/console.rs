//! Database-backed per-script console capture with consuming finalization.

use crate::job::output::{CaptureWriter, CompletedCapture};

pub(super) struct ConsoleOutput {
    capture: Option<CaptureWriter>,
}
impl ConsoleOutput {
    pub(super) fn new(capture: CaptureWriter) -> Self {
        Self {
            capture: Some(capture),
        }
    }

    pub(super) fn log(&mut self, text: &str) -> std::io::Result<()> {
        let capture = self
            .capture
            .as_mut()
            .ok_or_else(|| std::io::Error::other("console capture is finalized"))?;
        capture.write_text(text)?;
        capture.write_text("\n")
    }

    pub(super) fn finish(&mut self) -> std::io::Result<Option<CompletedCapture>> {
        self.capture
            .take()
            .ok_or_else(|| std::io::Error::other("console capture is finalized"))?
            .finish_nonempty()
    }
}
