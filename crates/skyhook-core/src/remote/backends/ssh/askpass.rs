use crate::remote::{
    SecretValue, SensitivePrompt, SensitivePromptHandler, SensitivePromptKind, prompt::PromptAnswer,
};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::path::PathBuf;
use std::{
    io::{Read as _, Write as _},
    path::Path,
    sync::Arc,
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{UnixListener, UnixStream},
};
use zeroize::Zeroizing;

pub(crate) struct AskpassServer {
    #[cfg(test)]
    pub socket: PathBuf,
    #[cfg(test)]
    pub executable: PathBuf,
    environment: std::collections::BTreeMap<String, String>,
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
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        // Randomness isolates servers even within one process; the PID is only descriptive.
        // Restrict access at creation, not just with a later chmod, even with umask 000.
        let directory = tempfile::Builder::new()
            .prefix(&format!("skyhook-askpass-{}-", std::process::id()))
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        // Restore owner bits if an unusually restrictive umask removed them.
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        let socket = directory.path().join("askpass.sock");
        let executable = directory.path().join("askpass");
        let environment = std::collections::BTreeMap::from([
            (
                "SSH_ASKPASS".into(),
                super::config::wire_path(&executable)?.to_owned(),
            ),
            ("SSH_ASKPASS_REQUIRE".into(), "force".into()),
            (
                "SKYHOOK_ASKPASS_SOCKET".into(),
                super::config::wire_path(&socket)?.to_owned(),
            ),
            ("DISPLAY".into(), "skyhook".into()),
        ]);
        let helper = std::env::current_exe()?;
        let mut helper_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&executable)?;
        helper_file.write_all(
            format!(
                "#!/bin/sh\nexec {} --askpass \"$@\"\n",
                super::config::shell_quote(super::config::wire_path(&helper)?)
            )
            .as_bytes(),
        )?;
        helper_file.set_permissions(std::fs::Permissions::from_mode(0o700))?;
        let listener = UnixListener::bind(&socket)?;
        // The private directory protects the socket between bind and chmod.
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
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
            #[cfg(test)]
            socket,
            #[cfg(test)]
            executable,
            environment,
            _directory: directory,
            task,
        })
    }

    pub fn environment(&self) -> std::collections::BTreeMap<String, String> {
        self.environment.clone()
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
    // Authenticate before reading/parsing untrusted input or invoking the handler.
    let peer = stream.peer_cred()?;
    // SAFETY: geteuid has no preconditions and does not mutate process state.
    authorize_peer_uid(peer.uid(), unsafe { libc::geteuid() })?;
    let peer_pid = peer.pid();
    let mut bytes = Vec::new();
    (&mut stream)
        .take(64 * 1024)
        .read_to_end(&mut bytes)
        .await?;
    let request: AskpassRequest = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
    let kind = classify(&request.prompt, request.hint.as_deref());
    let gone = async {
        let Some(pid) = peer_pid else {
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

fn authorize_peer_uid(peer_uid: u32, effective_uid: u32) -> Result<(), std::io::Error> {
    if peer_uid != effective_uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "askpass peer UID does not match the server's effective UID",
        ));
    }
    Ok(())
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FixedAnswer {
        value: &'static str,
        calls: AtomicUsize,
    }

    impl SensitivePromptHandler for FixedAnswer {
        fn prompt(&self, prompt: SensitivePrompt) -> crate::remote::SensitivePromptFuture {
            assert_eq!(prompt.kind, SensitivePromptKind::Password);
            self.calls.fetch_add(1, Ordering::SeqCst);
            let value = SecretValue::new(self.value.into());
            Box::pin(async move { Ok(value) })
        }
    }

    fn fixed(value: &'static str) -> Arc<FixedAnswer> {
        Arc::new(FixedAnswer {
            value,
            calls: AtomicUsize::new(0),
        })
    }

    async fn ask(mut client: UnixStream) -> PromptAnswer {
        let request = br#"{"prompt":"Password:","hint":null}"#;
        client.write_all(request).await.unwrap();
        client.shutdown().await.unwrap();
        let mut bytes = Zeroizing::new(Vec::new());
        client.read_to_end(&mut bytes).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn request_password(socket: &Path) -> SecretValue {
        let request = async { ask(UnixStream::connect(socket).await.unwrap()).await };
        match tokio::time::timeout(std::time::Duration::from_secs(5), request)
            .await
            .expect("askpass request timed out")
        {
            PromptAnswer::Accepted(value) => value,
            PromptAnswer::Rejected => panic!("password request unexpectedly rejected"),
        }
    }

    #[test]
    fn private_resources_ignore_umask() {
        const CHILD: &str = "SKYHOOK_ASKPASS_PERMISSIONS_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Only the isolated child changes umask; other test threads are unaffected.
            let name = format!(
                "{}::private_resources_ignore_umask",
                module_path!().split_once("::").unwrap().1
            );
            for mask in ["000", "777"] {
                let output = std::process::Command::new("/bin/sh")
                    .arg("-c")
                    .arg(format!("umask {mask}; exec \"$@\""))
                    .arg("askpass-permissions-test")
                    .arg(std::env::current_exe().unwrap())
                    .args(["--exact", &name, "--nocapture"])
                    .env(CHILD, "1")
                    .env("SKYHOOK_ASKPASS_SOCKET", "/unused-inherited-askpass.sock")
                    .output()
                    .unwrap();
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(output.status.success(), "umask {mask}: {stdout}\n{stderr}");
                assert!(stdout.contains("1 passed"));
            }
            return;
        }

        use std::os::unix::fs::PermissionsExt as _;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let server = AskpassServer::start(fixed("private-answer")).unwrap();
            for (path, mode) in [
                (server._directory.path(), 0o700),
                (server.executable.as_path(), 0o700),
                (server.socket.as_path(), 0o600),
            ] {
                let actual = std::fs::metadata(path).unwrap().permissions().mode() & 0o7777;
                assert_eq!(actual, mode, "incorrect permissions on {}", path.display());
            }
            let socket = &server.environment()["SKYHOOK_ASKPASS_SOCKET"];
            assert_eq!(*socket, server.socket.to_string_lossy());
            assert_ne!(*socket, std::env::var("SKYHOOK_ASKPASS_SOCKET").unwrap());
            let answer = request_password(&server.socket).await;
            assert_eq!(answer.expose(), "private-answer");
        });
    }

    #[tokio::test]
    async fn concurrent_servers_have_independent_handlers_and_cleanup() {
        let (first_handler, second_handler) = (fixed("first-answer"), fixed("second-answer"));
        let first = AskpassServer::start(first_handler.clone()).unwrap();
        let second = AskpassServer::start(second_handler.clone()).unwrap();
        assert_ne!(first._directory.path(), second._directory.path());
        assert_ne!(first.socket, second.socket);
        assert_ne!(first.executable, second.executable);
        for key in ["SSH_ASKPASS", "SKYHOOK_ASKPASS_SOCKET"] {
            assert_ne!(first.environment()[key], second.environment()[key]);
        }
        let (first_answer, second_answer) = tokio::join!(
            request_password(&first.socket),
            request_password(&second.socket)
        );
        assert_eq!(first_answer.expose(), "first-answer");
        assert_eq!(second_answer.expose(), "second-answer");
        let calls = |handler: &FixedAnswer| handler.calls.load(Ordering::SeqCst);
        assert_eq!((calls(&first_handler), calls(&second_handler)), (1, 1));

        let first_paths = [
            first._directory.path().to_path_buf(),
            first.socket.clone(),
            first.executable.clone(),
        ];
        drop(first);
        assert!(first_paths.iter().all(|path| !path.exists()));
        assert!(second.socket.exists() && second.executable.exists());
        let answer = request_password(&second.socket).await;
        assert_eq!(answer.expose(), "second-answer");
        assert_eq!(calls(&second_handler), 2);
        let second_directory = second._directory.path().to_path_buf();
        drop(second);
        assert!(!second_directory.exists());
    }

    #[test]
    fn peer_uid_must_match_effective_uid_even_for_root() {
        assert!(authorize_peer_uid(1000, 1000).is_ok());
        assert!(authorize_peer_uid(0, 0).is_ok());
        for (peer, effective) in [(1001, 1000), (0, 1000), (1000, 0)] {
            let error = authorize_peer_uid(peer, effective).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        }
    }

    #[test]
    fn classifies_openssh_prompts_and_confirmation_hints() {
        for (prompt, hint, kind) in [
            ("Password:", None, SensitivePromptKind::Password),
            (
                "Enter passphrase for key",
                None,
                SensitivePromptKind::KeyPassphrase,
            ),
            (
                "Are you sure (yes/no)?",
                None,
                SensitivePromptKind::HostConfirmation,
            ),
            (
                "Allow use of key?",
                Some("confirm"),
                SensitivePromptKind::AgentConfirmation,
            ),
        ] {
            assert_eq!(classify(prompt, hint), kind);
        }
    }

    #[tokio::test]
    async fn rejected_prompts_are_distinct_from_empty_secrets() {
        let (client, server) = UnixStream::pair().unwrap();
        let rejecting = Arc::new(crate::remote::RejectSensitivePrompts);
        let task = tokio::spawn(serve_one(server, rejecting));
        assert!(matches!(ask(client).await, PromptAnswer::Rejected));
        task.await.unwrap().unwrap();
        let empty = serde_json::from_str::<PromptAnswer>(r#"{"Accepted":""}"#).unwrap();
        assert!(matches!(empty, PromptAnswer::Accepted(_)));
    }

    #[test]
    fn confirmation_denial_is_failure_and_empty_password_is_success() {
        let accepted = |value: &str| PromptAnswer::Accepted(SecretValue::new(value.into()));
        assert!(answer_value(accepted("no"), true).is_err());
        assert!(answer_value(accepted("yes"), true).unwrap().is_none());
        let empty = answer_value(accepted(""), false).unwrap().unwrap();
        assert_eq!(empty.expose(), "");
        assert!(!format!("{:?}", accepted("never-print-this")).contains("never-print-this"));
    }
}
