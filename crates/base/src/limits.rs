//! Size limits shared across tools, each named by what it protects.

/// The most content a model may supply in one argument: text to write, a replacement,
/// or an MCP tool's arguments. It bounds what one call can make the host hold or send.
pub const MODEL_CONTENT_BYTES: usize = 4 * 1024 * 1024;

/// The largest text read whole into model context: prompt attachments, instruction files
/// and skill text.
pub const MAX_TEXT_BYTES: u64 = 1024 * 1024;

/// The largest single value held whole in memory: a file `replace` rewrites, one MCP
/// message, a JSON value a job query parses, or one line a pattern scans. Patterns see
/// whole lines up to this guard, so they never miss a match in a long line.
pub const WHOLE_VALUE_BYTES: usize = 64 * 1024 * 1024;

/// The most bytes of one saved result field a session's database or a tool's result
/// holds: a larger one is kept in a file, and a larger text file is read where it is.
pub const SAVED_FIELD_BYTES: u64 = 4 * 1024 * 1024;

/// Counts the bytes written to it and keeps none of them, refusing any past `limit`:
/// the size of an encoding, without a copy of it.
pub struct ByteCount {
    pub bytes: usize,
    pub limit: usize,
}

impl std::io::Write for ByteCount {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if data.len() > self.limit.saturating_sub(self.bytes) {
            return Err(std::io::ErrorKind::FileTooLarge.into());
        }
        self.bytes += data.len();
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
