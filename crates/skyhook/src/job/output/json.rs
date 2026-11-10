//! Saved JSON sequences read as the arrays they present, and saved JSON that fails to read.
use std::io::{self, BufRead, Read};

use super::ToolError;

/// A detected `Sequence` read as the array it presents, keeping every byte of its
/// values. Detection already validated the input as whitespace-separated objects
/// and arrays, so only string and nesting state decide where commas go.
pub(super) struct SequenceArray<R> {
    input: R,
    stage: Stage,
    scan: Scan,
    depth: u32,
    /// A top-level value ended, so the next one is preceded by a comma.
    separate: bool,
}

enum Stage {
    Open,
    Values,
    Closed,
}

/// What the previous byte left the scan inside.
#[derive(Clone, Copy)]
enum Scan {
    Structure,
    String,
    Escape,
}

impl<R: BufRead> SequenceArray<R> {
    pub(super) fn new(input: R) -> Self {
        Self {
            input,
            stage: Stage::Open,
            scan: Scan::Structure,
            depth: 0,
            separate: false,
        }
    }
}

impl<R: BufRead> Read for SequenceArray<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let Some(first) = buffer.first_mut() else {
            return Ok(0);
        };
        match self.stage {
            Stage::Open => {
                *first = b'[';
                self.stage = Stage::Values;
                return Ok(1);
            }
            Stage::Closed => return Ok(0),
            Stage::Values => {}
        }
        let bytes = self.input.fill_buf()?;
        if bytes.is_empty() {
            *first = b']';
            self.stage = Stage::Closed;
            return Ok(1);
        }
        let (mut read, mut written) = (0, 0);
        while read < bytes.len() && written < buffer.len() {
            let byte = bytes[read];
            match (self.scan, byte) {
                (Scan::Escape, _) => self.scan = Scan::String,
                (Scan::String, b'\\') => self.scan = Scan::Escape,
                (Scan::String, b'"') => self.scan = Scan::Structure,
                (Scan::String, _) => {}
                (Scan::Structure, b'{' | b'[') if self.depth == 0 && self.separate => {
                    buffer[written] = b',';
                    written += 1;
                    self.separate = false;
                    continue;
                }
                (Scan::Structure, b'"') => self.scan = Scan::String,
                (Scan::Structure, b'{' | b'[') => self.depth += 1,
                (Scan::Structure, b'}' | b']') => {
                    self.depth -= 1;
                    self.separate = self.depth == 0;
                }
                (Scan::Structure, _) => {}
            }
            buffer[written] = byte;
            (read, written) = (read + 1, written + 1);
        }
        self.input.consume(read);
        Ok(written)
    }
}

pub(super) fn saved_json(error: impl std::fmt::Display) -> ToolError {
    ToolError::failed(format!("saved output is not valid JSON: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Brackets and quotes inside strings are text, and a one-byte buffer still
    /// sees every comma.
    #[test]
    fn sequences_read_as_the_array_they_present() {
        let text = "{\"s\": \"}{\\\"[\\\\\"}\n\n[1.50,\n [2]]{}\n";
        let expected = "[{\"s\": \"}{\\\"[\\\\\"}\n\n,[1.50,\n [2]],{}\n]";
        let mut whole = String::new();
        SequenceArray::new(text.as_bytes())
            .read_to_string(&mut whole)
            .unwrap();
        assert_eq!(whole, expected);
        let mut bytewise = Vec::new();
        let mut byte = [0];
        let mut array = SequenceArray::new(text.as_bytes());
        while array.read(&mut byte).unwrap() == 1 {
            bytewise.push(byte[0]);
        }
        assert_eq!(bytewise, expected.as_bytes());
        let value: Value = serde_json::from_str(&whole).unwrap();
        assert_eq!(
            value,
            serde_json::json!([{"s": "}{\"[\\"}, [1.50, [2]], {}])
        );
    }
}
