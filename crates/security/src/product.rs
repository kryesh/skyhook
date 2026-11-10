use std::{num::NonZeroU16, path::Path, sync::Arc};

use skyhook::identity::SessionId;
use skyhook::product::{
    Configured, Error, Identity, Product, RootMode, SessionContext,
    page::{Disclosed, Page},
};
use skyhook::{config::Config, session::SessionStore};

use crate::{
    config::SecurityConfig,
    cvss::Severity,
    session::ResearchFactory,
    store::{LivePolicy, Run, Store},
};

pub struct Security;

/// Why a launch is refused before its session opens.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Refused {
    #[error(
        "live: ask requires an interactive session; use --live yes or --live no in headless mode"
    )]
    BatchAsk,
}

/// What a launch of the security product runs with, beside the shared configuration.
pub struct Options {
    /// Whether a new run's agents work against a running target; a resumed run keeps its own.
    pub live: Option<LivePolicy>,
    /// Shortlist threshold for a new run: the lowest severity a review judges in full.
    pub shortlist: Option<Severity>,
    /// How many agents the orchestrator's scheduler runs at once.
    pub workers: Option<NonZeroU16>,
    /// The product's own settings, as the host read them.
    pub config: SecurityConfig,
}

impl Product for Security {
    type Options = Options;

    fn identity(&self) -> Identity {
        Identity {
            host: "skyhook-security".parse().expect("static host identity"),
            schema: crate::store::SCHEMA,
            display_name: "Skyhook Security",
            root_mode: RootMode::Fixed,
            capabilities: crate::capabilities::declarations().collect(),
        }
    }

    async fn configure(
        &self,
        config: &mut Config,
        requested: Options,
        context: SessionContext<'_>,
    ) -> Result<Configured, Error> {
        let security = &requested.config;
        let options = security.run_options(
            requested.shortlist,
            requested.workers,
            requested.live,
            context.attendance,
        );
        let factory = ResearchFactory::new(options, requested.workers);
        security.apply(config)?;
        Ok(Configured {
            warnings: security.warnings(),
            factory: Arc::new(factory),
        })
    }
}

/// Every session `identity` created under `root`, oldest first, with its run or why it
/// cannot be read. Other hosts' sessions are not runs.
pub async fn runs(
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
                        let run = store.started()?.ok_or(crate::store::Error::NotFound {
                            record: crate::store::Record::Run,
                            id: None,
                        })?;
                        Ok::<_, crate::store::Error>((run, store.coverage()?.findings))
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

/// One run as its Research page with every section open in full, as the store holds it.
pub fn run_page(store: &Store) -> Result<Page, Error> {
    Ok(store.run_state()?.page(store, &Disclosed::whole(), None)?)
}

#[cfg(test)]
mod tests {
    use skyhook::config::workspace_session_root;
    use skyhook::product::{Attendance, Opening};
    use std::assert_matches;

    use super::*;
    use crate::store::tests::{component, fixture, harness};
    use skyhook::product::page::{Block, Record};

    /// A run's page supplies every record of a section, however many there are.
    #[test]
    fn run_page_supplies_every_record() {
        let (_session, store, actor) = fixture();
        let count = Disclosed::PAGE + 1;
        for index in 0..count {
            component(&store, &actor, &format!("part{index}"));
        }
        let page = run_page(&store).unwrap();
        let records = (page.blocks.iter()).flat_map(|block| match block {
            Block::Section(section) => section.records.as_slice(),
            _ => &[],
        });
        let named = |record: &&Record| record.cells.iter().any(|c| c.text.starts_with("part"));
        assert_eq!(records.filter(named).count(), count, "{page:?}");
    }

    /// The product's configuration for `opening` of `workspace` under the `configured`
    /// policy, with `live` given on the command line, once its session is prepared.
    async fn configure(
        workspace: &Path,
        opening: Opening<'_>,
        attendance: Attendance,
        (configured, live): (LivePolicy, Option<LivePolicy>),
    ) -> Result<Configured, Error> {
        let args = Options {
            live,
            shortlist: None,
            workers: None,
            config: SecurityConfig {
                live: Some(configured),
                ..SecurityConfig::default()
            },
        };
        let context = SessionContext {
            workspace,
            opening,
            attendance,
        };
        let mut config = Config::from_yaml("{}").unwrap();
        let configured = Security.configure(&mut config, args, context).await?;
        configured
            .factory
            .prepare(harness(workspace), context)
            .await?;
        Ok(configured)
    }

    /// The run a new session of `configured` starts in the terminal.
    async fn start(workspace: &Path, configured: Configured) -> SessionId {
        let context = SessionContext {
            workspace,
            opening: Opening::New(None),
            attendance: Attendance::Attended,
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

    /// Why a launch was refused.
    fn refusal(result: Result<Configured, Error>) -> Refused {
        let error = result.err().expect("the launch is refused");
        *error.downcast().unwrap()
    }

    /// A batch launch is refused when its run would ask: a new run under the command
    /// line's or the configuration's policy, a resumed run under its own. `runs` lists
    /// every run.
    #[tokio::test]
    async fn batch_admission_follows_the_policy_the_run_is_under() {
        use Refused::BatchAsk;
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path();
        let ask = (LivePolicy::Ask, None);
        let new = Opening::New(None);
        let refused = refusal(configure(workspace, new, Attendance::Batch, ask).await);
        assert_matches!(refused, BatchAsk);
        let quiet = (LivePolicy::Ask, Some(LivePolicy::No));
        let quiet = configure(workspace, new, Attendance::Batch, quiet).await;
        let quiet = start(workspace, quiet.unwrap()).await;
        let asking = start(
            workspace,
            configure(workspace, new, Attendance::Attended, ask)
                .await
                .unwrap(),
        )
        .await;
        let resume = Opening::Resume(quiet);
        configure(workspace, resume, Attendance::Batch, ask)
            .await
            .unwrap();
        let no = (LivePolicy::No, None);
        let refused =
            refusal(configure(workspace, Opening::Resume(asking), Attendance::Batch, no).await);
        assert_matches!(refused, BatchAsk);

        let listed = runs(&workspace_session_root(workspace), &Security.identity()).await;
        let listed: Vec<_> = (listed.unwrap().into_iter())
            .map(|(id, run)| (id, run.unwrap().0.options.live))
            .collect();
        assert_eq!(listed.len(), 2);
        for run in [(quiet, LivePolicy::No), (asking, LivePolicy::Ask)] {
            assert!(listed.contains(&run), "{listed:?}");
        }
    }
}
