use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};

use serde_json::Value;
use skyhook::{
    agent::{Question, QuestionError, QuestionHandler},
    identity::{AgentId, SessionId},
    remote::{
        SecretValue, SensitivePrompt, SensitivePromptFuture, SensitivePromptHandler,
        SensitivePromptKind,
    },
    tool::policy::{
        AuthorizationRequest, PathAccess, Policy, PolicyDecision, PolicyFuture, ToolEffect,
    },
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
    grants: Mutex<Vec<PathGrant>>,
}

#[derive(Clone)]
struct PathGrant {
    session: SessionId,
    target: String,
    access: PathAccess,
    path: PathBuf,
    subtree: bool,
}

impl PathGrant {
    fn allows(
        &self,
        session: SessionId,
        target: &str,
        access: PathAccess,
        path: &std::path::Path,
    ) -> bool {
        self.session == session
            && self.target == target
            && self.access == access
            && if self.subtree {
                path.starts_with(&self.path)
            } else {
                path == self.path
            }
    }
}

fn requires_prompt(effects: &[ToolEffect], has_missing_paths: bool) -> bool {
    has_missing_paths
        || effects.iter().any(|effect| {
            matches!(
                effect,
                ToolEffect::WriteWorkspace
                    | ToolEffect::ExecuteProcess
                    | ToolEffect::ManageTargets
                    | ToolEffect::RemoteAccess
            )
        })
}

impl CliPolicy {
    pub(super) const fn new(interaction: Arc<CliInteraction>) -> Self {
        Self {
            interaction,
            grants: Mutex::const_new(Vec::new()),
        }
    }
}

impl Policy for CliPolicy {
    fn authorize(&self, request: AuthorizationRequest) -> PolicyFuture<'_> {
        let interaction = self.interaction.clone();
        let grants = &self.grants;
        Box::pin(async move {
            let session = request.agent.session();
            let cached = grants.lock().await;
            let missing_paths = request
                .effects
                .iter()
                .filter_map(|effect| match effect {
                    ToolEffect::ExternalPath {
                        path,
                        access,
                        directory,
                    } => Some((path.clone(), *access, *directory)),
                    _ => None,
                })
                .filter(|(path, access, _)| {
                    !cached
                        .iter()
                        .any(|grant| grant.allows(session, &request.target, *access, path))
                })
                .collect::<Vec<_>>();
            drop(cached);
            let risky = requires_prompt(&request.effects, !missing_paths.is_empty());
            if !risky {
                return PolicyDecision::Allow;
            }
            let arguments = serde_json::to_string_pretty(&request.arguments)
                .unwrap_or_else(|_| "{}".to_owned());
            let paths = missing_paths
                .iter()
                .map(|(path, access, directory)| {
                    format!(
                        "\n- {:?} {}{} on {}",
                        access,
                        path.display(),
                        if *directory { " and descendants" } else { "" },
                        request.target
                    )
                })
                .collect::<String>();
            let prompt = format!(
                "\nAllow tool `{}` for agent {} on {}?{}\n{}\n[y/N] ",
                request.tool, request.agent, request.target, paths, arguments
            );
            match interaction.line(prompt).await {
                Ok(answer) if matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes") => {
                    if !missing_paths.is_empty() {
                        let mut cached = grants.lock().await;
                        cached.extend(missing_paths.into_iter().map(
                            |(path, access, directory)| PathGrant {
                                session,
                                target: request.target.clone(),
                                access,
                                path,
                                subtree: directory,
                            },
                        ));
                    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_grants_are_scoped_by_session_target_access_and_subtree() {
        let session = SessionId::from_bytes([1; 16]);
        let grant = PathGrant {
            session,
            target: "build".to_owned(),
            access: PathAccess::Read,
            path: PathBuf::from("/srv/shared"),
            subtree: true,
        };
        assert!(grant.allows(
            session,
            "build",
            PathAccess::Read,
            std::path::Path::new("/srv/shared/src/lib.rs")
        ));
        assert!(!grant.allows(
            session,
            "build",
            PathAccess::Write,
            std::path::Path::new("/srv/shared/src/lib.rs")
        ));
        assert!(!grant.allows(
            session,
            "other",
            PathAccess::Read,
            std::path::Path::new("/srv/shared/src/lib.rs")
        ));
        assert!(!grant.allows(
            SessionId::from_bytes([2; 16]),
            "build",
            PathAccess::Read,
            std::path::Path::new("/srv/shared/src/lib.rs")
        ));

        let file = PathGrant {
            subtree: false,
            ..grant
        };
        assert!(file.allows(
            session,
            "build",
            PathAccess::Read,
            std::path::Path::new("/srv/shared")
        ));
        assert!(!file.allows(
            session,
            "build",
            PathAccess::Read,
            std::path::Path::new("/srv/shared/child")
        ));

        assert!(!requires_prompt(
            &[ToolEffect::ExternalPath {
                path: PathBuf::from("/srv/shared"),
                access: PathAccess::Write,
                directory: true,
            }],
            false,
        ));
        assert!(requires_prompt(&[ToolEffect::ExecuteProcess], false));
    }
}
