//! A single root operation with journal-only diagnostics and deterministic cleanup.
use super::{
    cli::{ExecutionRequest, InitialInput},
    launch::{self, Launch},
};
use skyhook::agent::SessionHandle;
use std::io::{self, Write};

pub async fn run(
    request: ExecutionRequest,
    input: InitialInput,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = launch::load_config(&request.config, false).await?;
    // Model memory is shared with terminal launches, but UI settings are never read.
    let (saved, state_warning) = super::tui::state::load(&request.config.workspace);
    let model = launch::select_model(&config, request.model.as_deref(), saved.model.as_deref())?;
    let launch = Launch::from_request(&request.config, model, None).await?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
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
                && let Err(error) =
                    super::tui::state::remember(&launch.workspace, launch.model.name())
            {
                session
                    .record_status(
                        session.root_agent().clone(),
                        format!("Could not save model selection: {error}"),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
            }
            run_input(&session, &launch.workspace, input).await
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
            session
                .prompt_with_options(text, &attachments, skyhook::agent::PromptOptions::default())
                .await
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}
