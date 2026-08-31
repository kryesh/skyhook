mod interaction;

use std::{collections::HashSet, path::PathBuf, sync::Arc};

use clap::Parser;
use serde_json::Value;
use skyhook::{
    agent::RuntimeEvent,
    config::Config,
    identity::{AgentId, SessionId},
    session::SessionEvent,
    tool::policy::AllowAll,
};
use tokio::io::AsyncBufReadExt as _;

use interaction::{CliInteraction, CliPolicy, CliQuestions, CliSensitivePrompts};

#[derive(Parser)]
#[command(version, about = "Programmable coding-agent harness")]
struct Args {
    /// Explicit TOML config used instead of the user config.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Workspace visible to coding tools.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Resume an existing session id.
    #[arg(long)]
    resume: Option<SessionId>,
    /// Override the configured root model profile.
    #[arg(short = 'm', long = "model")]
    model: Option<String>,
    /// Override the configured root instruction profile.
    #[arg(long)]
    agent_profile: Option<String>,
    /// Approve every tool invocation without prompting.
    #[arg(long)]
    approve_all: bool,
    /// Attach images to the first prompt.
    #[arg(long = "image", requires = "prompt")]
    images: Vec<PathBuf>,
    /// Run one prompt and exit. Without it, Skyhook reads prompts interactively.
    #[arg(short, long, conflicts_with = "script")]
    prompt: Option<String>,
    /// Run a JavaScript workflow file through the registered script tool and exit.
    #[arg(short, long, value_name = "PATH", conflicts_with = "prompt")]
    script: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    if let Some(socket) = std::env::var_os("SKYHOOK_ASKPASS_SOCKET") {
        let prompt = std::env::args().nth(1).unwrap_or_default();
        if let Err(error) =
            skyhook::remote::run_askpass_helper(std::path::Path::new(&socket), prompt)
        {
            eprintln!("skyhook askpass: {error}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(error) = run().await {
        eprintln!("skyhook: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let mut config = Config::load(args.config.as_deref()).await?;
    if let Some(profile) = args.model {
        config.default_model_profile = profile;
    }
    if args.agent_profile.is_some() {
        config.default_agent_profile = args.agent_profile;
    }
    let interaction = Arc::new(CliInteraction::default());
    // `Config::build_harness` is convenient for embedders; the CLI adds its interactive hooks.
    let approve_all = args.approve_all || config.approve_all;
    let builder = config.harness_builder(args.workspace)?;
    let builder = if approve_all {
        builder.policy(Arc::new(AllowAll))
    } else {
        builder.policy(Arc::new(CliPolicy::new(interaction.clone())))
    };
    let harness = builder
        .question_handler(Arc::new(CliQuestions::new(interaction.clone())))
        .sensitive_prompt_handler(Arc::new(CliSensitivePrompts::new(interaction)))
        .build()
        .await?;
    let session = match args.resume {
        Some(id) => harness.resume_session(id).await?,
        None => harness.new_session().await?,
    };
    eprintln!("session {}", session.id());
    let mut events = session.subscribe();
    tokio::spawn(async move {
        let mut output = AgentOutput::default();
        loop {
            match events.recv().await {
                Ok(RuntimeEvent::TextDelta { agent, text }) => {
                    output.text(&agent, &text);
                }
                Ok(RuntimeEvent::TurnCompleted { agent, text }) => output.finish(&agent, &text),
                Ok(RuntimeEvent::Record(record)) => {
                    if let SessionEvent::JobCreated {
                        job,
                        tool,
                        arguments,
                        background,
                        ..
                    } = &record.event
                    {
                        output.tool(
                            &record.agent,
                            &format_tool_call(*job, tool, arguments, *background),
                        );
                    }
                }
                Ok(RuntimeEvent::ReasoningDelta { .. }) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    output.runtime("event stream lagged; some output was omitted");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    output.finish_any();
                    break;
                }
            }
        }
    });

    if let Some(script) = args.script {
        let source = tokio::fs::read_to_string(script).await?;
        let output = session.run_script(source).await?;
        println!("{}", serde_json::to_string_pretty(&output.value)?);
        return Ok(());
    }

    if let Some(prompt) = args.prompt {
        if args.images.is_empty() {
            let _ = session.prompt(prompt).await?;
        } else {
            let _ = session.prompt_with_images(prompt, &args.images).await?;
        }
        return Ok(());
    }

    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    eprintln!("Enter a prompt; Ctrl-D exits.");
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let _ = session.prompt(line).await?;
    }
    session.shutdown().await?;
    Ok(())
}

#[derive(Default)]
struct AgentOutput {
    active: Option<AgentId>,
    at_line_start: bool,
    turns_with_text: HashSet<AgentId>,
}

impl AgentOutput {
    fn text(&mut self, agent: &AgentId, text: &str) {
        use std::io::Write as _;

        if self.active.as_ref() != Some(agent) {
            self.finish_any();
            print!("[{agent}] ");
            self.active = Some(agent.clone());
            self.at_line_start = false;
        }
        print!("{text}");
        self.turns_with_text.insert(agent.clone());
        self.at_line_start = text.ends_with('\n');
        let _ = std::io::stdout().flush();
    }

    fn finish(&mut self, agent: &AgentId, text: &str) {
        if !text.is_empty() && !self.turns_with_text.contains(agent) {
            self.text(agent, text);
        }
        if self.active.as_ref() == Some(agent) {
            self.finish_any();
        }
        self.turns_with_text.remove(agent);
    }

    fn finish_any(&mut self) {
        if self.active.take().is_some() && !self.at_line_start {
            println!();
        }
        self.at_line_start = true;
    }

    fn tool(&mut self, agent: &AgentId, summary: &str) {
        self.finish_any();
        println!("[{agent}] {summary}");
    }

    fn runtime(&mut self, message: &str) {
        self.finish_any();
        eprintln!("[skyhook] {message}");
    }
}

fn format_tool_call(
    job: skyhook::identity::JobId,
    tool: &str,
    arguments: &Value,
    bg: bool,
) -> String {
    let detail = match tool {
        "read" | "remove" => one_arg(arguments, "path"),
        "search" | "glob" => format!(
            "{} in {}",
            quoted_arg(arguments, "pattern"),
            arg(arguments, "path").unwrap_or(".")
        ),
        "exec" => {
            let command = arguments
                .get("argv")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            with_target(&brief(&command, 100), arguments)
        }
        "shell" => with_target(
            &brief(arg(arguments, "command").unwrap_or(""), 100),
            arguments,
        ),
        "write" => format!(
            "{} ({} chars)",
            arg(arguments, "path").unwrap_or("<path>"),
            string_len(arguments, "content")
        ),
        "replace" => format!("{} (exact text replacement)", one_arg(arguments, "path")),
        "patch" => format!("{} (unified diff)", one_arg(arguments, "path")),
        "script" => format!(
            "JavaScript workflow ({} chars)",
            string_len(arguments, "source")
        ),
        "agent" => with_target(
            &format!("task {}", quoted_brief_arg(arguments, "prompt", 100)),
            arguments,
        ),
        "ask" => format!("{} question(s)", array_len(arguments, "questions")),
        "todo" => format!("{} item(s)", array_len(arguments, "items")),
        "target_add" => format!(
            "{} ({})",
            arg(arguments, "name").unwrap_or("<target>"),
            arg(arguments, "host").unwrap_or("<host>")
        ),
        "skill" => match arg(arguments, "path") {
            Some(path) => format!(
                "{} asset {path}",
                arg(arguments, "name").unwrap_or("<skill>")
            ),
            None => one_arg(arguments, "name"),
        },
        name if name.starts_with("job_") => arguments
            .get("job")
            .map_or_else(String::new, |value| format!("job {value}")),
        _ => safe_scalar_args(arguments),
    };
    let background = if bg { " [background]" } else { "" };
    if detail.is_empty() {
        format!("tool #{job}: {tool}{background}")
    } else {
        format!("tool #{job}: {tool} {detail}{background}")
    }
}

fn arg<'a>(arguments: &'a Value, name: &str) -> Option<&'a str> {
    arguments.get(name).and_then(Value::as_str)
}

fn one_arg(arguments: &Value, name: &str) -> String {
    arg(arguments, name).unwrap_or("<unspecified>").to_owned()
}

fn quoted_arg(arguments: &Value, name: &str) -> String {
    quoted_brief_arg(arguments, name, 80)
}

fn quoted_brief_arg(arguments: &Value, name: &str, limit: usize) -> String {
    format!("{:?}", brief(arg(arguments, name).unwrap_or(""), limit))
}

fn with_target(detail: &str, arguments: &Value) -> String {
    arg(arguments, "target").map_or_else(
        || detail.to_owned(),
        |target| format!("{detail} on {target}"),
    )
}

fn string_len(arguments: &Value, name: &str) -> usize {
    arg(arguments, name).map_or(0, |value| value.chars().count())
}

fn array_len(arguments: &Value, name: &str) -> usize {
    arguments
        .get(name)
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

fn brief(value: &str, limit: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut characters = normalized.chars();
    let visible = characters.by_ref().take(limit).collect::<String>();
    if characters.next().is_some() {
        format!("{visible}…")
    } else {
        visible
    }
}

fn safe_scalar_args(arguments: &Value) -> String {
    const REDACTED: &[&str] = &["content", "new", "old", "patch", "source", "value"];
    let Some(arguments) = arguments.as_object() else {
        return String::new();
    };
    arguments
        .iter()
        .filter(|(key, value)| {
            !REDACTED.contains(&key.as_str())
                && (value.is_null() || value.is_boolean() || value.is_number() || value.is_string())
        })
        .take(3)
        .map(|(key, value)| format!("{key}={}", brief(&value.to_string(), 40)))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;
    use serde_json::json;
    use skyhook::identity::JobId;

    use super::{Args, brief, format_tool_call};

    #[test]
    fn prompt_and_script_are_explicit_and_exclusive() {
        let prompt = Args::try_parse_from(["skyhook", "-p", "hello"]).unwrap();
        assert_eq!(prompt.prompt.as_deref(), Some("hello"));
        assert!(prompt.script.is_none());

        let script = Args::try_parse_from(["skyhook", "--script", "workflow.js"]).unwrap();
        assert_eq!(
            script.script.as_deref(),
            Some(std::path::Path::new("workflow.js"))
        );
        assert!(script.prompt.is_none());

        assert!(Args::try_parse_from(["skyhook", "bare prompt"]).is_err());
        assert!(Args::try_parse_from(["skyhook", "-p", "hello", "-s", "flow.js"]).is_err());

        let model = Args::try_parse_from([
            "skyhook",
            "--config",
            "skyhook.toml",
            "-m",
            "qwen36-35",
            "-p",
            "test a remote agent",
            "--approve-all",
        ])
        .unwrap();
        assert_eq!(model.model.as_deref(), Some("qwen36-35"));
        assert!(model.approve_all);
        assert!(Args::try_parse_from(["skyhook", "--model-profile", "old"]).is_err());
    }

    #[test]
    fn tool_summaries_are_brief_and_hide_payloads() {
        let script = format_tool_call(
            JobId::new(7).unwrap(),
            "script",
            &json!({"source":"const secret = 'do not print'; return 42", "bg":true}),
            true,
        );
        assert_eq!(
            script,
            "tool #7: script JavaScript workflow (40 chars) [background]"
        );
        assert!(!script.contains("secret"));

        let agent = format_tool_call(
            JobId::new(8).unwrap(),
            "agent",
            &json!({"prompt":"report the kernel version", "target":"lab-monitoring"}),
            false,
        );
        assert_eq!(
            agent,
            "tool #8: agent task \"report the kernel version\" on lab-monitoring"
        );
        assert_eq!(brief("one\n two   three", 7), "one two…");
    }
}
