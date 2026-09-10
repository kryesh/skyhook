mod app;
mod editor;
mod format;
mod keys;
mod model;
mod render;
pub mod state;
mod status;
mod tool_view;

use super::{Args, interaction::UiInteraction};
use app::{App, Work};
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
        Self::activate()?;
        Ok(guard)
    }
    fn activate() -> io::Result<()> {
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )
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

pub async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("Skyhook requires an interactive terminal. Run skyhook in a terminal; redirected input/output is not supported.".into());
    }
    let config = Arc::new(super::launch::load_config(&args).await?);
    let (saved, warning) = state::load();
    let model =
        super::launch::select_model(&config, args.model.as_deref(), saved.model.as_deref())?;
    let settings = state::settings()?;
    let keymap = keys::KeyMap::new(&settings.keybinds)?;
    if !matches!(settings.theme.as_str(), "dark" | "light") {
        return Err("tui.toml theme must be dark or light".into());
    }
    let (interaction, mut prompts) = UiInteraction::new();
    let launch = Launch::from_args(&args, config, model, Some(Arc::new(interaction))).await?;
    let session = match args.resume {
        Some(id) => Some(launch.create(Some(id)).await?),
        None => None,
    };
    let (snapshot, mut updates) = if let Some(session) = &session {
        let observation = session.observe().await;
        (observation.snapshot, Some(observation.updates))
    } else {
        (Default::default(), None)
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(
        session,
        launch,
        snapshot,
        saved.model,
        tx.clone(),
        keymap,
        saved.theme.as_deref().unwrap_or(&settings.theme) == "light",
    );
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
    let frame_interval = Duration::from_millis(16);
    let mut last_draw = tokio::time::Instant::now() - frame_interval;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    if let Some(path) = args.script {
        app.start_script(path);
    } else if let Some(prompt) = args.prompt {
        app.submit(prompt, args.images);
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
                event = async {
                    match &mut updates {
                        Some(updates) => updates.recv().await,
                        None => std::future::pending().await,
                    }
                } => match event {
                    Ok(event) => {
                        let mut records = app.observe(event);
                        // Reduce a burst once instead of rebuilding the projection per token.
                        for _ in 0..255 {
                            match updates.as_mut().expect("active observation").try_recv() {
                                Ok(event) => records |= app.observe(event),
                                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                                    let observation = app.session.as_ref().expect("observed session").observe().await;
                                    app.snapshot = observation.snapshot;
                                    updates = Some(observation.updates);
                                    app.reset_projection();
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                        if records { app.projection.rebuild(&app.snapshot); }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let observation = app.session.as_ref().expect("observed session").observe().await;
                        app.snapshot = observation.snapshot;
                        updates = Some(observation.updates);
                        app.reset_projection();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => updates = None,
                },
                Some(prompt) = prompts.recv() => app.prompt(prompt),
                Some(work) = rx.recv() => {
                    match work {
                        Work::SessionReady(Ok(session)) => {
                            let snapshot = if let Some(session) = &session {
                                let observation = session.observe().await;
                                updates = Some(observation.updates);
                                observation.snapshot
                            } else {
                                updates = None;
                                Default::default()
                            };
                            app.set_session(session, snapshot);
                        }
                        Work::Started(Ok(session)) => {
                            let observation = session.observe().await;
                            updates = Some(observation.updates);
                            app.session_started(session, observation.snapshot);
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
            if app.external_editor {
                app.external_editor = false;
                drop(input);
                TerminalGuard::restore();
                let edited = edit_external(app.editor.text.clone()).await;
                TerminalGuard::activate()?;
                terminal.clear()?;
                input = EventStream::new();
                match edited {
                    Ok(text) => app.editor.set(text),
                    Err(error) => app.notice(error.to_string()),
                }
                app.dirty = true;
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
            Work::Started(Ok(session)) | Work::SessionReady(Ok(Some(session))) => {
                let _ = session.shutdown().await;
            }
            _ => {}
        }
    }
    app.status.flush().await;
    if let Some(session) = &app.session {
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

async fn edit_external(text: String) -> io::Result<String> {
    let mut file = tempfile::Builder::new().suffix(".md").tempfile()?;
    file.write_all(text.as_bytes())?;
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    // The user's configured editor is shell syntax; the file path is a positional argument.
    let status = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(format!("exec {editor} \"$1\""))
        .arg("skyhook-editor")
        .arg(file.path())
        .status()
        .await?;
    if !status.success() {
        return Err(io::Error::other("external editor failed"));
    }
    tokio::fs::read_to_string(file.path()).await
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
