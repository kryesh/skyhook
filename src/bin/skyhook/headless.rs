//! A single root operation with journal-only diagnostics and deterministic cleanup.
use super::{
    Args,
    launch::{self, Launch},
};
use skyhook::agent::SessionHandle;
use std::{
    io::{self, Write},
    sync::Arc,
};

pub async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let config = Arc::new(launch::load_config(&args).await?);
    // Model memory is shared with terminal launches, but UI settings are never read.
    let (saved, state_warning) = super::tui::state::load();
    let model = launch::select_model(&config, args.model.as_deref(), saved.model.as_deref())?;
    let launch = Launch::from_args(&args, config, model, None).await?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let session = launch.create(args.resume).await?;

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
            if args.resume.is_none()
                && saved.model.as_deref() != Some(&launch.model)
                && let Err(error) = super::tui::state::remember(&launch.model)
            {
                session
                    .record_status(
                        session.root_agent().clone(),
                        format!("Could not save model selection: {error}"),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
            }
            run_input(&session, &args).await
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

async fn run_input(session: &SessionHandle, args: &Args) -> Result<(), String> {
    if let Some(path) = &args.script {
        let source = tokio::fs::read_to_string(path)
            .await
            .map_err(|error| error.to_string())?;
        session
            .run_script(source)
            .await
            .map_err(|error| error.to_string())?;
    } else if let Some(prompt) = &args.prompt {
        session
            .prompt_with_images(prompt, &args.images)
            .await
            .map_err(|error| error.to_string())?;
    } else {
        return Err("Headless execution requires --prompt or --script".into());
    }
    Ok(())
}
