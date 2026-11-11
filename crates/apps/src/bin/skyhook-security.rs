//! The security product in the terminal: its command line, look and `runs` command.
use std::io;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};

use skyhook::config::{Refused, undeclared_capabilities, workspace_session_root};
use skyhook::identity::SessionId;
use skyhook::product::{Error, Identity, Product, text::Table};
use skyhook::tool::policy::HostCapabilityDeclaration;
use skyhook_apps::args::{self, Invocation, ProductArgs, named};
use skyhook_apps::config::{LayerResolutionError, LayeredConfig, ResolvedConfig, resolve_layers};
use skyhook_apps::tui::{self, Color, Look, Palette, SyntaxPalette, Theme};
use skyhook_apps::{auth, batch, dump, exit_on, stats, text};
use skyhook_security::{
    LivePolicy, Options, Security, SecurityConfig, Severity, Store, run_page, runs,
};
use skyhook_tools::diagnostic::escape_controls;

#[cfg(not(windows))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const ABOUT: &str = "Evidence-led security research over a flaw-first ledger";
const STATE: &str = "security-state.json";

/// Red emphasis in place of Skyhook's blue; code keeps the shared syntax palette.
/// Emphasis contrasts 3.5:1 on the selected row and 4.7:1 on the user surface;
/// errors move to orange so they never read as emphasis.
const LOOK: Look = Look {
    theme: Theme {
        ui: Palette {
            emphasis: Color::Rgb(255, 96, 106),
            strong: Color::Rgb(255, 130, 160),
            error: Color::Rgb(255, 128, 0),
            ..Palette::DEFAULT
        },
        syntax: SyntaxPalette::DEFAULT,
    },
    placeholder: "Set the scope above and describe the target to start the security analysis, or /sessions to open one.",
};

fn main() {
    let identity = Security.identity();
    let invocation = args::parse::<Args, Command, _>(&Security, ABOUT);
    // SAFETY: no threads or runtime have been started.
    let runtime = unsafe { skyhook_apps::start(&identity) };
    let name = identity.name();
    runtime.block_on(async {
        match invocation {
            Invocation::Product(Command::Runs { workspace, session }) => {
                exit_on(name, list_runs(&workspace, session, &identity).await)
            }
            Invocation::Auth(command) => {
                exit_on(&format!("{name} auth"), auth::run(command, &identity).await)
            }
            Invocation::Dump(request) => exit_on(
                &format!("{name} dump"),
                dump::run(&Security, request, &mut io::stdout()).await,
            ),
            Invocation::Stats(request) => exit_on(
                &format!("{name} stats"),
                stats::run(request, &identity, &mut io::stdout().lock()).await,
            ),
            Invocation::Batch(job) => exit_on(
                name,
                batch::run(&Security, job, STATE, &mut io::stdout(), &mut io::stderr()).await,
            ),
            // The Research tab and the scope form come with `Security`'s sessions: its
            // factory's form and its extension's views, as any host gets them.
            Invocation::Interactive(session) => {
                exit_on(name, tui::run(&Security, session, STATE, LOOK).await)
            }
        }
    });
}

#[derive(clap::Args)]
struct Args {
    /// Whether a new run's agents work against a running target: probing it and demonstrating
    /// findings (interactive: ask; batch: no). A resumed run keeps its own.
    #[arg(long, group = "new", value_parser = named::<LivePolicy>())]
    live: Option<LivePolicy>,
    /// Shortlist threshold for a new run: the lowest severity a review judges in full.
    #[arg(long, group = "new", value_parser = named::<Severity>())]
    shortlist: Option<Severity>,
    /// How many agents the orchestrator's scheduler runs at once.
    #[arg(long)]
    workers: Option<NonZeroU16>,
    /// Explicit security.yaml; independent of --config.
    #[arg(long)]
    security_config: Option<PathBuf>,
}

impl ProductArgs<Options> for Args {
    async fn load(self, workspace: &Path) -> Result<ResolvedConfig<Options>, LayerResolutionError> {
        let declared = Security.identity().capabilities;
        let explicit = self.security_config.as_deref();
        let resolved = resolve_layers::<File>(workspace, explicit, &declared).await?;
        let (File(mut config), report) = (resolved.config, resolved.report);
        let last = report.sources.last().cloned();
        config.file = Some(last.unwrap_or_else(|| File::FILE_NAME.into()));
        let options = Options {
            live: self.live,
            shortlist: self.shortlist,
            workers: self.workers,
            config,
        };
        Ok(ResolvedConfig {
            config: options,
            report,
        })
    }
}

/// `security.yaml`, layered as `config.yaml` is: the product's settings, each MCP server
/// working where its file says and each model assignment where it is written.
#[derive(skyhook_base::decode::Decode)]
#[decode(transparent)]
struct File(SecurityConfig);

impl LayeredConfig for File {
    type Context<'a> = &'a [HostCapabilityDeclaration];

    const FILE_NAME: &'static str = "security.yaml";
    const NAMED_ENTRIES: &'static [&'static str] = &["models", "modes", "mcp"];
    const OPTIONAL: bool = true;

    fn check(&self, declared: Self::Context<'_>, _complete: bool) -> Vec<Refused> {
        undeclared_capabilities(&self.0.modes, &self.0.mcp, declared)
    }
}

#[derive(clap::Subcommand)]
enum Command {
    /// List the workspace's security sessions, each one run, or show one as its Research
    /// page with every section open in full.
    Runs {
        #[arg(short, long, default_value = ".")]
        workspace: PathBuf,
        session: Option<SessionId>,
    },
}

/// Every run in `workspace`'s sessions as a table, or `session`'s run as its Research page.
async fn list_runs(
    workspace: &Path,
    session: Option<SessionId>,
    identity: &Identity,
) -> Result<(), Error> {
    let root = workspace_session_root(workspace);
    if let Some(session) = session {
        let store = Store::inspect(&root, session).await?;
        let page = tokio::task::spawn_blocking(move || run_page(&store)).await??;
        let mut output = String::new();
        text::page(&page, &mut output);
        print!("{output}");
        return Ok(());
    }
    let runs = runs(&root, identity).await?;
    if runs.is_empty() {
        println!("No security runs.");
        return Ok(());
    }
    let mut table = Table {
        header: &["Session", "Created", "Findings"],
        numeric_from: 2,
        rows: Vec::new(),
    };
    let mut unavailable = Vec::new();
    for (id, run) in runs {
        match run {
            Ok((run, findings)) => {
                table.rows.push(vec![
                    run.id.to_string(),
                    run.created.to_rfc3339(),
                    findings.to_string(),
                ]);
            }
            Err(error) => unavailable.push(format!(
                "{id}  unavailable: {}",
                escape_controls(error.to_string())
            )),
        }
    }
    let mut output = String::new();
    text::table(&table, &mut output);
    output.extend(unavailable.iter().map(|line| format!("{line}\n")));
    print!("{output}");
    Ok(())
}
