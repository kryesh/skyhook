use std::{
    num::NonZeroU16,
    path::{Path, PathBuf},
    sync::Arc,
};

use clap::builder::PossibleValue;
use skyhook::{
    config::{Config, ConfigDiagnostic, LayeredConfig, workspace_session_root},
    identity::SessionId,
    session::SessionStore,
    tool::diagnostic::escape_controls,
};
use skyhook_cli::{
    Color, Configured, Error, Identity, Palette, Product, RootMode, SessionContext, SyntaxPalette,
    Table, Theme, page::Disclosed,
};

use crate::{
    config::SecurityConfig,
    cvss::Severity,
    session::{ResearchFactory, Scope},
    store::{PocPolicy, Run, Store},
};

/// Red emphasis in place of Skyhook's blue; code keeps the shared syntax palette.
/// Emphasis contrasts 3.5:1 on the selected row and 4.7:1 on the user surface;
/// errors move to orange so they never read as emphasis.
const THEME: Theme = Theme {
    ui: Palette {
        emphasis: Color::Rgb(255, 96, 106),
        strong: Color::Rgb(255, 130, 160),
        error: Color::Rgb(255, 128, 0),
        ..Palette::DEFAULT
    },
    syntax: SyntaxPalette::DEFAULT,
};

pub struct Security;

#[derive(clap::Args)]
pub struct Args {
    /// Live proof-of-concept policy (interactive: ask; batch: no).
    #[arg(long, value_enum)]
    pub poc: Option<PocPolicy>,
    /// Shortlist threshold for a new run: the lowest severity sent to review.
    #[arg(long, value_enum)]
    pub shortlist: Option<Severity>,
    /// How many agents the orchestrator's scheduler runs at once.
    #[arg(long)]
    pub workers: Option<NonZeroU16>,
    /// Explicit security.yaml; independent of --config.
    #[arg(long)]
    pub security_config: Option<PathBuf>,
}

impl clap::ValueEnum for Severity {
    fn value_variants<'a>() -> &'a [Self] {
        &Self::ALL
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        Some(PossibleValue::new(self.as_str()))
    }
}

#[derive(clap::Subcommand)]
pub enum Command {
    /// List the workspace's security sessions, each one run, or show one as its Research
    /// page with every section open in full.
    Runs {
        #[arg(short, long, default_value = ".")]
        workspace: PathBuf,
        session: Option<SessionId>,
    },
}

impl Product for Security {
    type Args = Args;
    type Command = Command;

    fn identity(&self) -> Identity {
        Identity {
            host: "skyhook-security".parse().expect("static host identity"),
            tables: Some(crate::store::EXTENSION),
            display_name: "Skyhook Security",
            about: "Evidence-led security research over a flaw-first ledger",
            placeholder: "Set the scope above and describe the target to start the security analysis, or /sessions to open one.",
            state_file: "security-state.json",
            root_mode: RootMode::Fixed,
            theme: THEME,
            capabilities: crate::capabilities::declarations().collect(),
        }
    }

    async fn configure(
        &self,
        config: &mut Config,
        args: Args,
        context: SessionContext<'_>,
    ) -> Result<Configured, Error> {
        let resolved =
            SecurityConfig::resolve(context.workspace, args.security_config.as_deref()).await?;
        let security = resolved.config;
        // The policy is checked once, on what the flag and configuration resolve to.
        let scope = Scope {
            options: security.run_options(args.shortlist, args.workers),
            poc: security.poc_policy(args.poc, context.headless)?,
        };
        security.apply(config)?;
        let mut report = resolved.report;
        if security
            .models
            .values()
            .all(|assignment| assignment.roles.is_empty())
        {
            report.diagnostics.push(ConfigDiagnostic {
                path: report
                    .sources
                    .last()
                    .cloned()
                    .unwrap_or_else(|| PathBuf::from(SecurityConfig::FILE_NAME)),
                message: "no model role hints configured; every agent runs on the default model (see security.example.yaml)".to_owned(),
            });
        }
        let factory = ResearchFactory::new(scope, args.workers, security.modes.clone());
        Ok(Configured {
            report,
            factory: Arc::new(factory),
        })
    }

    async fn command(&self, command: Command) -> Result<(), Error> {
        match command {
            Command::Runs {
                workspace,
                session: Some(session),
            } => {
                let root = workspace_session_root(&workspace);
                let store = Store::inspect(&root, session).await?;
                let text = tokio::task::spawn_blocking(move || run_detail(&store)).await??;
                print!("{text}");
                Ok(())
            }
            Command::Runs {
                workspace,
                session: None,
            } => {
                let runs = runs(&workspace_session_root(&workspace), &self.identity()).await?;
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
                table.render(&mut output);
                output.extend(unavailable.iter().map(|line| format!("{line}\n")));
                print!("{output}");
                Ok(())
            }
        }
    }
}

/// Every session `identity` created under `root`, oldest first, with its run or why it
/// cannot be read. Other hosts' sessions are not runs.
async fn runs(
    root: &Path,
    identity: &Identity,
) -> Result<Vec<(SessionId, Result<(Run, u64), Error>)>, Error> {
    let mut runs = Vec::new();
    for id in SessionStore::ids(root).await? {
        let summary = SessionStore::summary(root, id, identity.owner());
        let summary = match summary.await {
            Ok(summary) if summary.host != identity.host => continue,
            Ok(_) => {
                async {
                    let store = Store::inspect(root, id).await?;
                    let read = move || {
                        Ok::<_, crate::store::Error>((store.run()?, store.coverage()?.findings))
                    };
                    Ok::<_, Error>(tokio::task::spawn_blocking(read).await??)
                }
                .await
            }
            Err(error) => Err(error.into()),
        };
        runs.push((id, summary));
    }
    runs.sort_by_key(|(id, run)| (run.as_ref().ok().map(|(run, _)| run.created), *id));
    Ok(runs)
}

/// One run as plain text: the Research page with every section open in full, as the store
/// holds it; never a report the agents should write.
fn run_detail(store: &Store) -> Result<String, crate::store::Error> {
    let state = store.run_state()?;
    let mut out = String::new();
    (state.page(store, &Disclosed::whole())?).render(&mut out);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::{component, fixture};

    /// A run's detail lists every record of a section, however many there are.
    #[test]
    fn run_detail_lists_every_record() {
        let (_session, store, actor) = fixture();
        let count = Disclosed::PAGE + 1;
        for index in 0..count {
            component(&store, &actor, &format!("part{index}"));
        }
        let detail = run_detail(&store).unwrap();
        let listed = (detail.lines())
            .filter(|line| {
                (line.split_whitespace().last()).is_some_and(|name| name.starts_with("part"))
            })
            .count();
        assert_eq!(listed, count, "{detail}");
    }
}
