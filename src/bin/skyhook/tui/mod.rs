mod app;
mod composer;
mod editor;
mod format;
mod keys;
mod model;
mod render;
pub mod state;
mod status;
mod theme;
mod tool_view;

use super::{
    cli::{ExecutionRequest, InitialInput},
    interaction::UiInteraction,
};
use app::{App, PreparedObservation, Work};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        EventStream,
    },
    execute, queue,
    terminal::{
        BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use futures_util::StreamExt;
use std::{
    io::{self, IsTerminal, Write},
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;

pub(crate) use super::launch::Launch;

pub struct TerminalGuard;
impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        let guard = Self;
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        Ok(guard)
    }
    fn restore() {
        let _ = execute!(
            io::stdout(),
            EndSynchronizedUpdate,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        let _ = disable_raw_mode();
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        Self::restore();
    }
}

pub async fn run(
    request: ExecutionRequest,
    initial_input: Option<InitialInput>,
) -> Result<(), Box<dyn std::error::Error>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("Skyhook requires an interactive terminal. Run skyhook in a terminal; redirected input/output is not supported.".into());
    }
    let config = super::launch::load_config(&request.config, true).await?;
    let (saved, warning) = state::load(&request.config.workspace);
    let model =
        super::launch::select_model(&config, request.model.as_deref(), saved.model.as_deref())?;
    let (interaction, mut prompts) = UiInteraction::new();
    let launch = Launch::from_request(&request.config, model, Some(Arc::new(interaction))).await?;
    let session = match request.resume {
        Some(id) => Some(launch.create(Some(id)).await?),
        None => None,
    };
    let observation = match session {
        Some(session) => Some(PreparedObservation::subscribe(session).await),
        None => None,
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(observation, launch, saved, tx.clone());
    if let Some(warning) = warning {
        app.notice(warning);
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        TerminalGuard::restore();
        previous(info);
    }));
    let _guard = TerminalGuard::enter()?;
    let mut terminal = ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(
        io::BufWriter::with_capacity(64 * 1024, io::stdout()),
    ))?;
    terminal.clear()?;
    let mut input = EventStream::new();
    let mut ticks = tokio::time::interval(Duration::from_millis(100));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Clicks and typing paint immediately. Coalesce continuous mouse/background
    // bursts to avoid flooding the terminal; this is NOT a CPU rendering budget.
    let frame_interval = Duration::from_millis(4);
    let mut last_draw = tokio::time::Instant::now() - frame_interval;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    match initial_input {
        Some(InitialInput::Script(path)) => app.start_script(path),
        Some(InitialInput::Prompt { text, images }) => app.start_prompt(text, images).await,
        None => {}
    }
    let result: io::Result<()> = async {
        loop {
            let mut immediate = false;
            tokio::select! {
                event = input.next() => match event {
                    Some(Ok(event)) => {
                        immediate = !matches!(&event, crossterm::event::Event::Mouse(mouse) if matches!(mouse.kind, crossterm::event::MouseEventKind::Moved | crossterm::event::MouseEventKind::Drag(_) | crossterm::event::MouseEventKind::ScrollUp | crossterm::event::MouseEventKind::ScrollDown | crossterm::event::MouseEventKind::ScrollLeft | crossterm::event::MouseEventKind::ScrollRight));
                        app.event(event);
                    },
                    Some(Err(error)) => return Err(error),
                    None => break,
                },
                event = app.recv_observation() => match event {
                    Ok(event) => {
                        let mut records = app.observe(event);
                        // Reduce a burst once instead of rebuilding the projection per token.
                        for _ in 0..255 {
                            match app.try_recv_observation() {
                                Ok(event) => records |= app.observe(event),
                                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                                    app.resubscribe().await;
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                        if records { app.projection.rebuild(&app.snapshot); }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        app.resubscribe().await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => app.close_observation(),
                },
                Some(prompt) = prompts.recv() => app.prompt(prompt),
                Some(work) = rx.recv() => {
                    match work {
                        Work::SessionReady { result: Ok(session) } => {
                            app.session_ready(session, false).await;
                        }
                        Work::Started { result: Ok(session) } => {
                            app.session_ready(Some(session), true).await;
                        }
                        work => app.work(work),
                    }
                }
                _ = ticks.tick() => app.tick(),
                _ = tokio::time::sleep_until(last_draw + frame_interval), if app.dirty => {},
                _ = terminate.recv() => app.shutdown(),
                _ = hangup.recv() => app.shutdown(),
                _ = interrupt.recv() => app.shutdown(),
            }
            if app.dirty && (immediate || last_draw.elapsed() >= frame_interval) {
                last_draw = tokio::time::Instant::now();
                draw_terminal(&mut terminal, &mut app)?;
                app.dirty = false;
            }
            if let Some(text) = app.clipboard.take() {
                copy_terminal(&text)?;
            }
            if app.exit {
                break;
            }
        }
        Ok(())
    }
    .await;
    // Terminal EOF/errors can leave a creation or switch result queued. Closing
    // first also makes later results shut their handles down in the worker.
    rx.close();
    while let Ok(work) = rx.try_recv() {
        match work {
            Work::Started {
                result: Ok(session),
                ..
            }
            | Work::SessionReady {
                result: Ok(Some(session)),
                ..
            } => {
                let _ = session.shutdown().await;
            }
            _ => {}
        }
    }
    app.status.flush().await;
    if let Some(session) = app.session() {
        session.shutdown().await?;
    }
    result?;
    Ok(())
}

/// Buffer each frame and present it atomically on terminals supporting synchronized output.
fn draw_terminal<W: Write>(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<W>>,
    app: &mut App,
) -> io::Result<()> {
    queue!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
    let rendered = terminal.draw(|frame| render::draw(frame, app)).map(|_| ());
    // End synchronization even if rendering fails, so the terminal is never left frozen.
    let ended = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
    rendered.and(ended)
}

fn copy_terminal(text: &str) -> io::Result<()> {
    use base64::Engine as _;
    write!(
        io::stdout(),
        "\x1b]52;c;{}\x07",
        base64::engine::general_purpose::STANDARD.encode(text)
    )?;
    io::stdout().flush()
}
