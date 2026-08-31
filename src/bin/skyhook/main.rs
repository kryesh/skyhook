mod interaction;

use std::{path::PathBuf, sync::Arc};

use clap::Parser;
use skyhook::{agent::RuntimeEvent, config::Config, identity::SessionId};
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
    #[arg(long)]
    model_profile: Option<String>,
    /// Override the configured root instruction profile.
    #[arg(long)]
    agent_profile: Option<String>,
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
    if let Some(profile) = args.model_profile {
        config.default_model_profile = profile;
    }
    if args.agent_profile.is_some() {
        config.default_agent_profile = args.agent_profile;
    }
    let interaction = Arc::new(CliInteraction::default());
    // `Config::build_harness` is convenient for embedders; the CLI adds its interactive hooks.
    let harness = config
        .harness_builder(args.workspace)?
        .policy(Arc::new(CliPolicy::new(interaction.clone())))
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
        use std::io::Write as _;
        loop {
            match events.recv().await {
                Ok(RuntimeEvent::TextDelta { text, .. }) => {
                    print!("{text}");
                    let _ = std::io::stdout().flush();
                }
                Ok(RuntimeEvent::TurnCompleted { .. }) => println!(),
                Ok(RuntimeEvent::Record(_) | RuntimeEvent::ReasoningDelta { .. })
                | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
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

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::Args;

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
    }
}
