//! Configured credentials: written literally, read from the environment or a private
//! file, or printed by a command, on the machine that uses them.

use std::{fmt, path::PathBuf, process::Stdio};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt as _;
use zeroize::Zeroizing;

use crate::{
    op::{FsError, TargetPath},
    process::{CommandLine, InvalidCommand, Shell},
    target::{Os, Platform},
};
use skyhook_base::{
    decode::{self, Context, Decode, Node, one_of},
    enums::{Kinds, NamedEnum},
};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows::check_private;

/// A value read from its source, erased from memory when dropped.
pub type Secret = Zeroizing<String>;

/// Credentials should be small; cap even unsuccessful or never-ending output.
const MAX_COMMAND_OUTPUT: usize = 64 * 1024;
const MAX_FILE_BYTES: u64 = 4096;

// A configured credential and where it is read from.
#[derive(Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Kinds)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[schemars(inline)]
#[kinds(name = CredentialKind)]
pub enum Credential {
    /// Environment variable holding the value
    Env(String),
    /// Command printing the value; text runs with the origin's shell
    Command(CommandLine),
    /// Private file (readable only by the origin's user) holding the value; ~/ is the home directory
    File(TargetPath),
    /// The value itself
    #[serde(untagged)]
    Literal(String),
}

/// The sources a credential mapping can name: every kind but the literal value.
fn sources() -> impl Iterator<Item = CredentialKind> {
    (CredentialKind::ALL.into_iter()).filter(|kind| *kind != CredentialKind::Literal)
}

/// A string is the value itself; a mapping names exactly one source, each refusal
/// where it is written.
impl Decode for Credential {
    fn decode(node: &Node, cx: &mut Context) -> Option<Self> {
        if let Some(text) = node.as_str() {
            return Some(Self::Literal(text.to_owned()));
        }
        let names = Vec::from_iter(sources().map(CredentialKind::as_str));
        if node.as_map().is_none() {
            let expected = one_of(&names);
            cx.expected(
                node,
                &format!("a literal string or a mapping with one of {expected}"),
            );
            return None;
        }
        decode::variant(
            node,
            cx,
            &names,
            |key, value, cx| match CredentialKind::parse(key)? {
                CredentialKind::Env => String::decode(value, cx).map(Self::Env),
                CredentialKind::Command => CommandLine::decode(value, cx).map(Self::Command),
                CredentialKind::File => TargetPath::decode(value, cx).map(Self::File),
                CredentialKind::Literal => unreachable!("a mapping names no literal"),
            },
        )
    }
}

/// A literal may be a secret and a command may name one's location; only the kind
/// of source, the variable's name or the file's path is shown.
impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(_) => f.write_str("Literal(..)"),
            Self::Env(name) => f.debug_struct("Env").field("name", name).finish(),
            Self::Command(_) => f.write_str("Command(..)"),
            Self::File(path) => f.debug_struct("File").field("path", path).finish(),
        }
    }
}

/// A credential's source as an error names it: an environment variable by its name,
/// any other source by its kind alone.
impl fmt::Display for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Env(name) => write!(f, "env {name}"),
            Self::Command(_) => f.write_str("command output"),
            Self::File(_) => f.write_str("file contents"),
            Self::Literal(_) => f.write_str("literal"),
        }
    }
}

/// Why a configured credential was refused before it is read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SourceProblem {
    #[error("must not be blank")]
    Blank,
    #[error("env must name a nonempty environment variable")]
    InvalidEnvName,
    #[error(transparent)]
    Command(#[from] InvalidCommand),
    #[error("file must be an absolute path or start with ~/")]
    RelativeFile,
}

/// Why a credential could not be had, naming its source. Never carries the value,
/// a command's text or output, or a file's contents.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, serde::Deserialize, serde::Serialize)]
pub enum ResolveError {
    #[error("from env {0}: not set")]
    NotSet(String),
    #[error("from command: {0}")]
    Command(CommandProblem),
    #[error("from file: {0}")]
    File(FileProblem),
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, thiserror::Error, serde::Deserialize, serde::Serialize,
)]
pub enum CommandProblem {
    #[error("could not be run")]
    NotRun,
    #[error("exited unsuccessfully")]
    Failed,
    #[error("output exceeded {MAX_COMMAND_OUTPUT} bytes")]
    TooLarge,
    #[error(transparent)]
    Text(#[from] TextProblem),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, serde::Deserialize, serde::Serialize)]
pub enum FileProblem {
    #[error("starts with ~/, but there is no home directory")]
    NoHome,
    #[error("{}", file_failure(.0))]
    File(#[from] FsError),
    #[error("not owned by you")]
    NotOwned,
    #[error("accessible to other users; restrict it with chmod 600")]
    Shared,
    #[error("owned by {0}, not by you, SYSTEM or Administrators")]
    ForeignOwner(Sid),
    #[error("grants access to {0}; allow only you, SYSTEM and Administrators")]
    Granted(Sid),
    #[error(
        "has an access entry of type {0} that cannot be checked; allow only you, SYSTEM and Administrators"
    )]
    Unchecked(u8),
    #[error(transparent)]
    Text(#[from] TextProblem),
}

/// A credential file's own failure: its size against the credential limit, and an OS
/// error by its kind alone.
fn file_failure(error: &FsError) -> String {
    match error {
        FsError::TooLarge { .. } => format!("exceeds the {MAX_FILE_BYTES}-byte limit"),
        FsError::Os(error) => format!("could not be read: {}", error.kind.io()),
        error @ (FsError::NotRegular | FsError::Live(_)) => error.to_string(),
    }
}

/// A Windows security identifier in its text form, such as `S-1-5-32-545`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Sid(pub String);

impl fmt::Display for Sid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why printed or stored text holds no value.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, thiserror::Error, serde::Deserialize, serde::Serialize,
)]
pub enum TextProblem {
    #[error("output was not valid UTF-8")]
    NotUtf8,
    #[error("output was empty")]
    Empty,
}

impl Credential {
    /// Check the credential as configured, to be read on a machine running `reader`,
    /// without reading the environment, running a command or opening a file.
    pub fn admit(&self, reader: Os) -> Result<(), SourceProblem> {
        match self {
            Self::Literal(value) if value.trim().is_empty() => Err(SourceProblem::Blank),
            Self::Env(name) if name.trim().is_empty() || name.contains(['=', '\0']) => {
                Err(SourceProblem::InvalidEnvName)
            }
            Self::Command(command) => command.program().map(drop).map_err(Into::into),
            Self::File(file)
                if !file.is_absolute(reader) && file.home_relative(reader).is_none() =>
            {
                Err(SourceProblem::RelativeFile)
            }
            _ => Ok(()),
        }
    }

    /// A literal's text.
    pub fn literal(&self) -> Option<&str> {
        match self {
            Self::Literal(value) => Some(value),
            Self::Env(_) | Self::Command(_) | Self::File(_) => None,
        }
    }

    /// The credential as a table's columns hold it: its kind, the text of a literal, a
    /// variable or shell text, a file's path, and a program's argument vector.
    pub fn parts(&self) -> (CredentialKind, Option<&str>, Option<&TargetPath>, &[String]) {
        let (text, file, argv) = match self {
            Self::Literal(text) | Self::Env(text) | Self::Command(CommandLine::Shell(text)) => {
                (Some(text.as_str()), None, &[][..])
            }
            Self::Command(CommandLine::Argv(argv)) => (None, None, &argv[..]),
            Self::File(path) => (None, Some(path), &[][..]),
        };
        (self.kind(), text, file, argv)
    }

    /// The credential whose [`Self::parts`] these are; none when they describe none.
    pub fn from_parts(
        kind: CredentialKind,
        text: Option<String>,
        file: Option<TargetPath>,
        argv: Vec<String>,
    ) -> Option<Self> {
        Some(match (kind, text, file, argv.is_empty()) {
            (CredentialKind::Literal, Some(value), None, true) => Self::Literal(value),
            (CredentialKind::Env, Some(name), None, true) => Self::Env(name),
            (CredentialKind::Command, Some(text), None, true) => {
                Self::Command(CommandLine::Shell(text))
            }
            (CredentialKind::Command, None, None, _) => Self::Command(CommandLine::Argv(argv)),
            (CredentialKind::File, None, Some(path), true) => Self::File(path),
            _ => return None,
        })
    }

    /// The value, when reading it starts no process and opens no file: a literal's
    /// or an environment variable's. Others are read by [`Self::resolve`] alone.
    pub fn read_now(&self) -> Option<Result<Secret, ResolveError>> {
        match self {
            Self::Literal(value) => Some(Ok(Zeroizing::new(value.clone()))),
            Self::Env(name) => Some(match std::env::var(name) {
                Ok(value) if !value.is_empty() => Ok(Zeroizing::new(value)),
                _ => Err(ResolveError::NotSet(name.clone())),
            }),
            Self::Command(_) | Self::File(_) => None,
        }
    }

    /// As configured, but a literal's text replaced by its kind's name.
    pub fn redacted(&self) -> Self {
        match self {
            Self::Literal(_) => Self::Literal(CredentialKind::Literal.to_string()),
            credential => credential.clone(),
        }
    }

    /// The value, read now.
    pub async fn resolve(&self) -> Result<Secret, ResolveError> {
        match self {
            Self::Command(command) => run(command).await.map_err(ResolveError::Command),
            Self::File(file) => read_file(file).await.map_err(ResolveError::File),
            Self::Literal(_) | Self::Env(_) => self.read_now().expect("read without waiting"),
        }
    }
}

/// Stdout of a successful run. Stdin is closed and stderr discarded.
async fn run(command: &CommandLine) -> Result<Secret, CommandProblem> {
    let program = command.program().map_err(|_| CommandProblem::NotRun)?;
    let mut command = program.command(Shell::Plain);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = crate::process::spawn(&mut command).map_err(|_| CommandProblem::NotRun)?;
    let stdout = child.stdout.take().ok_or(CommandProblem::NotRun)?;
    let mut bytes = Zeroizing::new(Vec::new());
    stdout
        .take((MAX_COMMAND_OUTPUT + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| CommandProblem::NotRun)?;
    if bytes.len() > MAX_COMMAND_OUTPUT {
        return Err(CommandProblem::TooLarge);
    }
    if !child
        .wait()
        .await
        .map_err(|_| CommandProblem::NotRun)?
        .success()
    {
        return Err(CommandProblem::Failed);
    }
    Ok(value(&bytes)?)
}

/// A private file's contents.
async fn read_file(path: &TargetPath) -> Result<Secret, FileProblem> {
    let path = expand_home(path).ok_or(FileProblem::NoHome)?;
    let file = crate::fs::open_regular(&path, MAX_FILE_BYTES).await?;
    check_private(&file)?;
    let bytes = crate::fs::read_to_limit(file, MAX_FILE_BYTES).await?;
    Ok(value(&Zeroizing::new(bytes))?)
}

/// `path` with a leading `~/` naming the home directory, as OpenSSH reads key paths, by
/// this machine's rules; none when there is no home directory.
pub(crate) fn expand_home(path: &TargetPath) -> Option<PathBuf> {
    match path.home_relative(Platform::current().os) {
        Some(rest) => {
            let mut expanded = std::env::home_dir()?.into_os_string();
            expanded.push(rest);
            Some(expanded.into())
        }
        None => Some(path.native().to_owned()),
    }
}

/// Like OpenSSH for private keys, refuse a file other users could read or replace.
#[cfg(unix)]
fn check_private(file: &std::fs::File) -> Result<(), FileProblem> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = file
        .metadata()
        .map_err(|error| FileProblem::File(error.into()))?;
    // SAFETY: geteuid has no preconditions and does not mutate process state.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(FileProblem::NotOwned);
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(FileProblem::Shared);
    }
    Ok(())
}

/// Produced text without one trailing line ending, which must leave more than
/// whitespace.
fn value(bytes: &[u8]) -> Result<Secret, TextProblem> {
    let text = std::str::from_utf8(bytes).map_err(|_| TextProblem::NotUtf8)?;
    let text = text.strip_suffix('\n').unwrap_or(text);
    match text.strip_suffix('\r').unwrap_or(text) {
        text if text.trim().is_empty() => Err(TextProblem::Empty),
        text => Ok(Zeroizing::new(text.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_parse_one_named_source_or_a_literal() {
        let shell = CommandLine::Shell("pass show x".into());
        let argv = CommandLine::Argv(vec!["pass".into(), "show".into(), "x".into()]);
        for (text, expected) in [
            (r#""plain""#, Credential::Literal("plain".into())),
            (r#"{"env": "KEY"}"#, Credential::Env("KEY".into())),
            (r#"{"command": "pass show x"}"#, Credential::Command(shell)),
            (
                r#"{"command": ["pass", "show", "x"]}"#,
                Credential::Command(argv),
            ),
            (r#"{"file": "~/pw"}"#, Credential::File("~/pw".into())),
        ] {
            let parsed = serde_json::from_str::<Credential>(text).unwrap();
            assert_eq!(parsed, expected, "{text}");
            let written = serde_json::to_value(&parsed).unwrap();
            assert_eq!(
                serde_json::from_value::<Credential>(written).unwrap(),
                parsed
            );
            assert_eq!(parsed.admit(Os::Linux), Ok(()), "{text}");
        }
        for (text, problem) in [
            (r#"" ""#, SourceProblem::Blank),
            (r#"{"env": "A=B"}"#, SourceProblem::InvalidEnvName),
            (r#"{"command": ""}"#, InvalidCommand::Empty.into()),
            (r#"{"command": []}"#, InvalidCommand::Empty.into()),
            (r#"{"file": "pw"}"#, SourceProblem::RelativeFile),
        ] {
            let parsed = serde_json::from_str::<Credential>(text).unwrap();
            assert_eq!(parsed.admit(Os::Linux), Err(problem), "{text}");
            // What is refused is still a credential a table's columns hold.
            let (kind, value, file, argv) = parsed.parts();
            let (value, file) = (value.map(str::to_owned), file.cloned());
            let held = Credential::from_parts(kind, value, file, argv.to_vec());
            assert_eq!(held.as_ref(), Some(&parsed), "{text}");
        }
        for text in [r#"{"bogus": "x"}"#, r#"{"env": "A", "file": "/b"}"#, "{}"] {
            assert!(serde_json::from_str::<Credential>(text).is_err(), "{text}");
        }
    }

    /// A file is admitted as home-relative exactly when this machine reads it from the
    /// home directory.
    #[test]
    fn home_relative_files_are_the_ones_read_from_home() {
        let home = std::env::home_dir().unwrap();
        for path in ["~/pw", "~//pw", "~pw", "~", "~user/pw", r"~\pw"] {
            let file = Credential::File(path.into());
            let admitted = file.admit(Platform::current().os).is_ok();
            let expanded = expand_home(&path.into()).unwrap();
            assert_eq!(admitted, expanded.starts_with(&home), "{path}");
        }
    }

    #[tokio::test]
    async fn commands_and_files_drop_one_line_ending_and_files_must_be_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir().unwrap();
        let file = |name: &str, contents: &[u8], mode: u32| {
            let path = directory.path().join(name);
            std::fs::write(&path, contents).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            Credential::File(path.to_str().unwrap().into())
        };
        let argv = |argv: &[&str]| {
            let argv = argv.iter().map(|&arg| arg.to_owned()).collect();
            Credential::Command(CommandLine::Argv(argv))
        };
        for (source, expected) in [
            (file("crlf", b" pass word \r\n\n", 0o600), " pass word \r\n"),
            (file("bare", b"pw\r\n", 0o400), "pw"),
            (argv(&["printf", "%s\\n", "a b"]), "a b"),
        ] {
            assert_eq!(source.resolve().await.unwrap().as_str(), expected);
        }
        let file_problem = |problem| ResolveError::File(problem);
        let empty = TextProblem::Empty.into();
        for (source, expected) in [
            (
                file("shared", b"pw", 0o640),
                file_problem(FileProblem::Shared),
            ),
            (
                file("large", &[b'x'; 4097], 0o600),
                file_problem(
                    FsError::TooLarge {
                        limit: MAX_FILE_BYTES,
                    }
                    .into(),
                ),
            ),
            (
                file("binary", b"\xff", 0o600),
                file_problem(TextProblem::NotUtf8.into()),
            ),
            (file("blank", b" \n", 0o600), file_problem(empty)),
            (
                Credential::File(directory.path().to_str().unwrap().into()),
                file_problem(FsError::NotRegular.into()),
            ),
            (
                Credential::File(directory.path().join("absent").to_str().unwrap().into()),
                file_problem(FsError::from(std::io::Error::from_raw_os_error(libc::ENOENT)).into()),
            ),
            (
                argv(&["false"]),
                ResolveError::Command(CommandProblem::Failed),
            ),
            (
                argv(&["printf", "\\n"]),
                ResolveError::Command(TextProblem::Empty.into()),
            ),
        ] {
            assert_eq!(source.resolve().await.unwrap_err(), expected);
        }
    }
}
