//! Disk-backed per-script console capture.
use std::io::Write;

pub(super) struct ConsoleOutput {
    file: std::fs::File,
}
impl ConsoleOutput {
    pub(super) fn new(path: &std::path::Path) -> std::io::Result<Self> {
        Ok(Self {
            file: std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(path)?,
        })
    }
    pub(super) fn log(&mut self, text: &str) -> std::io::Result<()> {
        self.file.write_all(text.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()
    }
    pub(super) fn finish(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
