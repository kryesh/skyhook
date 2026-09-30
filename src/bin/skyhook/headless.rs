//! A single root operation with journaled diagnostics and deterministic cleanup.
use super::{
    cli::{BatchRequest, InitialInput, PermissionArgs},
    launch::{self, Launch, LaunchError, OperationError, Permissions},
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
}

#[derive(Debug, thiserror::Error)]
pub enum Failure {
    #[error(transparent)]
    Operation(#[from] OperationError),
    #[error("Interrupted by {0}")]
    Signal(&'static str),
    #[error("{0}; shutdown failed: {1}")]
    Shutdown(Box<Self>, HarnessError),
}

pub async fn run(request: BatchRequest, input: InitialInput) -> Result<(), BatchError> {
    let BatchRequest {
        execution: request,
        permissions,
    } = request;
    let config = launch::load_config(&request.config, false)
        .await
        .map_err(LaunchError::from)?;
    // Model memory is shared with terminal launches, but UI settings are never read.
    let (saved, state_warning) = super::state::load(&request.config.source.workspace);
    let model = launch::select_model(&config, request.model.as_ref(), saved.model.as_ref())?;
    // A named mode also applies to a resumed session, from this prompt on.
    let mode = match &permissions {
        PermissionArgs::Mode(Some(mode)) => Some(mode.clone()),
        _ => None,
    };
    let permissions = Permissions::for_batch(&permissions, request.resume.is_some(), &config)
        .map_err(LaunchError::from)?;
    let launch = Launch::from_request(&request, model, permissions, None).await?;
    let watch = |kind| signal(kind).map_err(BatchError::Signals);
    let mut terminate = watch(SignalKind::terminate())?;
    let mut hangup = watch(SignalKind::hangup())?;
    let mut interrupt = watch(SignalKind::interrupt())?;
    let session = launch.create(request.resume).await?;

    // This must be the first action after open, before reading a workflow/image or
    // contacting a provider. Flush explicitly so pipe consumers can follow events.
    let announced = {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{}", session.id()).and_then(|()| stdout.flush())
    };
    let outcome = if let Err(error) = announced {
        Err(OperationError::from(error).into())
    } else {
        let operation = async {
            for warning in session
                .warnings()
                .iter()
                .chain(session.startup_warnings())
                .chain(state_warning.iter())
            {
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
                && let Err(error) = super::state::update(&launch.workspace, |state| {
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
            run_input(&session, &launch.workspace, input, mode).await
        };
        tokio::select! {
            result = operation => result.map_err(Failure::from),
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
        (Err(error), Err(cleanup)) => Err(Failure::Shutdown(Box::new(error), cleanup)),
    };
    let status = match &outcome {
        Ok(()) => "Completed".to_owned(),
        Err(error) => format!("Failed: {error}"),
    };
    session
        .record_status(session.root_agent().clone(), status)
        .await?;
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
            let attachments = launch::read_images(workspace, &images)
                .await
                .map_err(OperationError::Attachment)?;
            let selection = session.selection(None, mode.as_ref())?;
            session
                .prompt_with_options(text, &attachments, selection)
                .await?;
        }
    }
    Ok(())
}
