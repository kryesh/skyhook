use std::{future::Future, pin::Pin, sync::Arc};

use serde_json::Value;
use skyhook::{
    agent::{Question, QuestionError, QuestionHandler},
    identity::AgentId,
    remote::{
        SecretValue, SensitivePrompt, SensitivePromptFuture, SensitivePromptHandler,
        SensitivePromptKind,
    },
    tool::policy::{
        AuthorizationRequest, Capability, PermissionUse, Policy, PolicyDecision, PolicyFuture,
    },
};
use tokio::sync::Mutex;

#[derive(Clone, Default)]
pub(super) struct CliInteraction {
    terminal: Arc<Mutex<()>>,
    interactive: bool,
}

impl CliInteraction {
    pub fn new(interactive: bool) -> Self {
        use std::io::IsTerminal as _;
        Self {
            terminal: Arc::default(),
            interactive: interactive && std::io::stdin().is_terminal(),
        }
    }

    async fn line(&self, prompt: String) -> Result<String, String> {
        self.input(prompt, false)
            .await
            .map(|line| line.trim().to_owned())
    }

    async fn secret(&self, prompt: String) -> Result<String, String> {
        self.input(prompt, true).await
    }

    async fn input(&self, prompt: String, secret: bool) -> Result<String, String> {
        if !self.interactive {
            return Err("interactive input is unavailable in this invocation".to_owned());
        }
        let guard = self.terminal.clone().lock_owned().await;
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancellation = CancelInput(cancelled.clone());
        let result = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            read_terminal(&prompt, secret, &cancelled).map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| error.to_string())?;
        drop(cancellation);
        result
    }
}

struct CancelInput(Arc<std::sync::atomic::AtomicBool>);
impl Drop for CancelInput {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

fn read_terminal(
    prompt: &str,
    secret: bool,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<String, std::io::Error> {
    use std::{
        io::{Read as _, Write as _},
        os::{fd::AsRawFd as _, unix::fs::OpenOptionsExt as _},
    };
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open("/dev/tty")?;
    let fd = tty.as_raw_fd();
    let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: fd refers to an open terminal and original points to writable termios storage.
    if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: tcgetattr initialized the value above.
    let original = unsafe { original.assume_init() };
    struct Restore(i32, libc::termios);
    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: the terminal file outlives this guard.
            unsafe {
                libc::tcsetattr(self.0, libc::TCSANOW, &self.1);
            }
        }
    }
    let restore = Restore(fd, original);
    if secret {
        let mut hidden = original;
        hidden.c_lflag &= !libc::ECHO;
        // SAFETY: fd and hidden are valid for this terminal.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &hidden) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    tty.write_all(prompt.as_bytes())?;
    let mut bytes = Vec::new();
    loop {
        if cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "interactive input was cancelled",
            ));
        }
        let mut event = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: event is a valid pollfd for the open terminal.
        let ready = unsafe { libc::poll(&mut event, 1, 100) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            continue;
        }
        let mut byte = [0];
        match tty.read(&mut byte) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "terminal input closed",
                ));
            }
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => bytes.push(byte[0]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
    drop(restore);
    if secret {
        tty.write_all(b"\n")?;
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    String::from_utf8(bytes).map_err(std::io::Error::other)
}

fn requires_prompt(permission: &PermissionUse) -> bool {
    match permission.capability {
        Capability::Read | Capability::Agents => false,
        Capability::Exec | Capability::Targets => true,
        Capability::Write => {
            permission.resource.namespace != "workspace"
                || permission
                    .resource
                    .segments
                    .first()
                    .is_none_or(|target| target != "root")
        }
    }
}

impl Policy for CliInteraction {
    fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_> {
        let interaction = self.clone();
        Box::pin(async move {
            if !request.permissions.iter().any(requires_prompt) {
                return PolicyDecision::allow();
            }
            let arguments = serde_json::to_string_pretty(&request.arguments)
                .unwrap_or_else(|_| "{}".to_owned());
            let permissions = request
                .permissions
                .iter()
                .map(|permission| {
                    let resource = permission.resource.segments.join("/");
                    format!(
                        "\n- {:?}: {}/{}",
                        permission.capability, permission.resource.namespace, resource
                    )
                })
                .collect::<String>();
            let prompt = format!(
                "\nAllow tool `{}` for agent {}?{}\n{}\n[y/N] ",
                request.tool, request.agent, permissions, arguments
            );
            match interaction.line(prompt).await {
                Ok(answer) if matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes") => {
                    PolicyDecision::Allow {
                        grants: request
                            .permissions
                            .into_iter()
                            .filter_map(|permission| permission.proposed_grant)
                            .collect(),
                    }
                }
                Ok(_) => PolicyDecision::Deny {
                    reason: "denied by user".to_owned(),
                },
                Err(error) => PolicyDecision::Deny { reason: error },
            }
        })
    }
}

impl SensitivePromptHandler for CliInteraction {
    fn prompt(&self, prompt: SensitivePrompt) -> SensitivePromptFuture {
        let interaction = self.clone();
        Box::pin(async move {
            let value = if matches!(
                prompt.kind,
                SensitivePromptKind::HostConfirmation | SensitivePromptKind::AgentConfirmation
            ) {
                interaction.line(format!("\n{} ", prompt.message)).await
            } else {
                interaction.secret(format!("\n{} ", prompt.message)).await
            }
            .map_err(skyhook::remote::SensitivePromptError::Failed)?;
            Ok(SecretValue::new(value))
        })
    }
}

impl QuestionHandler for CliInteraction {
    fn ask(
        &self,
        agent: AgentId,
        questions: Vec<Question>,
    ) -> Pin<Box<dyn Future<Output = Result<Value, QuestionError>> + Send + 'static>> {
        let interaction = self.clone();
        Box::pin(async move {
            let body = serde_json::to_string_pretty(&questions)
                .map_err(|error| QuestionError::Failed(error.to_string()))?;
            let line = interaction
                .line(format!(
                    "\nAgent {agent} asks:\n{body}\nanswer (JSON or text)> "
                ))
                .await
                .map_err(QuestionError::Failed)?;
            Ok(serde_json::from_str(&line).unwrap_or(Value::String(line)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skyhook::tool::policy::ResourceId;

    #[tokio::test]
    async fn noninteractive_invocations_reject_every_authentication_prompt() {
        let interaction = CliInteraction::new(false);
        for kind in [
            SensitivePromptKind::Password,
            SensitivePromptKind::KeyPassphrase,
            SensitivePromptKind::KeyboardInteractive,
            SensitivePromptKind::HostConfirmation,
            SensitivePromptKind::AgentConfirmation,
        ] {
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(100),
                interaction.prompt(SensitivePrompt {
                    kind,
                    message: "must not read stdin".into(),
                }),
            )
            .await
            .unwrap();
            assert!(result.is_err());
        }
    }

    #[test]
    fn approval_policy_is_resource_driven() {
        assert!(!requires_prompt(&PermissionUse::new(
            Capability::Write,
            ResourceId::workspace("root", std::path::Path::new("/workspace")),
        )));
        assert!(requires_prompt(&PermissionUse::new(
            Capability::Write,
            ResourceId::path("root", std::path::Path::new("/outside")),
        )));
        assert!(requires_prompt(&PermissionUse::new(
            Capability::Exec,
            ResourceId::workspace("root", std::path::Path::new("/workspace")),
        )));
        assert!(requires_prompt(&PermissionUse::new(
            Capability::Targets,
            ResourceId::new("route", ["build"]),
        )));
    }
}
