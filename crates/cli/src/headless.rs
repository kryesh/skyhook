//! A single root operation with journaled diagnostics and deterministic cleanup.
use super::{
    InitialInput, Opening,
    args::{BatchRequest, PermissionArgs},
    launch::{self, ConfiguredHost, Launch, LaunchError, OperationError, Permissions},
};
use skyhook::agent::{HarnessError, SessionHandle};
use std::io::{self, Write};
use tokio::signal::unix::{SignalKind, signal};

/// Why a batch job failed.
#[derive(Debug, thiserror::Error)]
pub enum BatchError {
    #[error(transparent)]
    Launch(#[from] LaunchError),
    #[error("could not watch for termination signals: {0}")]
    Signals(#[source] io::Error),
    /// The job itself failed; its session journals why.
    #[error(transparent)]
    Failed(#[from] Failure),
    #[error("could not record the final status: {0}")]
    Status(#[from] HarnessError),
    /// The job failed, and recording why failed too.
    #[error("{0}; could not record the final status: {1}")]
    Unrecorded(Box<Failure>, HarnessError),
}

#[derive(Debug, thiserror::Error)]
pub enum Failure {
    #[error(transparent)]
    Operation(#[from] OperationError),
    /// The resumed root's model, which the configuration no longer has.
    #[error("unknown model `{0}`; resume with --model to run on another")]
    ModelGone(skyhook::provider::profile::ModelRef),
    #[error("Interrupted by {0}")]
    Signal(&'static str),
    #[error("{0}; shutdown failed: {1}")]
    Shutdown(Box<Self>, HarnessError),
}

/// Run the batch job, announcing its session id on `announce` before any input is read.
pub async fn run(
    request: BatchRequest,
    input: InitialInput,
    host: ConfiguredHost,
    announce: &mut impl Write,
) -> Result<(), BatchError> {
    let BatchRequest {
        execution: request,
        permissions,
    } = request;
    let config = &host.config;
    let selected = !matches!(permissions, PermissionArgs::Mode(None));
    launch::admit_root_mode(&host.identity, config, selected)?;
    // Model memory is shared with terminal launches, but UI settings are never read.
    let (saved, state_warning) = super::state::load(&host.state);
    let model = launch::select_model(config, request.model.as_ref(), saved.model.as_ref())?;
    // A named mode also applies to a resumed session, from this prompt on.
    let mode = match &permissions {
        PermissionArgs::Mode(Some(mode)) => Some(mode.clone()),
        _ => None,
    };
    let permissions = Permissions::for_batch(&permissions, request.resume.is_some(), config)
        .map_err(LaunchError::from)?;
    let launch = Launch::new(model, permissions, None, host)?;
    let watch = |kind| signal(kind).map_err(BatchError::Signals);
    let mut terminate = watch(SignalKind::terminate())?;
    let mut hangup = watch(SignalKind::hangup())?;
    let mut interrupt = watch(SignalKind::interrupt())?;
    let opening = request.resume.map_or(Opening::New(None), Opening::Resume);
    let opened = launch.create(opening).await?;
    let session = &opened.session;

    // This must be the first action after open, before reading a workflow/image or
    // contacting a provider. Flush explicitly so pipe consumers can follow events.
    let announced = writeln!(announce, "{}", session.id()).and_then(|()| announce.flush());
    let outcome = if let Err(error) = announced {
        Err(OperationError::from(error).into())
    } else {
        let operation = async {
            opened
                .extension
                .opened(session, launch.context(opening))
                .await?;
            // A resumed session switches to a named model before anything else runs.
            if let (Some(_), Some(model)) = (request.resume, &request.model) {
                session.activate_on(model).await?;
            }
            for warning in session.warnings().iter().chain(state_warning.iter()) {
                session
                    .record_status(
                        session.root_agent().clone(),
                        format!("Startup warning: {warning}"),
                    )
                    .await?;
            }
            let model = launch.model.name();
            if request.resume.is_none()
                && saved.model.as_ref() != Some(&model)
                && let Err(error) = super::state::update(&launch.state, |state| {
                    state.model = Some(model);
                })
            {
                session
                    .record_status(
                        session.root_agent().clone(),
                        format!("Could not save model selection: {error}"),
                    )
                    .await?;
            }
            run_input(session, &launch.workspace, input, mode).await
        };
        tokio::select! {
            result = operation => result.map_err(|error| match error {
                OperationError::Harness(HarnessError::UnknownModel(model))
                    if request.resume.is_some() => Failure::ModelGone(model),
                error => error.into(),
            }),
            _ = terminate.recv() => Err(Failure::Signal("SIGTERM")),
            _ = hangup.recv() => Err(Failure::Signal("SIGHUP")),
            _ = interrupt.recv() => Err(Failure::Signal("SIGINT")),
        }
    };
    // Root completion is not session quiescence: cancel/drain background tools
    // and child agents, including after interruption or input preparation errors.
    let outcome = match (outcome, session.shutdown().await) {
        (result, Ok(())) => result,
        (Ok(()), Err(error)) => Err(OperationError::from(error).into()),
        // Shutdown reports again the failed journal that stopped the operation.
        (Err(Failure::Operation(error)), Err(_)) if error.journal_failed() => Err(error.into()),
        (Err(error), Err(cleanup)) => Err(Failure::Shutdown(Box::new(error), cleanup)),
    };
    // A session only viewed, or whose journal failed, records nothing, its final
    // status included.
    if session.phase().runs() {
        let status = match &outcome {
            Ok(()) => "Completed".to_owned(),
            Err(error) => format!("Failed: {error}"),
        };
        let recorded = session.record_status(session.root_agent().clone(), status);
        if let Err(status) = recorded.await {
            return Err(match outcome {
                Ok(()) => status.into(),
                Err(failure) => BatchError::Unrecorded(Box::new(failure), status),
            });
        }
    }
    Ok(outcome?)
}

async fn run_input(
    session: &SessionHandle,
    workspace: &std::path::Path,
    input: InitialInput,
    mode: Option<skyhook::tool::policy::ModeName>,
) -> Result<(), OperationError> {
    match input {
        InitialInput::Script(path) => {
            let source = tokio::fs::read_to_string(path).await?;
            session.run_script(source).await?;
        }
        InitialInput::Prompt { text, images } => {
            let attachments = launch::read_images(workspace, &images).await?;
            let selection = session.selection(None, mode.as_ref())?;
            session
                .prompt_with_options(text, &attachments, selection)
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        args::{Invocation, tests::parse_from},
        interaction::tests::{test_config, two_models},
    };
    use skyhook::{config::Config, identity::SessionId, session::SessionStore};
    use std::assert_matches;
    use std::path::PathBuf;

    /// A workspace whose `config.yaml` its batch jobs run with.
    pub(crate) struct Workspace(tempfile::TempDir);

    impl Workspace {
        pub(crate) fn new(config: Config) -> Self {
            let workspace = Self(tempfile::tempdir().unwrap());
            workspace.configure(config);
            workspace
        }

        pub(crate) fn path(&self, name: &str) -> PathBuf {
            self.0.path().join(name)
        }

        fn configure(&self, config: Config) {
            self.write("config.yaml", &config.to_yaml().unwrap());
        }

        pub(crate) fn write(&self, name: &str, text: &str) -> String {
            let path = self.path(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path.to_str().unwrap().to_owned()
        }

        /// Run `batch` with `args`: its outcome and the session it announced.
        pub(crate) async fn batch(
            &self,
            args: &[&str],
        ) -> (Result<(), BatchError>, Option<SessionId>) {
            let (root, config) = (self.path(""), self.path("config.yaml"));
            let common = ["skyhook", "batch", "--workspace", root.to_str().unwrap()];
            let config = ["--config", config.to_str().unwrap()];
            let Ok(Invocation::Headless(request, input, args)) =
                parse_from([&common[..], &config, args].concat())
            else {
                panic!("batch invocation");
            };
            let mut announced = Vec::new();
            let result = async {
                let host =
                    launch::configure(&crate::Skyhook, args, &request.execution, true).await?;
                run(request, input, host, &mut announced).await
            }
            .await;
            let announced = String::from_utf8(announced).unwrap();
            let id = (!announced.is_empty()).then(|| {
                let id = announced.strip_suffix('\n').expect("the id ends its line");
                id.parse().expect("only the session id is announced")
            });
            (result, id)
        }

        /// The session's journal, as JSON.
        async fn journal(&self, id: SessionId) -> String {
            let sessions = self.path(".skyhook/sessions");
            let records = SessionStore::read_records(&sessions, id).await.unwrap();
            serde_json::to_string(&records).unwrap()
        }
    }

    /// A job journals its startup warnings, its model, its tools' work under `--approve-all`
    /// and its status; an explicit model is remembered for the workspace, and a resume
    /// appends to the same session.
    #[tokio::test]
    async fn a_batch_job_journals_its_run_and_a_resume_appends() {
        let workspace = Workspace::new(two_models());
        workspace.write(".skyhook/state.json", "invalid JSON");
        let script = workspace.write(
            "run.js",
            "return (await tool.exec({command:['sh','-c','echo approved-output']})).unwrap();",
        );
        let args = [
            "-m",
            "test/second",
            "--capabilities",
            "exec",
            "--approve-all",
        ];
        let (result, id) = workspace
            .batch(&[&args[..], &["-s", &script]].concat())
            .await;
        result.unwrap();
        let id = id.unwrap();
        let journal = workspace.journal(id).await;
        for expected in [
            "Startup warning: Could not read UI state",
            r#""profile":{"name":"test/second""#,
            "approved-output",
            "Completed",
        ] {
            assert!(journal.contains(expected), "{expected}: {journal}");
        }
        let (saved, _) = crate::state::load(&workspace.path(".skyhook/state.json"));
        assert_eq!(saved.model, Some("test/second".parse().unwrap()));

        let resumed = workspace.write("resumed.js", "return 'resumed-result';");
        let (result, again) = workspace
            .batch(&["--resume", &id.to_string(), "-s", &resumed])
            .await;
        result.unwrap();
        assert_eq!(again, Some(id));
        let appended = workspace.journal(id).await;
        let before = journal.strip_suffix(']').unwrap();
        assert!(appended.starts_with(before) && appended.contains("resumed-result"));
    }

    /// A job that cannot open announces nothing; one that fails, its input included, is
    /// journaled as failed; a resumed session that never ran journals nothing; and a resumed
    /// session whose model the configuration dropped points to `--model`.
    #[tokio::test]
    async fn failures_are_journaled_once_a_job_runs() {
        let workspace = Workspace::new(two_models());
        let missing = "00000000000000000000000000000001";
        for args in [
            &["-p", "x", "--mode", "missing"][..],
            &["-p", "x", "--resume", missing],
        ] {
            let (result, id) = workspace.batch(args).await;
            assert!(result.is_err() && id.is_none(), "{args:?}");
        }
        assert!(!workspace.path(".skyhook/sessions").exists());

        let thrown = workspace.write("throw.js", "throw new Error('deliberate-failure');");
        let (result, id) = workspace.batch(&["-m", "test/second", "-s", &thrown]).await;
        assert_matches!(result, Err(BatchError::Failed(_)));
        let id = id.unwrap();
        let journal = workspace.journal(id).await;
        assert!(journal.contains("Failed:") && journal.contains("deliberate-failure"));
        let (result, image) = workspace
            .batch(&["-p", "x", "--image", "missing.png"])
            .await;
        assert_matches!(result, Err(BatchError::Failed(_)));
        assert!(workspace.journal(image.unwrap()).await.contains("Failed:"));

        let (absent, id) = (workspace.path("absent.js"), id.to_string());
        let (result, again) = workspace
            .batch(&["--resume", &id, "-s", absent.to_str().unwrap()])
            .await;
        assert_eq!(again.map(|again| again.to_string()), Some(id.clone()));
        assert!(result.unwrap_err().to_string().contains("No such file"));
        assert_eq!(workspace.journal(id.parse().unwrap()).await, journal);

        workspace.configure(test_config());
        let (result, _) = workspace.batch(&["--resume", &id, "-s", &thrown]).await;
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("unknown model `test/second`; resume with --model"),
            "{error}"
        );
    }
}
