//! Per-script console capture, delivered with the terminal result.

const MAX_CONSOLE_BYTES: usize = 16 * 1024 * 1024;
const TRUNCATED: &str = "\n[console output truncated at 16 MiB]\n";

#[derive(Default)]
pub(super) struct ConsoleOutput {
    text: String,
    truncated: bool,
}

impl ConsoleOutput {
    pub(super) fn log(&mut self, text: &str) {
        if self.truncated {
            return;
        }
        self.append(text);
        self.append("\n");
    }

    fn append(&mut self, text: &str) {
        if self.truncated {
            return;
        }
        let remaining = MAX_CONSOLE_BYTES - self.text.len();
        if text.len() <= remaining {
            self.text.push_str(text);
        } else {
            let mut end = remaining;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            self.text.push_str(&text[..end]);
            self.text.push_str(TRUNCATED);
            self.truncated = true;
        }
    }

    pub(super) fn take(&mut self) -> String {
        std::mem::take(&mut self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_cap_preserves_utf8_and_marks_truncation_once() {
        let mut output = ConsoleOutput::default();
        output.log(&"x".repeat(MAX_CONSOLE_BYTES - 2));
        output.log("🦀");
        output.log("discarded");
        let text = output.take();
        assert_eq!(text.len(), MAX_CONSOLE_BYTES - 1 + TRUNCATED.len());
        assert!(text.ends_with(TRUNCATED));
        assert_eq!(text.matches(TRUNCATED).count(), 1);
        assert!(!text.contains("discarded"));
    }
}
