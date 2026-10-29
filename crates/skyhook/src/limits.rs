//! Size limits shared across tools, each named by what it protects.

/// The most content a model may supply in one argument: text to write, a replacement,
/// or an MCP tool's arguments. It bounds what one call can make the host hold or send.
pub(crate) const MODEL_CONTENT_BYTES: usize = 4 * 1024 * 1024;

/// The largest single value held whole in memory: a file `replace` rewrites, one MCP
/// message, a JSON value a job query parses, or one line a pattern scans. Patterns see
/// whole lines up to this guard, so they never miss a match in a long line.
pub(crate) const WHOLE_VALUE_BYTES: usize = 64 * 1024 * 1024;
