//! Exact-text replacement in a UTF-8 file, read, edited and committed in one step.
use skyhook_base::limits::WHOLE_VALUE_BYTES;

use crate::diagnostic::{Failure, Fault, Operation};
use crate::fs::Contents;
use crate::op::{Events, Never, Replace, SHOWN_LINES};

impl crate::op::Operation for Replace {
    type Event = Never;
    type End = ();
    type Error = Failure;

    fn paths(&mut self) -> Vec<&mut crate::op::TargetPath> {
        vec![&mut self.path]
    }

    async fn perform(self, _: &super::Local, _: Events<Never>) -> Result<(), Failure> {
        let replace = self;
        let unchanged =
            |operation, fault: Fault| Failure::unchanged(operation, &replace.path, fault);
        let path = replace.path.native();
        let bytes = crate::fs::read_regular(path, WHOLE_VALUE_BYTES as u64)
            .await
            .map_err(|error| unchanged(Operation::Read, error.into()))?;
        let text = String::from_utf8(bytes).map_err(|error| {
            let error = error.utf8_error();
            let fault = Fault::NotText {
                valid_up_to: error.valid_up_to(),
                error_len: error.error_len().map(|length| length as u8),
            };
            unchanged(Operation::Deserialize, fault)
        })?;
        let ending = LineEnding::of(&text);
        let (old, new) = (ending.apply(replace.old), ending.apply(replace.new));
        let found = text.matches(&old).count();
        if found != replace.count {
            let fault = Fault::CountMismatch {
                expected: replace.count,
                found,
                lines: match_lines(&text, &old),
            };
            return Err(unchanged(Operation::Validate, fault));
        }
        // Bound the result before allocating it: a short search string and
        // a large replacement can otherwise expand a small input enormously.
        let bytes = replacement_size(text.len(), old.len(), new.len(), found);
        if bytes > WHOLE_VALUE_BYTES {
            let limit = WHOLE_VALUE_BYTES;
            return Err(unchanged(
                Operation::Validate,
                Fault::EditTooLarge { bytes, limit },
            ));
        }
        let output = text.replace(&old, &new);
        crate::fs::atomic_write(path, Contents::Bytes(output.into_bytes()))
            .await
            .map_err(|error| Failure::atomic(&replace.path, error))?;
        Ok(())
    }
}

/// Starting lines of the first few matches, one beyond those shown when there are more.
fn match_lines(text: &str, old: &str) -> Vec<usize> {
    let mut lines = Vec::new();
    let (mut line, mut scanned) = (1, 0);
    for (offset, _) in text.match_indices(old).take(SHOWN_LINES + 1) {
        line += text[scanned..offset].matches('\n').count();
        scanned = offset;
        lines.push(line);
    }
    lines
}

/// How the model's line breaks map onto a file's.
#[derive(Clone, Copy)]
enum LineEnding {
    /// Every line break in the file is CRLF, so each of the model's line breaks, bare `\n` or
    /// `\r\n`, stands for `\r\n`.
    Crlf,
    /// LF, mixed, or no line breaks: the model's text is matched and written exactly. A mixed
    /// file has no single ending to follow, so guessing one could match or write the wrong one.
    Exact,
}

impl LineEnding {
    fn of(text: &str) -> Self {
        let breaks = text.matches('\n').count();
        if breaks > 0 && breaks == text.matches("\r\n").count() {
            Self::Crlf
        } else {
            Self::Exact
        }
    }

    fn apply(self, text: String) -> String {
        match self {
            Self::Crlf => text.replace("\r\n", "\n").replace('\n', "\r\n"),
            Self::Exact => text,
        }
    }
}

/// `count` non-overlapping matches of `old` lie within `text`.
fn replacement_size(text: usize, old: usize, new: usize, count: usize) -> usize {
    (text - old * count).saturating_add(new.saturating_mul(count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::TargetPath;

    #[test]
    fn match_lines_reports_starting_lines_and_one_past_the_shown() {
        assert!(match_lines("a\nb\n", "x").is_empty());
        assert_eq!(match_lines("x x\na\nb\nx\n", "x"), [1, 1, 4]);
        assert_eq!(match_lines("a\nb\nc\nb\nc\n", "b\nc"), [2, 4]);
        assert_eq!(
            match_lines(&"x\n".repeat(12), "x"),
            (1..=SHOWN_LINES + 1).collect::<Vec<_>>()
        );
    }

    #[test]
    fn replacement_size_checks_expansion_shrinkage_and_overflow() {
        assert_eq!(replacement_size(8, 1, 3, 2), 12);
        assert_eq!(replacement_size(8, 3, 1, 2), 4);
        assert_eq!(replacement_size(8, 1, usize::MAX, 2), usize::MAX);
    }

    async fn replace(edit: Replace) -> Result<(), Failure> {
        use crate::target::Target as _;
        let Ok(replaced) = super::super::Local::new(std::env::temp_dir())
            .call(edit)
            .await;
        replaced
    }

    fn edit(path: &std::path::Path, old: &str, new: &str, count: usize) -> Replace {
        Replace {
            path: TargetPath::new(path.to_str().unwrap()),
            old: old.into(),
            new: new.into(),
            count,
        }
    }

    /// A file beyond what a model may supply is still edited, while a replacement
    /// that would grow past the memory guard is refused without mutating; a
    /// mismatch reports counts and lines, never the file's text.
    #[tokio::test]
    async fn replace_edits_large_files_and_refuses_expansion_and_mismatches() {
        use skyhook_base::limits::MODEL_CONTENT_BYTES;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("large.txt");
        let large = format!("{}needle", "x".repeat(MODEL_CONTENT_BYTES));
        std::fs::write(&path, &large).unwrap();
        replace(edit(&path, "needle", "thread", 1)).await.unwrap();
        let edited = std::fs::read_to_string(&path).unwrap();
        assert_eq!(edited.len(), large.len());
        assert!(edited.ends_with("xthread"));
        let copies = WHOLE_VALUE_BYTES / (MODEL_CONTENT_BYTES - 1) + 1;
        std::fs::write(&path, "x".repeat(copies)).unwrap();
        let new = "y".repeat(MODEL_CONTENT_BYTES - 1);
        let refused = replace(edit(&path, "x", &new, copies)).await;
        assert!(matches!(
            refused,
            Err(Failure {
                fault: Fault::EditTooLarge { .. },
                ..
            })
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x".repeat(copies));
        std::fs::write(&path, "a a\nb\na\n").unwrap();
        let mismatch = replace(edit(&path, "a", "c", 2)).await.unwrap_err();
        let lines = vec![1, 1, 3];
        let fault = Fault::CountMismatch {
            expected: 2,
            found: 3,
            lines,
        };
        assert_eq!(mismatch.fault, fault);
    }

    /// A uniformly CRLF file reads the model's line breaks, bare or CRLF, as its own;
    /// LF and mixed files are matched and written exactly as given.
    #[tokio::test]
    async fn replace_follows_a_uniform_crlf_file_and_matches_others_exactly() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("endings.txt");
        for (original, old, new, edited) in [
            ("a\r\nb\r\n", "a\nb", "a\nc", Some("a\r\nc\r\n")),
            ("a\r\nb\r\n", "a\r\nb", "x\r\ny\nz", Some("x\r\ny\r\nz\r\n")),
            ("a\nb\n", "a\nb", "a\nc", Some("a\nc\n")),
            ("a\nb\n", "a\r\nb", "a\nc", None),
            ("a\r\nb\nc\r\n", "b\nc", "b\nd", Some("a\r\nb\nd\r\n")),
            ("a\r\nb\nc\r\n", "a\nb", "a\nd", None),
        ] {
            std::fs::write(&path, original).unwrap();
            let result = replace(edit(&path, old, new, 1)).await;
            assert_eq!(result.is_ok(), edited.is_some(), "{original:?} {old:?}");
            let written = std::fs::read_to_string(&path).unwrap();
            assert_eq!(written, edited.unwrap_or(original), "{original:?} {old:?}");
        }
    }
}
