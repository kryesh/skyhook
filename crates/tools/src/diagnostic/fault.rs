//! How a target's typed faults read, and what failed where only the tool can name it.
use skyhook_worker::diagnostic::{Fault, Operation, Subject};
use skyhook_worker::op::{IoKind, OsError, Pipe, Protected, SHOWN_LINES};

use super::Cause;

pub(super) fn render(fault: Fault) -> (Cause, Option<(Operation, Subject)>) {
    let message = |text: &str| Cause::Message(text.to_owned());
    let invalid = |text: &str| Cause::InvalidArguments(text.to_owned());
    let argument = |text: &str, pointer: &[&str]| {
        let named = (Operation::Validate, Subject::argument(pointer));
        (invalid(text), Some(named))
    };
    let cause = match fault {
        Fault::Os(error) => Cause::os(error),
        Fault::NotRegular => message("not a regular file"),
        Fault::TooLarge { limit } => Cause::Message(format!("file exceeds the {limit}-byte limit")),
        Fault::Live => Cause::Message(skyhook_worker::fs::LiveDatabase.to_string()),
        Fault::Unspelled { native } => Cause::Unspelled { native },
        Fault::EmptyPath => {
            let named = (Operation::Validate, Subject::argument(["path"]));
            return (message("path cannot be empty"), Some(named));
        }
        Fault::Protected(protected) => Cause::Message(format!(
            "cannot remove {} or a directory containing it",
            match protected {
                Protected::AuthorizationRoot => "the authorization root",
                Protected::Workspace => "the workspace",
            }
        )),
        Fault::NotText {
            valid_up_to,
            error_len: Some(length),
        } => Cause::Message(format!(
            "invalid utf-8 sequence of {length} bytes from index {valid_up_to}"
        )),
        Fault::NotText { valid_up_to, .. } => Cause::Message(format!(
            "incomplete utf-8 byte sequence from index {valid_up_to}"
        )),
        Fault::CountMismatch {
            expected,
            found,
            lines,
        } => Cause::Message(format!(
            "expected {expected} matches, found {found}{}",
            match_lines(&lines)
        )),
        Fault::EditTooLarge { bytes, limit } => Cause::Message(write_too_large(bytes, limit)),
        Fault::UnsupportedEntry => message("unsupported filesystem entry"),
        Fault::NotEmpty(error) => Cause::Io(OsError {
            kind: IoKind::DirectoryNotEmpty,
            detail: Some(
                "use recursive removal only if all directory contents should be deleted".into(),
            ),
            ..error
        }),
        Fault::Regex => return argument("regular expression could not be compiled", &["pattern"]),
        Fault::Globs(Some(index)) => {
            let index = index.to_string();
            return argument("glob pattern could not be compiled", &["glob", &index]);
        }
        Fault::Globs(None) => {
            let named = (Operation::Validate, Subject::Label("glob patterns".into()));
            return (invalid("glob patterns could not be compiled"), Some(named));
        }
        Fault::GlobPattern => return argument("glob pattern could not be compiled", &["pattern"]),
        Fault::SearchRoot => message("search root is not a file or directory"),
        Fault::GlobRoot => message("glob root is not a directory"),
        Fault::Loop => message("symbolic link loop"),
        Fault::Traversal => message("directory traversal failed"),
        Fault::NotDirectory => message("not a directory"),
        Fault::NoCacheDirectory => message("this user has no cache directory"),
        Fault::NotPrivate => message("not a directory only its user can use"),
        Fault::NotUnicode => message("this user's cache directory has a path that is not Unicode"),
        Fault::Pipe { pipe, error } => {
            let subject = Subject::Label(pipe_name(pipe).to_owned());
            return match error {
                None => (
                    message("pipe unavailable"),
                    Some((Operation::Prepare, subject)),
                ),
                // A pipe's I/O detail may hold what the process wrote.
                Some(error) => {
                    let cause = Cause::Io(OsError {
                        detail: None,
                        ..error
                    });
                    (cause, Some((Operation::Read, subject)))
                }
            };
        }
        Fault::InvalidProxy => invalid("invalid proxy URL"),
        Fault::Client => message("the HTTP client could not be built"),
        Fault::Tls => message("the TLS handshake failed"),
        Fault::ConnectTimeout => message("connect_timeout elapsed while connecting"),
        Fault::RequestTimeout => message("the request timed out"),
        Fault::Connect => message("connecting to the server failed"),
        Fault::Decode => message("decoding the response failed"),
        Fault::Body => message("the response body failed"),
        Fault::Request => message("the HTTP request failed"),
        Fault::UploadNotRegular => invalid("upload path must be a regular file"),
        Fault::TooManyRedirects => message("maximum redirects exceeded"),
        Fault::InvalidLocation => message("invalid redirect location header"),
        Fault::InvalidRedirectUrl => message("invalid redirect URL"),
        Fault::UnsupportedRedirectUrl => {
            message("redirect URL must be HTTP(S) without embedded credentials")
        }
        Fault::Downgrade => message("HTTPS to HTTP redirect blocked"),
        Fault::ResponseTooLarge => message("response exceeds max_bytes"),
        Fault::SaveExists => {
            invalid("save_to.path already exists; set save_to.overwrite to replace it")
        }
        Fault::SaveNotFile => invalid("save_to.path must be a regular file"),
        Fault::Timeout(after) => {
            Cause::Message(format!("the {} s timeout elapsed", after.as_secs()))
        }
    };
    (cause, None)
}

pub(crate) fn write_too_large(bytes: usize, limit: usize) -> String {
    format!("write is {bytes} bytes, exceeding the {limit}-byte limit")
}

fn pipe_name(pipe: Pipe) -> &'static str {
    match pipe {
        Pipe::Stdout => "stdout pipe",
        Pipe::Stderr => "stderr pipe",
    }
}

/// Starting lines of the first few matches.
fn match_lines(lines: &[usize]) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let more = if lines.len() > SHOWN_LINES {
        ", ..."
    } else {
        ""
    };
    let lines: Vec<_> = lines
        .iter()
        .take(SHOWN_LINES)
        .map(usize::to_string)
        .collect();
    format!(" at lines {}{more}", lines.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_lines_reports_starting_lines_and_caps_the_list() {
        assert_eq!(match_lines(&[]), "");
        assert_eq!(match_lines(&[1, 1, 4]), " at lines 1, 1, 4");
        assert_eq!(
            match_lines(&(1..=11).collect::<Vec<_>>()),
            " at lines 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, ..."
        );
    }
}
