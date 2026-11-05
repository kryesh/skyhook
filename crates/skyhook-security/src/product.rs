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
    Color, Configured, Error, Identity, Opening, Palette, Product, RootMode, SessionContext,
    SyntaxPalette, Table, Theme, page::Disclosed,
};

use crate::{
    config::SecurityConfig,
    cvss::Severity,
    session::{ResearchFactory, Scope, resumed_policy},
    store::{LivePolicy, Run, Store},
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
    /// Whether a new run's agents work against a running target: probing it and demonstrating
    /// findings (interactive: ask; batch: no). A resumed run keeps its own.
    #[arg(long, value_enum)]
    pub live: Option<LivePolicy>,
    /// Shortlist threshold for a new run: the lowest severity a review judges in full.
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
            schema: crate::store::SCHEMA,
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
        if let (Opening::Resume(_), Some(_)) = (context.opening, args.live) {
            return Err(crate::config::Error::LiveOnResume.into());
        }
        let scope = Scope {
            options: security.run_options(args.shortlist, args.workers),
            live: security.live_policy(args.live, context.headless),
        };
        let factory = ResearchFactory::new(scope, args.workers);
        // The policy is checked once, on what the opened session runs under; another host's
        // session opens without this product's resources.
        let (live, factory) = match context.opening {
            Opening::Resume(id) if self.owns(context.workspace, id).await? => {
                let live = resumed_policy(context.workspace, id, scope.live).await?;
                (live, factory.resuming(id, live))
            }
            Opening::New(_) | Opening::Resume(_) => (scope.live, factory),
        };
        if context.headless && live == LivePolicy::Ask {
            return Err(crate::config::Error::HeadlessAsk.into());
        }
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

impl Security {
    /// Whether session `id` of `workspace` is one of this product's own.
    async fn owns(&self, workspace: &Path, id: SessionId) -> Result<bool, Error> {
        let identity = self.identity();
        let root = workspace_session_root(workspace);
        let summary = SessionStore::summary(&root, id, Some(identity.owner())).await?;
        Ok(summary.host == identity.host)
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
        let summary = SessionStore::summary(root, id, Some(identity.owner()));
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
    (state.page(store, &Disclosed::whole(), None)?).render(&mut out);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use crate::store::tests::{component, fixture, harness};

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

    /// The product's configuration for `opening` of `workspace`, from the `security.yaml` in
    /// it, with `live` given on the command line.
    async fn configure(
        workspace: &Path,
        opening: Opening<'_>,
        headless: bool,
        live: Option<LivePolicy>,
    ) -> Result<Configured, Error> {
        let args = Args {
            live,
            shortlist: None,
            workers: None,
            security_config: Some(workspace.join("security.yaml")),
        };
        let context = SessionContext {
            workspace,
            opening,
            headless,
        };
        let mut config = Config::from_yaml("{}").unwrap();
        Security.configure(&mut config, args, context).await
    }

    /// The run a new session of `configured` starts.
    async fn start(workspace: &Path, configured: Configured) -> SessionId {
        let context = SessionContext {
            workspace,
            opening: Opening::New(None),
            headless: true,
        };
        let prepared = configured.factory.prepare(harness(workspace), context);
        let prepared = prepared.await.unwrap();
        let session = prepared.builder.build().await.unwrap().new_session().await;
        let session = session.unwrap();
        prepared.extension.opened(&session, context).await.unwrap();
        session.shutdown().await.unwrap();
        session.close().await.unwrap();
        session.id()
    }

    /// Why configuration refused a launch.
    fn refusal(result: Result<Configured, Error>) -> crate::config::Error {
        let error = result.err().expect("the launch is refused");
        *error.downcast().unwrap()
    }

    /// A headless launch is refused when its run would ask: a new run under the command
    /// line's or the configuration's policy, a resumed run under its own, which `--live`
    /// cannot change. `runs` lists every run.
    #[tokio::test]
    async fn headless_admission_follows_the_policy_the_run_is_under() {
        use crate::config::Error::{HeadlessAsk, LiveOnResume};
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path();
        let policy = |live: &str| {
            std::fs::write(workspace.join("security.yaml"), format!("live: {live}\n")).unwrap()
        };
        policy("ask");
        let new = Opening::New(None);
        let refused = refusal(configure(workspace, new, true, None).await);
        assert_matches!(refused, HeadlessAsk);
        let quiet = configure(workspace, new, true, Some(LivePolicy::No)).await;
        let quiet = start(workspace, quiet.unwrap()).await;
        let asking = start(
            workspace,
            configure(workspace, new, false, None).await.unwrap(),
        )
        .await;
        let resume = Opening::Resume(quiet);
        configure(workspace, resume, true, None).await.unwrap();
        let live = Some(LivePolicy::Yes);
        let refused = refusal(configure(workspace, resume, true, live).await);
        assert_matches!(refused, LiveOnResume);
        policy("no");
        let refused = refusal(configure(workspace, Opening::Resume(asking), true, None).await);
        assert_matches!(refused, HeadlessAsk);

        let listed = runs(&workspace_session_root(workspace), &Security.identity()).await;
        let listed: Vec<_> = (listed.unwrap().into_iter())
            .map(|(id, run)| (id, run.unwrap().0.live))
            .collect();
        assert_eq!(listed.len(), 2);
        for run in [(quiet, LivePolicy::No), (asking, LivePolicy::Ask)] {
            assert!(listed.contains(&run), "{listed:?}");
        }
    }
}
