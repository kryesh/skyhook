//! A single root operation with journaled diagnostics and deterministic cleanup.
use super::{
    cli::{BatchRequest, InitialInput, PermissionArgs},
    launch::{self, Launch, Permissions},
};
use skyhook::agent::SessionHandle;
use std::io::{self, Write};

pub async fn run(
    request: BatchRequest,
    input: InitialInput,
) -> Result<(), Box<dyn std::error::Error>> {
    let BatchRequest {
        execution: request,
        permissions,
    } = request;
    let config = launch::load_config(&request.config, false).await?;
    // Model memory is shared with terminal launches, but UI settings are never read.
    let (saved, state_warning) = super::tui::state::load(&request.config.workspace);
    let model = launch::select_model(&config, request.model.as_deref(), saved.model.as_deref())?;
    // A named mode also applies to a resumed session, from this prompt on.
    let mode = match &permissions {
        PermissionArgs::Mode(Some(mode)) => Some(mode.clone()),
        _ => None,
    };
    let permissions = Permissions::for_batch(&permissions, request.resume.is_some(), &config)?;
    let launch = Launch::from_request(&request, model, permissions, None).await?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    if request.resume.is_some() && mode.is_some() && matches!(input, InitialInput::Script(_)) {
        return Err(
            "--mode changes a resumed session with its next prompt; a script has none".into(),
        );
    }
    let session = launch.create(request.resume).await?;

    // This must be the first action after open, before reading a workflow/image or
    // contacting a provider. Flush explicitly so pipe consumers can follow events.
    let announced = {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{}", session.id()).and_then(|()| stdout.flush())
    };
    let outcome = if let Err(error) = announced {
        Err(error.to_string())
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
                    .await
                    .map_err(|error| error.to_string())?;
            }
            if request.resume.is_none()
                && saved.model.as_deref() != Some(launch.model.name())
                && let Err(error) = super::tui::state::update(&launch.workspace, |state| {
                    state.model = Some(launch.model.name().to_owned());
                })
            {
                session
                    .record_status(
                        session.root_agent().clone(),
                        format!("Could not save model selection: {error}"),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
            }
            run_input(&session, &launch.workspace, input, mode).await
        };
        tokio::select! {
            result = operation => result,
            _ = terminate.recv() => Err("Interrupted by SIGTERM".into()),
            _ = hangup.recv() => Err("Interrupted by SIGHUP".into()),
            _ = interrupt.recv() => Err("Interrupted by SIGINT".into()),
        }
    };
    // Root completion is not session quiescence: cancel/drain background tools
    // and child agents, including after interruption or input preparation errors.
    let outcome = match (outcome, session.shutdown().await) {
        (result, Ok(())) => result,
        (Ok(()), Err(error)) => Err(error.to_string()),
        (Err(error), Err(cleanup)) => Err(format!("{error}; shutdown failed: {cleanup}")),
    };
    let status = match &outcome {
        Ok(()) => "Completed".to_owned(),
        Err(error) => format!("Failed: {error}"),
    };
    session
        .record_status(session.root_agent().clone(), status)
        .await?;
    outcome.map_err(Into::into)
}

async fn run_input(
    session: &SessionHandle,
    workspace: &std::path::Path,
    input: InitialInput,
    mode: Option<String>,
) -> Result<(), String> {
    match input {
        InitialInput::Script(path) => {
            let source = tokio::fs::read_to_string(path)
                .await
                .map_err(|error| error.to_string())?;
            session
                .run_script(source)
                .await
                .map_err(|error| error.to_string())?;
        }
        InitialInput::Prompt { text, images } => {
            let attachments = launch::read_images(workspace, &images).await?;
            let selection = session
                .selection(None, mode.as_deref())
                .map_err(|error| error.to_string())?;
            session
                .prompt_with_options(text, &attachments, selection)
                .await
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}
