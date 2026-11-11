//! A single root operation with journaled diagnostics and deterministic cleanup.
use crate::OperationError;
use crate::args::{ConfigArgs, ProductArgs};
use crate::signals::{Signals, Unwatched};
use crate::state::Prefs;
use skyhook::agent::{HarnessError, SessionHandle};
use skyhook::launch::{
    Interface, LaunchError,
    request::{ExecutionRequest, InitialInput, PermissionRequest},
    startup,
};
use skyhook::product::{Attendance, Opening, Product};
use skyhook::remote::ArtifactError;
use std::io::Write;

/// A batch job: its configuration, its session, its root's permissions, its input and the
/// product's options as the command line gave them.
pub struct BatchRequest<A> {
    pub(crate) config: ConfigArgs,
    pub(crate) execution: ExecutionRequest,
    pub(crate) permissions: PermissionRequest,
    pub(crate) input: InitialInput,
    pub(crate) options: A,
}

/// Why a batch job failed.
#[derive(Debug, thiserror::Error)]
pub enum BatchError {
    #[error(transparent)]
    Configure(#[from] crate::config::ConfigureError),
    #[error(transparent)]
    Launch(#[from] LaunchError),
    #[error(transparent)]
    Shims(#[from] ArtifactError),
    #[error(transparent)]
    Signals(#[from] Unwatched),
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
    ModelGone(skyhook::provider::model::ModelRef),
    #[error("Interrupted by {0}")]
    Signal(&'static str),
    #[error("{0}; shutdown failed: {1}")]
    Shutdown(Box<Self>, HarnessError),
}

/// Run the batch job as `product`'s, its UI state in `state_file`, announcing its session id
/// on `announce` before any input is read. What startup warns about the configuration goes to
/// `warn`, before any refusal.
pub async fn run<P: Product>(
    product: &P,
    request: BatchRequest<impl ProductArgs<P::Options>>,
    state_file: &str,
    announce: &mut impl Write,
    warn: &mut impl Write,
) -> Result<(), BatchError> {
    let BatchRequest {
        config,
        execution: request,
        permissions,
        input,
        options,
    } = request;
    let catalog = crate::shims::catalog()?;
    let attendance = Attendance::Batch;
    let configured = crate::config::configure(product, options, &config, &request, attendance);
    let (host, warnings) = configured.await?;
    let name = host.identity.host.clone();
    // Model memory is shared with terminal launches.
    let (prefs, state_warning) = Prefs::load(host.workspace(), state_file);
    let saved = prefs.saved.borrow().clone();
    let remembered = (saved.model.as_ref(), saved.mode.as_ref());
    let launched = startup::prepare(host, &request, &permissions, remembered, catalog);
    for warning in warnings {
        let warning = skyhook_tools::diagnostic::escape_controls(warning);
        let _ = writeln!(warn, "{name} config: {warning}");
    }
    let launch = launched?;
    let mut signals = Signals::watch()?;
    // A named mode also applies to a resumed session, from this prompt on.
    let mode = match &permissions {
        PermissionRequest::Mode(Some(mode)) => Some(mode.clone()),
        _ => None,
    };
    let opening = request.resume.map_or(Opening::New(None), Opening::Resume);
    let opened = launch.create(opening, &Interface::Batch).await?;
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
                .opened(session, launch.context(opening, Attendance::Batch))
                .await?;
            // A resumed session switches to a named model before anything else runs.
            if let (Some(_), Some(model)) = (request.resume, &request.model) {
                session.activate_on(model).await?;
            }
            let state_warning = state_warning.as_ref().map(ToString::to_string);
            for warning in session.warnings().iter().chain(state_warning.iter()) {
                session
                    .record_status(
                        session.root_agent().clone(),
                        format!("Startup warning: {warning}"),
                    )
                    .await?;
            }
            let model = launch.model().name();
            if request.resume.is_none() && saved.model.as_ref() != Some(&model) {
                prefs.saved.borrow_mut().model = Some(model);
                let (tx, written) = tokio::sync::oneshot::channel();
                prefs.save(move |saved| drop(tx.send(saved.map_err(ToString::to_string))));
                if let Ok(Err(error)) = written.await {
                    session
                        .record_status(
                            session.root_agent().clone(),
                            format!("Could not save model selection: {error}"),
                        )
                        .await?;
                }
            }
            run_input(session, launch.workspace(), input, mode).await
        };
        tokio::select! {
            result = operation => result.map_err(|error| match error {
                OperationError::Harness(HarnessError::UnknownModel(model))
                    if request.resume.is_some() => Failure::ModelGone(model),
                error => error.into(),
            }),
            signal = signals.recv() => Err(Failure::Signal(signal)),
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
    mode: Option<skyhook::config::mode::ModeName>,
) -> Result<(), OperationError> {
    match input {
        InitialInput::Script(path) => {
            let source = tokio::fs::read_to_string(path).await?;
            session.run_script(source).await?;
        }
        InitialInput::Prompt { text, images } => {
            let attachments = skyhook::attachment::read_images(workspace, &images).await?;
            let selection = session.selection(None, mode.as_ref())?;
            session
                .prompt_with_options(text, &attachments, selection)
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{Invocation, tests::invocation};
    use crate::state;
    use crate::tests::{CONFIG, Fixture, models};
    use skyhook::identity::SessionId;
    use skyhook::session::SessionStore;
    use std::assert_matches;
    use std::path::PathBuf;

    /// A workspace whose `config.yaml` its batch jobs run with.
    struct Workspace(tempfile::TempDir);

    impl Workspace {
        fn new(config: &str) -> Self {
            let workspace = Self(tempfile::tempdir().unwrap());
            workspace.configure(config);
            workspace
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.path().join(name)
        }

        fn configure(&self, config: &str) {
            self.write("config.yaml", config);
        }

        fn write(&self, name: &str, text: &str) -> String {
            let path = self.path(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path.to_str().unwrap().to_owned()
        }

        /// Run `batch` with `args`: its outcome and the session it announced.
        async fn batch(&self, args: &[&str]) -> (Result<(), BatchError>, Option<SessionId>) {
            let (result, id, _) = self.batch_as(&skyhook::product::Skyhook, args).await;
            (result, id)
        }

        /// Run `batch` for `product` with `args`: its outcome, the session it announced
        /// and what it warned of.
        async fn batch_as(
            &self,
            product: &impl skyhook::product::Product<Options = ()>,
            args: &[&str],
        ) -> (Result<(), BatchError>, Option<SessionId>, String) {
            let (root, config) = (self.path(""), self.path("config.yaml"));
            let common = ["skyhook", "batch", "--workspace", root.to_str().unwrap()];
            let config = ["--config", config.to_str().unwrap()];
            let Ok(Invocation::Batch(request)) = invocation([&common[..], &config, args].concat())
            else {
                panic!("batch invocation");
            };
            let (mut announced, mut warned) = (Vec::new(), Vec::new());
            let result = run(product, request, "state.json", &mut announced, &mut warned).await;
            let announced = String::from_utf8(announced).unwrap();
            let id = (!announced.is_empty()).then(|| {
                let id = announced.strip_suffix('\n').expect("the id ends its line");
                id.parse().expect("only the session id is announced")
            });
            (result, id, String::from_utf8(warned).unwrap())
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
        let workspace = Workspace::new(&models(&["second"]));
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
        let (saved, _) = state::load(&workspace.path(".skyhook/state.json"));
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
        let workspace = Workspace::new(&models(&["second"]));
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
        assert_matches!(
            result,
            Err(BatchError::Failed(Failure::Operation(OperationError::Io(error))))
                if error.kind() == std::io::ErrorKind::NotFound
        );
        assert_eq!(workspace.journal(id.parse().unwrap()).await, journal);

        workspace.configure(CONFIG);
        let (result, _) = workspace.batch(&["--resume", &id, "-s", &thrown]).await;
        assert_matches!(result, Err(BatchError::Failed(Failure::ModelGone(_))));
    }

    /// What the configuration's loader and the product warn of goes to stderr, escaped,
    /// before any refusal, a configuration that fails admission included, leaving stdout to
    /// the session id.
    #[tokio::test]
    async fn configuration_warnings_go_to_stderr_before_any_refusal() {
        let workspace = Workspace::new(CONFIG);
        let script = workspace.write("run.js", "return 1;");
        let product = Fixture {
            warnings: vec!["product.yaml: fixture warning\u{1b}[31m".into()],
            ..Fixture::default()
        };
        let (result, _, warned) = workspace.batch_as(&product, &["-s", &script]).await;
        result.unwrap();
        let expected = "skyhook config: product.yaml: fixture warning\\u{1b}[31m\n";
        assert_eq!(warned, expected);
        let refused = ["-m", "test/removed", "-s", &script];
        let (result, id, warned) = workspace.batch_as(&product, &refused).await;
        assert!(result.is_err() && id.is_none());
        assert_eq!(warned, expected);
        workspace.configure(&format!("{CONFIG}default_mode: missing\n"));
        let (result, id, warned) = workspace.batch_as(&product, &["-s", &script]).await;
        let error = result.unwrap_err().to_string();
        assert!(error.contains("default_mode") && id.is_none(), "{error}");
        assert_eq!(warned, expected);
    }
}
