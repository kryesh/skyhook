mod app;
mod composer;
mod editor;
pub(crate) mod format;
mod frames;
mod host;
mod keys;
mod model;
mod render;
pub mod state;
mod status;
mod theme;
mod tool_view;

use super::cli::{InitialInput, InteractiveRequest};
use app::{App, PreparedObservation};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream,
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
    task::Poll,
    time::Duration,
};
use tokio::{sync::mpsc, time::Instant};

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
    request: InteractiveRequest,
    initial_input: Option<InitialInput>,
) -> Result<(), Box<dyn std::error::Error>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("Skyhook requires an interactive terminal. Run skyhook in a terminal; redirected input/output is not supported.".into());
    }
    let InteractiveRequest {
        execution: request,
        mode: explicit,
    } = request;
    let config = super::launch::load_config(&request.config, true).await?;
    let (saved, warning) = state::load(&request.config.workspace);
    let model =
        super::launch::select_model(&config, request.model.as_deref(), saved.model.as_deref())?;
    // Like the model: an explicit mode, then the last one used, then the default. A
    // resumed session may know an explicit mode the configuration no longer has.
    let configured = match &explicit {
        Some(mode) if request.resume.is_some() => config.select_mode(Some(mode)).ok(),
        Some(mode) => Some(config.select_mode(Some(mode))?),
        None => saved
            .mode
            .as_deref()
            .and_then(|mode| config.select_mode(Some(mode)).ok()),
    };
    let mode = configured
        .unwrap_or_else(|| config.default_mode())
        .to_owned();
    let permissions = super::launch::Permissions::Mode(mode.clone());
    let launch = Launch::from_request(&request, model, permissions, None).await?;
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
    let mut app = App::new(observation, launch, mode, saved, tx);
    // An explicit mode outranks the one a resumed session was last in, if it has it.
    if let Some(mode) = explicit {
        if app.modes().contains_key(&mode) {
            app.mode = mode;
        } else {
            app.notice(format!("Unknown mode: {mode}"));
        }
    }
    if let Some(warning) = warning {
        app.notice(warning);
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        TerminalGuard::restore();
        previous(info);
    }));
    let _guard = TerminalGuard::enter()?;
    let (output, mut writer) = frames::Writer::spawn(io::stdout());
    let mut terminal = ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(output))?;
    terminal.clear()?;
    let mut input = EventStream::new();
    let mut ticks = tokio::time::interval(Duration::from_millis(100));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pacer = frames::Pacer::default();
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
            let now = Instant::now();
            if app.dirty && !writer.busy() && pacer.due().is_none_or(|due| due <= now) {
                draw_terminal(&mut terminal, app)?;
                pacer.drawn(now, now.elapsed());
                app.dirty = false;
            }
            if let Some(text) = app.clipboard.take() {
                copy_terminal(terminal.backend_mut(), &text)?;
            }
            writer.send()?;
            // A frame held back by the writer waits for it; otherwise for the budget.
            let busy = writer.busy();
            let budget = pacer.due().filter(|_| app.dirty && !busy);
            tokio::select! {
                event = input.next() => match event {
                    Some(Ok(event)) => {
                        apply(&mut host, &mut pacer, event);
                        drain(&mut input, &mut host, &mut pacer).await?;
                    }
                    Some(Err(error)) => return Err(error),
                    None => break,
                },
                event = host.next() => host.handle(event).await,
                _ = ticks.tick() => host.tick(),
                written = writer.written(), if busy => written?,
                _ = tokio::time::sleep_until(budget.unwrap_or(now)), if budget.is_some() => {},
                _ = terminate.recv() => host.quit(),
                _ = hangup.recv() => host.quit(),
                _ = interrupt.recv() => host.quit(),
            }
        }
        Ok(())
    }
    .await;
    // Everything drawn reaches the terminal before it is restored.
    drop(terminal);
    let written = writer.join();
    host.close().await?;
    result?;
    written?;
    Ok(())
}

/// Input continuing a gesture (pointer motion, drags, wheel steps) repaints within
/// the frame budget; anything else paints as soon as the writer is free.
fn apply(host: &mut host::Host, pacer: &mut frames::Pacer, event: Event) {
    use crossterm::event::MouseEventKind as Kind;
    let gesture = matches!(&event, Event::Mouse(mouse) if matches!(
        mouse.kind,
        Kind::Moved
            | Kind::Drag(_)
            | Kind::ScrollUp
            | Kind::ScrollDown
            | Kind::ScrollLeft
            | Kind::ScrollRight
    ));
    if !gesture {
        pacer.input();
    }
    host.app().event(event);
}

/// Apply every input event already queued, so one frame answers a burst: a window
/// drag relays out once, for its latest size. A host request is settled first.
async fn drain(
    input: &mut EventStream,
    host: &mut host::Host,
    pacer: &mut frames::Pacer,
) -> io::Result<()> {
    while host.app().host.is_none() && !host.app().exit {
        // Poll with this task's waker: crossterm keeps the first waker it is given
        // until input arrives, so a no-op waker would strand later input.
        let next = std::future::poll_fn(|cx| Poll::Ready(input.poll_next_unpin(cx))).await;
        match next {
            Poll::Ready(Some(Ok(event))) => apply(host, pacer, event),
            Poll::Ready(Some(Err(error))) => return Err(error),
            Poll::Ready(None) | Poll::Pending => break,
        }
    }
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

fn copy_terminal(output: &mut impl Write, text: &str) -> io::Result<()> {
    use base64::Engine as _;
    write!(
        output,
        "\x1b]52;c;{}\x07",
        base64::engine::general_purpose::STANDARD.encode(text)
    )
}
