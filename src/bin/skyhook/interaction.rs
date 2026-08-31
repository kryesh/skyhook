use std::{future::Future, pin::Pin, sync::Arc};

use serde_json::Value;
use skyhook::{
    agent::{Question, QuestionError, QuestionHandler},
    identity::AgentId,
    remote::{
        SecretValue, SensitivePrompt, SensitivePromptFuture, SensitivePromptHandler,
        SensitivePromptKind,
    },
    tool::policy::{AuthorizationRequest, Policy, PolicyDecision, PolicyFuture, ToolEffect},
};
use tokio::sync::Mutex;

#[derive(Default)]
pub(super) struct CliInteraction {
    terminal: Mutex<()>,
}

impl CliInteraction {
    async fn line(&self, prompt: String) -> Result<String, String> {
        let _guard = self.terminal.lock().await;
        tokio::task::spawn_blocking(move || {
            use std::io::Write as _;
            eprint!("{prompt}");
            std::io::stderr()
                .flush()
                .map_err(|error| error.to_string())?;
            let mut line = String::new();
            std::io::stdin()
                .read_line(&mut line)
                .map_err(|error| error.to_string())?;
            Ok(line.trim().to_owned())
        })
        .await
        .map_err(|error| error.to_string())?
    }

    async fn secret(&self, prompt: String) -> Result<String, String> {
        let _guard = self.terminal.lock().await;
        tokio::task::spawn_blocking(move || {
            use std::io::Write as _;
            eprint!("{prompt}");
            std::io::stderr()
                .flush()
                .map_err(|error| error.to_string())?;
            let disabled = std::process::Command::new("stty")
                .arg("-echo")
                .status()
                .map_err(|error| error.to_string())?;
            if !disabled.success() {
                return Err("could not disable terminal echo".to_owned());
            }
            let mut line = String::new();
            let read = std::io::stdin()
                .read_line(&mut line)
                .map_err(|error| error.to_string());
            let restored = std::process::Command::new("stty").arg("echo").status();
            eprintln!();
            if !restored.is_ok_and(|status| status.success()) {
                return Err("could not restore terminal echo".to_owned());
            }
            read?;
            Ok(line.trim_end_matches(['\r', '\n']).to_owned())
        })
        .await
        .map_err(|error| error.to_string())?
    }
}

pub(super) struct CliPolicy {
    interaction: Arc<CliInteraction>,
}

impl CliPolicy {
    pub(super) const fn new(interaction: Arc<CliInteraction>) -> Self {
        Self { interaction }
    }
}

impl Policy for CliPolicy {
    fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_> {
        let interaction = self.interaction.clone();
        Box::pin(async move {
            let risky = request.effects.iter().any(|effect| {
                matches!(
                    effect,
                    ToolEffect::WriteWorkspace
                        | ToolEffect::ExecuteProcess
                        | ToolEffect::ManageTargets
                        | ToolEffect::RemoteAccess
                )
            });
            if !risky {
                return PolicyDecision::Allow;
            }
            let arguments = serde_json::to_string_pretty(&request.arguments)
                .unwrap_or_else(|_| "{}".to_owned());
            let prompt = format!(
                "\nAllow tool `{}` for agent {}?\n{}\n[y/N] ",
                request.tool, request.agent, arguments
            );
            match interaction.line(prompt).await {
                Ok(answer) if matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes") => {
                    PolicyDecision::Allow
                }
                Ok(_) => PolicyDecision::Deny {
                    reason: "denied by user".to_owned(),
                },
                Err(error) => PolicyDecision::Deny { reason: error },
            }
        })
    }
}

pub(super) struct CliSensitivePrompts {
    interaction: Arc<CliInteraction>,
}

impl CliSensitivePrompts {
    pub(super) const fn new(interaction: Arc<CliInteraction>) -> Self {
        Self { interaction }
    }
}

impl SensitivePromptHandler for CliSensitivePrompts {
    fn prompt(&self, prompt: SensitivePrompt) -> SensitivePromptFuture {
        let interaction = self.interaction.clone();
        Box::pin(async move {
            let value = if prompt.kind == SensitivePromptKind::HostConfirmation {
                interaction.line(format!("\n{} ", prompt.message)).await
            } else {
                interaction.secret(format!("\n{} ", prompt.message)).await
            }
            .map_err(skyhook::remote::SensitivePromptError::Failed)?;
            Ok(SecretValue::new(value))
        })
    }
}

pub(super) struct CliQuestions {
    interaction: Arc<CliInteraction>,
}

impl CliQuestions {
    pub(super) const fn new(interaction: Arc<CliInteraction>) -> Self {
        Self { interaction }
    }
}

impl QuestionHandler for CliQuestions {
    fn ask(
        &self,
        agent: AgentId,
        questions: Vec<Question>,
    ) -> Pin<Box<dyn Future<Output = Result<Value, QuestionError>> + Send + 'static>> {
        let interaction = self.interaction.clone();
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
