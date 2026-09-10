//! Lazy native-provider credentials. Only successful, validated headers are cached.

use super::Protocol;
use crate::provider::{ProviderError, ProviderErrorKind};
use reqwest::header::HeaderValue;
use std::{process::Stdio, sync::Arc};
use tokio::{io::AsyncReadExt, process::Command, sync::OnceCell};

// Credentials should be small; cap even unsuccessful or never-ending output.
const MAX_STDOUT_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub(super) struct ApiKeyCommand {
    command: String,
    header: Arc<OnceCell<HeaderValue>>,
}

impl ApiKeyCommand {
    pub(super) fn new(command: String) -> Self {
        Self {
            command,
            header: Arc::new(OnceCell::new()),
        }
    }

    pub(super) async fn header(&self, protocol: Protocol) -> Result<HeaderValue, ProviderError> {
        self.header
            .get_or_try_init(|| execute(&self.command, protocol))
            .await
            .cloned()
    }
}

fn failure(message: &'static str) -> ProviderError {
    // Never retain the command, output, exit status, or underlying OS error.
    ProviderError {
        kind: ProviderErrorKind::Authentication,
        message: message.into(),
    }
}

async fn execute(command: &str, protocol: Protocol) -> Result<HeaderValue, ProviderError> {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| failure("API key command could not be started"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| failure("API key command output could not be read"))?;
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    stdout
        .take((MAX_STDOUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| failure("API key command output could not be read"))?;
    if bytes.len() > MAX_STDOUT_BYTES {
        return Err(failure("API key command output exceeded the size limit"));
    }
    let status = child
        .wait()
        .await
        .map_err(|_| failure("API key command could not be awaited"))?;
    if !status.success() {
        return Err(failure("API key command exited unsuccessfully"));
    }
    let key = std::str::from_utf8(&bytes)
        .map_err(|_| failure("API key command output was not valid UTF-8"))?
        .trim();
    if key.is_empty() {
        return Err(failure("API key command output was empty"));
    }
    let mut header = match protocol {
        Protocol::Chat | Protocol::Responses => {
            let bearer = zeroize::Zeroizing::new(format!("Bearer {key}"));
            HeaderValue::from_str(&bearer)
        }
        Protocol::Anthropic => HeaderValue::from_str(key),
    }
    .map_err(|_| failure("API key command output was not a valid credential header"))?;
    header.set_sensitive(true);
    Ok(header)
}

#[cfg(test)]
mod tests;
