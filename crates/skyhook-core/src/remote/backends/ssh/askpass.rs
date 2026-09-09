use crate::remote::{
    SecretValue, SensitivePrompt, SensitivePromptHandler, SensitivePromptKind, prompt::PromptAnswer,
};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{UnixListener, UnixStream},
};
use zeroize::Zeroizing;

pub(crate) struct AskpassServer {
    pub socket: PathBuf,
    pub executable: PathBuf,
    _directory: tempfile::TempDir,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Deserialize, Serialize)]
struct AskpassRequest {
    prompt: String,
    hint: Option<String>,
}

impl AskpassServer {
    pub fn start(handler: Arc<dyn SensitivePromptHandler>) -> Result<Self, std::io::Error> {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::Builder::new()
            .prefix("skyhook-askpass-")
            .tempdir()?;
        let socket = directory.path().join("askpass.sock");
        let executable = directory.path().join("askpass");
        let helper = std::env::current_exe()?;
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nexec {} --askpass \"$@\"\n",
                super::config::shell_quote(&helper.to_string_lossy())
            ),
        )?;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
        let listener = UnixListener::bind(&socket)?;
        let task = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let handler = handler.clone();
                        tasks.spawn(async move { let _ = serve_one(stream, handler).await; });
                    }
                    _ = tasks.join_next(), if !tasks.is_empty() => {}
                }
            }
        });
        Ok(Self {
            socket,
            executable,
            _directory: directory,
            task,
        })
    }

    pub fn environment(&self) -> std::collections::BTreeMap<String, String> {
        std::collections::BTreeMap::from([
            (
                "SSH_ASKPASS".into(),
                self.executable.to_string_lossy().into_owned(),
            ),
            ("SSH_ASKPASS_REQUIRE".into(), "force".into()),
            (
                "SKYHOOK_ASKPASS_SOCKET".into(),
                self.socket.to_string_lossy().into_owned(),
            ),
            ("DISPLAY".into(), "skyhook".into()),
        ])
    }
}

impl Drop for AskpassServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_one(
    mut stream: UnixStream,
    handler: Arc<dyn SensitivePromptHandler>,
) -> Result<(), std::io::Error> {
    let mut bytes = Vec::new();
    (&mut stream)
        .take(64 * 1024)
        .read_to_end(&mut bytes)
        .await?;
    let request: AskpassRequest = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
    let kind = classify(&request.prompt, request.hint.as_deref());
    let peer = stream.peer_cred()?.pid();
    let gone = async {
        let Some(pid) = peer else {
            std::future::pending::<()>().await;
            return;
        };
        loop {
            // SAFETY: signal zero only checks whether the peer process still exists.
            if unsafe { libc::kill(pid, 0) } != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    };
    let answer = tokio::select! {
        result = handler.prompt(SensitivePrompt {kind, message:request.prompt}) => match result {
            Ok(value) => PromptAnswer::Accepted(value), Err(_) => PromptAnswer::Rejected,
        },
        () = gone => PromptAnswer::Rejected,
    };
    let bytes = Zeroizing::new(serde_json::to_vec(&answer).map_err(std::io::Error::other)?);
    stream.write_all(&bytes).await?;
    stream.shutdown().await
}

fn classify(prompt: &str, hint: Option<&str>) -> SensitivePromptKind {
    let lower = prompt.to_ascii_lowercase();
    if lower.contains("yes/no") || lower.contains("authenticity of host") {
        SensitivePromptKind::HostConfirmation
    } else if hint == Some("confirm") {
        SensitivePromptKind::AgentConfirmation
    } else if lower.contains("passphrase") {
        SensitivePromptKind::KeyPassphrase
    } else if lower.contains("password") {
        SensitivePromptKind::Password
    } else {
        SensitivePromptKind::KeyboardInteractive
    }
}

pub fn run_helper(socket: &Path, prompt: String) -> Result<(), Box<dyn std::error::Error>> {
    let hint = std::env::var("SSH_ASKPASS_PROMPT").ok();
    let confirmation = classify(&prompt, hint.as_deref()) == SensitivePromptKind::AgentConfirmation;
    let request = serde_json::to_vec(&AskpassRequest { prompt, hint })?;
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    stream.write_all(&request)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut response = Zeroizing::new(Vec::new());
    stream.take(64 * 1024).read_to_end(&mut response)?;
    if let Some(value) = answer_value(
        serde_json::from_slice::<PromptAnswer>(&response)?,
        confirmation,
    )? {
        std::io::stdout().write_all(value.expose().as_bytes())?;
    }
    Ok(())
}

fn answer_value(
    answer: PromptAnswer,
    confirmation: bool,
) -> Result<Option<SecretValue>, &'static str> {
    match answer {
        PromptAnswer::Accepted(value) if confirmation => {
            if matches!(
                value.expose().trim().to_ascii_lowercase().as_str(),
                "yes" | "y"
            ) {
                Ok(None)
            } else {
                Err("authentication confirmation declined")
            }
        }
        PromptAnswer::Accepted(value) => Ok(Some(value)),
        PromptAnswer::Rejected => {
            Err("authentication interaction unavailable, declined or cancelled")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classifies_openssh_prompts_and_confirmation_hints() {
        assert_eq!(classify("Password:", None), SensitivePromptKind::Password);
        assert_eq!(
            classify("Enter passphrase for key", None),
            SensitivePromptKind::KeyPassphrase
        );
        assert_eq!(
            classify("Are you sure (yes/no)?", None),
            SensitivePromptKind::HostConfirmation
        );
        assert_eq!(
            classify("Allow use of key?", Some("confirm")),
            SensitivePromptKind::AgentConfirmation
        );
    }
    #[tokio::test]
    async fn rejected_prompts_are_distinct_from_empty_secrets() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let task = tokio::spawn(serve_one(
            server,
            Arc::new(crate::remote::RejectSensitivePrompts),
        ));
        client
            .write_all(br#"{"prompt":"Password:","hint":null}"#)
            .await
            .unwrap();
        client.shutdown().await.unwrap();
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).await.unwrap();
        assert!(matches!(
            serde_json::from_slice::<PromptAnswer>(&bytes).unwrap(),
            PromptAnswer::Rejected
        ));
        task.await.unwrap().unwrap();
        assert!(matches!(
            serde_json::from_str::<PromptAnswer>(r#"{"Accepted":""}"#).unwrap(),
            PromptAnswer::Accepted(_)
        ));
    }
    #[test]
    fn confirmation_denial_is_failure_and_empty_password_is_success() {
        assert!(answer_value(PromptAnswer::Accepted(SecretValue::new("no".into())), true).is_err());
        assert!(
            answer_value(PromptAnswer::Accepted(SecretValue::new("yes".into())), true)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            answer_value(
                PromptAnswer::Accepted(SecretValue::new(String::new())),
                false
            )
            .unwrap()
            .unwrap()
            .expose(),
            ""
        );
        let answer = PromptAnswer::Accepted(SecretValue::new("never-print-this".into()));
        assert!(!format!("{answer:?}").contains("never-print-this"));
    }
}
