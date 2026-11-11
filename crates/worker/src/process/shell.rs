//! Commands: shell text or a program and its arguments, and the processes they run,
//! shell text in sh on unix and in Windows PowerShell 5.1 on Windows.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use skyhook_base::decode::{Context, Decode, Node, Value};

use crate::op::Program;

// A command: shell text, or a program and its arguments.
#[derive(Clone, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
#[serde(untagged)]
#[schemars(inline)]
pub enum CommandLine {
    /// Shell text
    #[schemars(extend("minLength" = 1))]
    Shell(String),
    /// Program and arguments without shell parsing, for example `["cargo","test"]`
    #[schemars(extend("minItems" = 1))]
    Argv(Vec<String>),
}

/// Why a command line names no process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidCommand {
    #[error("`command` is a JSON array encoded as a string; send `command` as an array")]
    EncodedArray,
    #[error("command cannot be empty")]
    Empty,
}

/// How shell text runs: `Login` reads the login profile first (`/bin/sh -lc`),
/// `Plain` does not (`/bin/sh -c`). On Windows both run in Windows PowerShell 5.1,
/// which every supported release ships, without a profile.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Shell {
    Login,
    Plain,
}

impl Shell {
    #[cfg(unix)]
    fn command(self, text: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("/bin/sh");
        let flag = match self {
            Self::Login => "-lc",
            Self::Plain => "-c",
        };
        command.arg(flag).arg(text);
        command
    }

    #[cfg(windows)]
    fn command(self, text: &str) -> tokio::process::Command {
        // Neither reads a profile.
        let (Self::Login | Self::Plain) = self;
        let mut command = tokio::process::Command::new("powershell.exe");
        command
            .args(["-NoProfile", "-NonInteractive", "-Command"])
            .arg(powershell_script(text));
        command
    }
}

impl CommandLine {
    /// The program this command line runs.
    pub fn program(&self) -> Result<Program, InvalidCommand> {
        match self {
            // A model sometimes encodes argv as a string; running it through the
            // shell would execute a bracketed word rather than the intended program.
            Self::Shell(text) if serde_json::from_str::<Vec<String>>(text).is_ok() => {
                Err(InvalidCommand::EncodedArray)
            }
            Self::Shell(text) if !text.is_empty() => Ok(Program::Shell(text.clone())),
            Self::Argv(argv) if let Some((program, args)) = argv.split_first() => {
                Ok(Program::Argv {
                    program: program.clone(),
                    args: args.to_vec(),
                })
            }
            Self::Shell(_) | Self::Argv(_) => Err(InvalidCommand::Empty),
        }
    }
}

impl Program {
    /// The process this program runs, shell text as `shell` runs it.
    pub(crate) fn command(&self, shell: Shell) -> tokio::process::Command {
        match self {
            Self::Shell(text) => shell.command(text),
            Self::Argv { program, args } => {
                let mut command = tokio::process::Command::new(program);
                command.args(args);
                command
            }
        }
    }
}

/// `text` as a Windows PowerShell 5.1 script that behaves as sh would run it, wherever
/// the script is written: console output, and text piped to programs, is UTF-8 without
/// a byte-order mark; progress records would otherwise reach stderr as serialized
/// objects; and the exit status is the last command's, a native program's code passing
/// through. Newlines, never `;`, join the parts, so the text may end in a here-string.
///
/// The text runs as a script block piped to `Out-Default`, which writes the objects it
/// formats before the epilogue's `exit` ends the process; the block records its own
/// last status, which would otherwise be `Out-Default`'s. A terminating error or a
/// top-level `return` leaves that status unset, failing the script.
pub fn powershell_script(text: &str) -> String {
    const PREAMBLE: &str = "$ProgressPreference = 'SilentlyContinue'\n\
        [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)\n\
        $OutputEncoding = [Console]::OutputEncoding\n\
        $global:LASTEXITCODE = 0\n\
        $global:skyhookOk = $false\n\
        & {\n";
    const EPILOGUE: &str = "\n$global:skyhookOk = $?\n\
        } | Out-Default\n\
        if ($global:skyhookOk) { exit 0 }\n\
        if ($LASTEXITCODE) { exit $LASTEXITCODE }\n\
        exit 1";
    format!("{PREAMBLE}{text}{EPILOGUE}")
}

/// Shell text as a string, a program and its arguments as a list.
impl Decode for CommandLine {
    fn decode(node: &Node, cx: &mut Context) -> Option<Self> {
        match node.value {
            Value::Str(_) => String::decode(node, cx).map(Self::Shell),
            Value::Seq(_) => Vec::decode(node, cx).map(Self::Argv),
            _ => {
                cx.expected(node, "shell text or a list of a program and its arguments");
                None
            }
        }
    }
}
