//! The controlling terminal of an SSH process whose connection types a secret: a
//! password, or a key's passphrase. OpenSSH reads it from its terminal, so the
//! process gets a terminal of its own while its standard streams stay pipes: a
//! pseudo-terminal on unix, a pseudo console on Windows.
//! A connection types at most one secret and never answers keyboard-interactive
//! authentication, so only the local OpenSSH writes to the terminal: its first
//! output is the secret's prompt, where the secret is typed once, whatever the
//! prompt says. A hop may never prompt, as when its key is already in the agent;
//! output after the answered line ends the connection.

use std::{io, sync::Arc};

use tokio::sync::OnceCell;
use zeroize::Zeroizing;

use crate::{
    origin::error::{HopPasswordError, MAX_PASSWORD_BYTES, PromptFailure, SshError},
    process::Console,
    secret::{Credential, Secret},
};

/// The most of the answered prompt kept to report a later one.
const PROMPT_BYTES: usize = 1024;

/// One connection attempt's secret, read once by whichever of its SSH processes
/// needs it first and forgotten with the attempt.
#[derive(Clone, Default)]
pub struct ConnectionSecret(Arc<OnceCell<Secret>>);

impl ConnectionSecret {
    /// `target`'s secret, read from `credential` on this machine.
    pub(super) async fn read(
        &self,
        target: &str,
        credential: &Credential,
    ) -> Result<Secret, SshError> {
        let failed = |error| SshError::Password {
            target: target.to_owned(),
            error,
        };
        let read = || async {
            let secret = (credential.resolve().await).map_err(|error| failed(error.into()))?;
            if secret.chars().any(char::is_control) {
                return Err(failed(HopPasswordError::Control));
            }
            if secret.len() > MAX_PASSWORD_BYTES {
                return Err(failed(HopPasswordError::TooLong));
            }
            Ok(secret)
        };
        self.0.get_or_try_init(read).await.cloned()
    }
}

/// What the terminal has shown, against its one secret.
enum Answer {
    /// Until the first output, the secret's prompt.
    Unprompted(Secret),
    /// Typed at the prompt shown. OpenSSH ends the answered line itself, its prompt
    /// having turned echo off; an echo, were echo on, would end with it.
    Answering(String),
    /// Any more output is another prompt.
    Answered(String),
}

enum Step {
    Wait,
    Type(Zeroizing<Vec<u8>>),
    /// Another prompt, after the one answered.
    Fail(String),
}

impl Answer {
    /// Take what the terminal showed since the last read, whole.
    fn read(&mut self, shown: &[u8]) -> Step {
        match self {
            Self::Unprompted(secret) => {
                let mut typed = Zeroizing::new(Vec::with_capacity(secret.len() + 1));
                typed.extend_from_slice(secret.as_bytes());
                typed.push(Console::ENTER);
                let prompt = String::from_utf8_lossy(&shown[..shown.len().min(PROMPT_BYTES)]);
                *self = Self::Answering(prompt.trim().to_owned());
                Step::Type(typed)
            }
            Self::Answering(prompt) => match shown.iter().position(|&byte| byte == b'\n') {
                None => Step::Wait,
                Some(end) if end + 1 == shown.len() => {
                    *self = Self::Answered(std::mem::take(prompt));
                    Step::Wait
                }
                Some(_) => Step::Fail(std::mem::take(prompt)),
            },
            Self::Answered(prompt) => Step::Fail(std::mem::take(prompt)),
        }
    }
}

/// An SSH process's controlling terminal, typing one hop's secret.
pub(super) struct Terminal {
    pub(super) console: Console,
    target: String,
    answer: Answer,
}

impl Terminal {
    /// A terminal typing `target`'s `secret`.
    pub(super) fn open(target: String, secret: Secret) -> io::Result<Self> {
        Ok(Self {
            console: Console::open()?,
            target,
            answer: Answer::Unprompted(secret),
        })
    }

    /// Answer the prompt until output fails the connection.
    pub(super) async fn answer(&mut self) -> PromptFailure {
        loop {
            let shown = match self.console.shown().await {
                Ok(Some(shown)) => shown,
                Ok(None) => return std::future::pending().await,
                Err(error) => return PromptFailure::Terminal(error.into()),
            };
            match self.answer.read(&shown) {
                Step::Wait => {}
                Step::Fail(prompt) => {
                    let target = self.target.clone();
                    return PromptFailure::Repeated { target, prompt };
                }
                Step::Type(typed) => {
                    if let Err(error) = self.console.type_in(&typed).await {
                        return PromptFailure::Terminal(error.into());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        origin::error::TransportError,
        origin::ssh::{config::tests::hop, process::tests::start, route::SshRoute},
        tests::bounded,
    };
    use std::assert_matches;
    use tokio::io::AsyncReadExt as _;

    #[tokio::test]
    async fn untypable_or_unreadable_secrets_fail_naming_the_target() {
        let read = async |credential| ConnectionSecret::default().read("db", &credential).await;
        for (password, expected) in [
            ("a\nb".into(), HopPasswordError::Control),
            ("x".repeat(1024), HopPasswordError::TooLong),
        ] {
            let Err(SshError::Password { error, .. }) = read(Credential::Literal(password)).await
            else {
                panic!("an untypable password was accepted");
            };
            assert_eq!(error, expected);
        }
        let unset = read(Credential::Env("SKYHOOK_TEST_UNSET_PW".into())).await;
        let expected = "target `db` password from env SKYHOOK_TEST_UNSET_PW: not set";
        assert_eq!(unset.unwrap_err().to_string(), expected);
    }

    /// SSH's own terminal is a pseudo-terminal it reads and writes as `/dev/tty`.
    /// Like OpenSSH, the stand-in turns echo off, prompts, reads the answer and ends
    /// its line, then runs `after`.
    async fn prompted(after: &str) -> (tempfile::TempDir, Result<String, TransportError>) {
        let route = SshRoute::Secret {
            before: Vec::new(),
            hop: hop("db", serde_json::json!({})),
            credential: Credential::Literal("pw".into()),
            after: Vec::new(),
        };
        let script = format!(
            "stty -echo </dev/tty\nprintf 'db password: ' >/dev/tty\nread -r answer </dev/tty\nprintf '\\n' >/dev/tty\n{after}"
        );
        let (bin, mut transport) = start(&script, &route, &Default::default()).await;
        let mut output = String::new();
        let read = bounded(transport.output.read_to_string(&mut output)).await;
        (bin, read.map(|_| output).map_err(TransportError::from))
    }

    #[tokio::test]
    async fn the_secret_is_typed_once_at_the_first_output() {
        let (_bin, output) = prompted("echo \"$answer\"").await;
        assert_eq!(output.unwrap(), "pw\n");
    }

    #[tokio::test]
    async fn output_after_the_answered_line_fails_the_connection() {
        let (_bin, output) = prompted("printf 'again: ' >/dev/tty\nread -r again </dev/tty").await;
        let Err(TransportError::Ssh(SshError::Prompt(failure))) = output else {
            panic!("{output:?}");
        };
        assert_matches!(&failure, PromptFailure::Repeated { prompt, .. } if prompt == "db password:");
    }
}
