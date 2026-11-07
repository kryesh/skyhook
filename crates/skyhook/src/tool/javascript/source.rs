//! The submitted async body, compiled as its own module. Compiling a module
//! runs nothing, so a syntax error is known before anything runs.

pub(super) const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;

/// The module name that marks the submitted body's frames.
pub(super) const FILENAME: &str = "skyhook-script";
// The wrapper opens and closes on lines of its own, and the body compiles one
// line up, so positions everywhere are the body's own. The comma keeps the
// function anonymous rather than named `default`.
const PREFIX: &str = "export default (0, async () => {\n";
const SUFFIX: &str = "\n})";

pub(super) struct SubmittedSource {
    wrapper: String,
    /// Where the body ends in the wrapper, in UTF-16 units as V8 counts.
    end: usize,
}

impl SubmittedSource {
    pub(super) fn new(source: &str) -> Self {
        Self {
            wrapper: format!("{PREFIX}{source}{SUFFIX}"),
            end: PREFIX.len() + source.encode_utf16().count(),
        }
    }

    /// The compiled module; a syntax error is the pending exception, its stack
    /// set to the frame of its position, as the failure renderer reads stacks.
    /// An error past the body means the body is incomplete, or closes the wrapper's
    /// function early with a `}` of its own: only then does it parse when that
    /// function's call closes straight after it.
    pub(super) fn compile<'s>(
        &self,
        scope: &mut v8::PinnedRef<'s, v8::TryCatch<v8::HandleScope>>,
    ) -> Option<v8::Local<'s, v8::Module>> {
        let module = super::engine::compile(scope, FILENAME, &self.wrapper, -1);
        if module.is_none()
            && let (Some(message), Some(error)) = (scope.message(), scope.exception())
            && let (Some(line), Ok(error)) = (
                message.get_line_number(scope),
                error.try_cast::<v8::Object>(),
            )
        {
            let ended = usize::try_from(message.get_start_position()).is_ok_and(|at| at > self.end);
            let frame = if ended {
                let text = if self.closes_early(scope) {
                    "Unmatched '}': the body closes before its end"
                } else {
                    "Unexpected end of input"
                };
                if let (Some(key), Some(text)) = (
                    v8::String::new(scope, "message"),
                    v8::String::new(scope, text),
                ) {
                    error.set(scope, key.into(), text.into());
                }
                "    at end of source".to_owned()
            } else {
                format!("    at line {line}:{}", message.get_start_column() + 1)
            };
            if let (Some(key), Some(frame)) = (
                v8::String::new(scope, "stack"),
                v8::String::new(scope, &frame),
            ) {
                error.set(scope, key.into(), frame.into());
            }
        }
        module
    }

    fn closes_early(&self, scope: &mut v8::PinScope<'_, '_>) -> bool {
        let body = &self.wrapper[..self.wrapper.len() - SUFFIX.len()];
        v8::tc_scope!(let scope, scope);
        super::engine::compile(scope, FILENAME, &format!("{body}\n)"), -1).is_some()
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use crate::tool::diagnostic::{Cause, Effects, Operation};
    use crate::tool::javascript::tests::plain;

    #[tokio::test]
    async fn syntax_errors_report_body_positions() {
        // Input that closes the wrapper runs before the body, not at compilation.
        let diagnostic = plain("}); throw new Error('escaped'); (0, async () => {")
            .await
            .unwrap_err()
            .diagnostic();
        assert_eq!(diagnostic.context.operation, Operation::Execute);
        assert_eq!(diagnostic.context.effects, Effects::Unknown);
        for (source, location) in [
            ("\n\nconst value = ;", "line 3:15"),
            ("let a = 1; let b = ;", "line 1:20"),
            ("if (true) {", "end of source"),
            ("return 1; }", "end of source"),
        ] {
            let diagnostic = plain(source).await.unwrap_err().diagnostic();
            assert_eq!(diagnostic.context.operation, Operation::Validate);
            assert_eq!(diagnostic.context.effects, Effects::NotStarted);
            assert_matches!(&diagnostic.cause, Cause::Message(message)
                    if message.starts_with("SyntaxError: ") && message.ends_with(&format!("at {location}")),
                "{diagnostic:?}"
            );
        }
        // Input that ends early reads as such, wherever the parser stopped, and
        // so does a body that closes the wrapper with a brace of its own.
        for (source, message) in [
            ("const items = [1, 2", "Unexpected end of input"),
            ("return 1 +", "Unexpected end of input"),
            (
                "return 1; }\n",
                "Unmatched '}': the body closes before its end",
            ),
        ] {
            let diagnostic = plain(source).await.unwrap_err().diagnostic();
            let expected = format!("SyntaxError: {message} at end of source");
            assert_eq!(diagnostic.cause, Cause::Message(expected), "{source}");
        }
    }
}
