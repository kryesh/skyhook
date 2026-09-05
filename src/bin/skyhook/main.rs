mod interaction;

mod embedded_shims {
    include!(concat!(env!("OUT_DIR"), "/embedded_shims.rs"));
}

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

use interaction::CliInteraction;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(name = "skyhook", version, about = "Programmable coding-agent harness")]
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
    let shim_catalog =
        skyhook::remote::EmbeddedShimCatalog::from_assets(embedded_shims::EMBEDDED_SHIMS)?;
    if let Some(profile) = args.model {
        config.default_model_profile = profile;
    }
    if args.agent_profile.is_some() {
        config.default_agent_profile = args.agent_profile;
    }
    let interaction = Arc::new(CliInteraction::default());
    let approve_all = args.approve_all || config.approve_all;
    let builder = config
        .harness_builder(args.workspace)?
        .shim_catalog(shim_catalog);
    let builder = if approve_all {
        builder.policy(Arc::new(AllowAll))
    } else {
        builder.policy(interaction.clone())
    };
    let harness = builder
        .question_handler(interaction.clone())
        .sensitive_prompt_handler(interaction)
        .build()
        .await?;
    let session = match args.resume {
        Some(id) => harness.resume_session(id).await?,
        None => harness.new_session().await?,
    };
    eprintln!("session {}", session.id());
    let mut events = session.subscribe();
    let (output_shutdown, mut output_shutdown_rx) = tokio::sync::oneshot::channel();
    let output_task = tokio::spawn(async move {
        let mut output = AgentOutput::default();
        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Ok(event) => output.event(event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        output.runtime("event stream lagged; some output was omitted");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                _ = &mut output_shutdown_rx => {
                    while let Ok(event) = events.try_recv() {
                        output.event(event);
                    }
                    break;
                }
            }
        }
        output.finish_any();
    });

    if let Some(script) = args.script {
        let source = tokio::fs::read_to_string(script).await?;
        let output = session.run_script(source).await?;
        println!("{}", serde_json::to_string_pretty(&output.value)?);
        if !output.console_output.is_empty() {
            eprintln!("Console output:\n{}", output.console_output);
        }
    } else if let Some(prompt) = args.prompt {
        if args.images.is_empty() {
            let _ = session.prompt(prompt).await?;
        } else {
            let _ = session.prompt_with_images(prompt, &args.images).await?;
        }
    } else {
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        eprintln!("Enter a prompt; Ctrl-D exits.");
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let _ = session.prompt(line).await?;
        }
    }
    session.shutdown().await?;
    let usage = session.usage().await;
    let _ = output_shutdown.send(());
    output_task.await?;
    eprintln!(
        "{}",
        token_summary(
            usage.output_tokens,
            usage.input_tokens,
            usage.cached_input_tokens
        )
    );
    Ok(())
}

fn token_summary(output_tokens: u64, input_tokens: u64, cached_input_tokens: u64) -> String {
    let total_input_tokens = input_tokens.saturating_add(cached_input_tokens);
    format!(
        "tokens: {output_tokens} output, {total_input_tokens} total input, {input_tokens} uncached input"
    )
}

#[derive(Default)]
struct AgentOutput {
    active: Option<AgentId>,
    at_line_start: bool,
    turns_with_text: HashSet<AgentId>,
}

impl AgentOutput {
    fn event(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::TextDelta { agent, text } => self.text(&agent, &text),
            RuntimeEvent::TurnCompleted { agent, text } => self.finish(&agent, &text),
            RuntimeEvent::Record(record) => {
                if let SessionEvent::JobCreated {
                    job,
                    tool,
                    arguments,
                    background,
                    location,
                    ..
                } = &record.event
                {
                    self.tool(
                        &record.agent,
                        &format_tool_call(*job, tool, arguments, *background, location),
                    );
                }
            }
            RuntimeEvent::ReasoningDelta { .. } => {}
        }
    }

    fn text(&mut self, agent: &AgentId, text: &str) {
        use std::io::Write as _;

        if self.active.as_ref() != Some(agent) {
            self.finish_any();
            print!("[{}] ", agent_label(agent));
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
        println!("[{}] {summary}", agent_label(agent));
    }

    fn runtime(&mut self, message: &str) {
        self.finish_any();
        eprintln!("[skyhook] {message}");
    }
}

fn agent_label(agent: &AgentId) -> String {
    if agent.path().is_empty() {
        return "root".to_owned();
    }
    agent
        .path()
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(":")
}

fn format_tool_call(
    job: skyhook::identity::JobId,
    tool: &str,
    arguments: &Value,
    bg: bool,
    location: &skyhook::execution::ExecutionLocation,
) -> String {
    let detail = match tool {
        "read" | "remove" => one_arg(arguments, "path"),
        "search" | "glob" => format!(
            "{} in {}",
            quoted_brief_arg(arguments, "pattern", 80),
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
        "ask" => format!(
            "question {}: {}",
            arg(arguments, "id").unwrap_or("<id>"),
            quoted_brief_arg(arguments, "prompt", 100)
        ),
        "todo" => format!(
            "{} item(s)",
            arguments
                .get("items")
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        ),
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
    let location = format!(
        "[target={} workspace={}]",
        location.target,
        location.workspace.display()
    );
    if detail.is_empty() {
        format!("tool #{job}: {location} {tool}{background}")
    } else {
        format!("tool #{job}: {location} {tool} {detail}{background}")
    }
}

fn arg<'a>(arguments: &'a Value, name: &str) -> Option<&'a str> {
    arguments.get(name).and_then(Value::as_str)
}

fn one_arg(arguments: &Value, name: &str) -> String {
    arg(arguments, name).unwrap_or("<unspecified>").to_owned()
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
    use skyhook::{
        execution::ExecutionLocation,
        identity::{AgentId, JobId, SessionId},
    };

    use super::{Args, agent_label, brief, format_tool_call, token_summary};

    #[test]
    fn token_summary_reports_total_and_uncached_input() {
        assert_eq!(
            token_summary(12, 34, 56),
            "tokens: 12 output, 90 total input, 34 uncached input"
        );
    }

    #[test]
    fn agent_labels_omit_the_session_id() {
        let root = AgentId::root(SessionId::from_bytes([0xab; 16]));
        assert_eq!(agent_label(&root), "root");
        assert_eq!(agent_label(&root.child(2)), "2");
        assert_eq!(agent_label(&root.child(2).child(3)), "2:3");
    }

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
        let location = ExecutionLocation::root("/workspace".into());
        let script = format_tool_call(
            JobId::new(7).unwrap(),
            "script",
            &json!({"source":"const secret = 'do not print'; return 42", "bg":true}),
            true,
            &location,
        );
        assert_eq!(
            script,
            "tool #7: [target=root workspace=/workspace] script JavaScript workflow (40 chars) [background]"
        );
        assert!(!script.contains("secret"));

        let agent = format_tool_call(
            JobId::new(8).unwrap(),
            "agent",
            &json!({"prompt":"report the kernel version", "target":"lab-monitoring"}),
            false,
            &location,
        );
        assert_eq!(
            agent,
            "tool #8: [target=root workspace=/workspace] agent task \"report the kernel version\" on lab-monitoring"
        );
        assert_eq!(brief("one\n two   three", 7), "one two…");

        let located = format_tool_call(
            JobId::new(9).unwrap(),
            "read",
            &json!({"path":"README.md"}),
            false,
            &location,
        );
        assert_eq!(
            located,
            "tool #9: [target=root workspace=/workspace] read README.md"
        );
    }
}
