mod app;
mod composer;
mod editor;
pub(crate) mod format;
mod host;
mod keys;
mod model;
mod render;
pub mod state;
mod status;
mod theme;
mod tool_view;

use super::cli::{ExecutionRequest, InitialInput};
use app::{App, PreparedObservation};
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
    let launch = Launch::from_request(&request.config, model, None).await?;
    let (launch, prompts) = host::with_prompts(launch);
    let session = match request.resume {
        Some(id) => Some(launch.create(Some(id)).await?),
        None => None,
    };
    let observation = match session {
        Some(session) => Some(PreparedObservation::subscribe(session).await),
        None => None,
    };
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(observation, launch, saved, tx);
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
    let mut host = host::Host::new(app, rx, prompts);
    let result: io::Result<()> = async {
        while host.settle() {
            let app = host.app();
            if app.dirty && last_draw.elapsed() >= frame_interval {
                last_draw = tokio::time::Instant::now();
                draw_terminal(&mut terminal, app)?;
                app.dirty = false;
            }
            if let Some(text) = app.clipboard.take() {
                copy_terminal(&text)?;
            }
            let dirty = app.dirty;
            tokio::select! {
                event = input.next() => match event {
                    Some(Ok(event)) => {
                        // Clicks and typing paint at once; continuous mouse bursts coalesce.
                        if !matches!(&event, crossterm::event::Event::Mouse(mouse) if matches!(mouse.kind, crossterm::event::MouseEventKind::Moved | crossterm::event::MouseEventKind::Drag(_) | crossterm::event::MouseEventKind::ScrollUp | crossterm::event::MouseEventKind::ScrollDown | crossterm::event::MouseEventKind::ScrollLeft | crossterm::event::MouseEventKind::ScrollRight)) {
                            last_draw = tokio::time::Instant::now() - frame_interval;
                        }
                        host.app().event(event);
                    },
                    Some(Err(error)) => return Err(error),
                    None => break,
                },
                event = host.next() => host.handle(event).await,
                _ = ticks.tick() => host.tick(),
                _ = tokio::time::sleep_until(last_draw + frame_interval), if dirty => {},
                _ = terminate.recv() => host.quit(),
                _ = hangup.recv() => host.quit(),
                _ = interrupt.recv() => host.quit(),
            }
        }
        Ok(())
    }
    .await;
    host.close().await?;
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
