use std::{
    io::{Read as _, Write as _},
    path::Path,
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{UnixListener, UnixStream},
};
use zeroize::Zeroizing;

use super::{SensitivePrompt, SensitivePromptHandler, SensitivePromptKind};

pub(super) struct AskpassServer {
    pub socket: std::path::PathBuf,
    _directory: tempfile::TempDir,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Deserialize, Serialize)]
struct AskpassRequest {
    prompt: String,
}

impl AskpassServer {
    pub fn start(
        handler: Arc<dyn SensitivePromptHandler>,
        allow_secrets: bool,
    ) -> Result<Self, std::io::Error> {
        let directory = tempfile::Builder::new()
            .prefix("skyhook-askpass-")
            .tempdir()?;
        let socket = directory.path().join("askpass.sock");
        let listener = UnixListener::bind(&socket)?;
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let handler = handler.clone();
                tokio::spawn(async move {
                    let _ = serve_one(stream, handler, allow_secrets).await;
                });
            }
        });
        Ok(Self {
            socket,
            _directory: directory,
            task,
        })
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
    allow_secrets: bool,
) -> Result<(), std::io::Error> {
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await?;
    let request: AskpassRequest = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
    let kind = classify(&request.prompt);
    if kind != SensitivePromptKind::HostConfirmation && !allow_secrets {
        return Ok(());
    }
    if let Ok(secret) = handler
        .prompt(SensitivePrompt {
            kind,
            message: request.prompt,
        })
        .await
    {
        stream.write_all(secret.expose().as_bytes()).await?;
        stream.flush().await?;
    }
    Ok(())
}

fn classify(prompt: &str) -> SensitivePromptKind {
    let lower = prompt.to_ascii_lowercase();
    if lower.contains("yes/no") || lower.contains("authenticity of host") {
        SensitivePromptKind::HostConfirmation
    } else if lower.contains("passphrase") {
        SensitivePromptKind::KeyPassphrase
    } else if lower.contains("password") {
        SensitivePromptKind::Password
    } else {
        SensitivePromptKind::KeyboardInteractive
    }
}

pub fn run_helper(socket: &Path, prompt: String) -> Result<(), Box<dyn std::error::Error>> {
    let request = serde_json::to_vec(&AskpassRequest { prompt })?;
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    stream.write_all(&request)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut response = Zeroizing::new(Vec::new());
    stream.read_to_end(&mut response)?;
    std::io::stdout().write_all(&response)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_openssh_prompts() {
        assert_eq!(classify("Password:"), SensitivePromptKind::Password);
        assert_eq!(
            classify("Enter passphrase for key"),
            SensitivePromptKind::KeyPassphrase
        );
        assert_eq!(
            classify("Are you sure (yes/no)?"),
            SensitivePromptKind::HostConfirmation
        );
    }
}
