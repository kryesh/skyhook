mod interaction;
mod tui;
mod embedded_shims {
    include!(concat!(env!("OUT_DIR"), "/embedded_shims.rs"));
}
use clap::Parser;
use skyhook::identity::SessionId;
use std::path::PathBuf;
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
    /// Select and remember the root model for a new session.
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
    /// Open the interface and submit this initial prompt.
    #[arg(short, long, conflicts_with = "script")]
    prompt: Option<String>,
    /// Open the interface and run this JavaScript workflow.
    #[arg(short, long, value_name = "PATH", conflicts_with = "prompt")]
    script: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    if std::env::args().nth(1).as_deref() == Some("--askpass") {
        let Some(socket) = std::env::var_os("SKYHOOK_ASKPASS_SOCKET") else {
            std::process::exit(1)
        };
        let prompt = std::env::args().nth(2).unwrap_or_default();
        if let Err(error) =
            skyhook::remote::run_askpass_helper(std::path::Path::new(&socket), prompt)
        {
            eprintln!("skyhook askpass: {error}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(error) = tui::run(Args::parse()).await {
        eprintln!("skyhook: {error}");
        std::process::exit(1);
    }
}
