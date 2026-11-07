//! Invocation-local environment defaults, loaded before any worker threads exist.
use std::{fmt, fs::File, io::Read};

#[derive(Debug)]
pub(super) enum LoadError {
    Read,
    Parse,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Dotenv parser errors include the original line (often a credential).
        // Never retain or display parser errors, file contents, or variable values.
        f.write_str(match self {
            Self::Read => "could not read invocation directory .env file",
            Self::Parse => "invalid invocation directory .env file",
        })
    }
}

/// Load exactly `./.env`, without walking ancestors or consulting config/workspace.
/// Existing variables, including empty and non-Unicode values, take precedence.
///
/// # Safety
/// Must only be called during single-threaded process startup, before any thread
/// or runtime is started: mutating the process environment is otherwise unsafe.
pub(super) unsafe fn load_invocation_env() -> Result<(), LoadError> {
    let mut file = match File::open(".env") {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(LoadError::Read),
    };
    let mut source = String::new();
    file.read_to_string(&mut source)
        .map_err(|_| LoadError::Read)?;
    for entry in entries(&source) {
        let (key, value) = entry?;
        if std::env::var_os(&key).is_none() {
            // SAFETY: guaranteed by this function's startup-only caller contract.
            unsafe { std::env::set_var(key, value) };
        }
    }
    Ok(())
}

/// The variables `source` sets, in order.
fn entries(source: &str) -> impl Iterator<Item = Result<(String, String), LoadError>> {
    // The iterator does not strip a UTF-8 BOM, unlike dotenvy's load helpers.
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    dotenvy::from_read_iter(source.as_bytes()).map(|entry| {
        let (key, value) = entry.map_err(|_| LoadError::Parse)?;
        // set_var panics on NUL; reject it without exposing a secret in a panic.
        if key.contains('\0') || value.contains('\0') {
            return Err(LoadError::Parse);
        }
        Ok((key, value))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_parse_dotenv_syntax_and_reject_malformed_files() {
        let parsed: Vec<_> = entries("\u{feff}export KEY=\"value\" # comment\nOTHER=plain\n")
            .collect::<Result<_, _>>()
            .unwrap();
        let expected = [("KEY", "value"), ("OTHER", "plain")]
            .map(|(key, value)| (key.to_owned(), value.to_owned()));
        assert_eq!(parsed, expected);
        for source in ["TOKEN='unterminated\n", "TOKEN=nul\0inside\n"] {
            assert!(entries(source).any(|entry| entry.is_err()), "{source:?}");
        }
    }
}
