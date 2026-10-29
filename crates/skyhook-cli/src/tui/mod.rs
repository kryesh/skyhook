mod app;
mod composer;
mod editor;
pub(crate) mod format;
mod frames;
mod host;
mod keys;
mod model;
mod render;
mod status;
mod theme;
pub use theme::{Color, Palette, Rgb, SyntaxPalette, Theme};
mod tool_view;
mod viewport;

use super::{
    Identity, InitialInput, Opening, RootMode, args::InteractiveRequest, launch::ConfiguredHost,
};
use app::App;
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

/// The period of toasts, prompt cleanup and animation.
const TICK: Duration = Duration::from_millis(100);

/// The terminal while the TUI owns it. Every exit path (the end of `run`, an error
/// unwinding it, the panic hook) restores it through [`restore`](Self::restore),
/// which runs once whichever comes first.
pub struct TerminalGuard;

/// Whether the terminal still needs restoring.
static ENTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        let guard = Self;
        ENTERED.store(true, std::sync::atomic::Ordering::SeqCst);
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        Ok(guard)
    }

    /// Hand the terminal back. Reporting stops first, then input already queued
    /// (mouse reports the terminal sent meanwhile) is discarded, so none of it
    /// reaches the shell once raw mode ends.
    fn restore() {
        if !ENTERED.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            EndSynchronizedUpdate,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        discard_input();
        let _ = disable_raw_mode();
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        Self::restore();
    }
}

/// Drop input the terminal sent that nothing read yet.
fn discard_input() {
    // SAFETY: tcflush takes a descriptor and a queue selector and touches no memory.
    unsafe {
        libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
    }
}

pub(super) fn check_terminal(identity: &Identity) -> Result<(), super::Error> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(format!("{} requires an interactive terminal. Run {} in a terminal; redirected input/output is not supported.", identity.display_name, identity.name()).into());
    }
    Ok(())
}

pub async fn run(
    request: InteractiveRequest,
    initial_input: Option<InitialInput>,
    host: ConfiguredHost,
) -> Result<(), Box<dyn std::error::Error>> {
    let InteractiveRequest {
        execution: request,
        mode: explicit,
    } = request;
    theme::install(host.identity.theme);
    let config = &host.config;
    super::launch::admit_root_mode(&host.identity, config, explicit.is_some())?;
    let (saved, warning) = crate::state::load(&host.state);
    let model = super::launch::select_model(config, request.model.as_ref(), saved.model.as_ref())?;
    let mode = initial_mode(
        config,
        explicit.as_ref(),
        saved.mode.as_ref(),
        request.resume.map_or(Opening::New(None), Opening::Resume),
        host.identity.root_mode,
    )?;
    let permissions = super::launch::Permissions::Mode(mode.clone());
    let launch = Launch::new(model, permissions, None, host)?;
    let (launch, prompts) = host::with_prompts(launch);
    let observation = match request.resume {
        Some(id) => {
            let opening = crate::Opening::Resume(id);
            let session = launch.create(opening).await?;
            Some(host::observed(session, &launch, opening, request.model.as_ref()).await?)
        }
        None => None,
    };
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App::new(observation, launch, &mode, saved, tx);
    // An explicit mode outranks the one a resumed session was last in, if the
    // session has it.
    if let Some(mode) = explicit
        && !app.select_mode(&mode)
    {
        app.local_notice(format!("Unknown mode: {mode}"));
    }
    if let Some(warning) = warning {
        app.local_notice(warning);
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        TerminalGuard::restore();
        previous(info);
    }));
    let guard = TerminalGuard::enter()?;
    let (output, mut writer) = frames::Writer::spawn(io::stdout());
    let mut terminal = ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(output))?;
    terminal.clear()?;
    let mut input = EventStream::new();
    let mut ticks = tokio::time::interval(TICK);
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
        // A menu an event opened is drawn before the next event: its rows are what
        // it pages by.
        let mut opened = false;
        // Quitting ends interaction at once; sessions stop after the terminal is back.
        while host.settle() && !host.quitting() {
            let app = host.app();
            let now = Instant::now();
            if app.dirty && !writer.busy() && pacer.due().is_none_or(|due| due <= now) {
                draw_terminal(&mut terminal, app)?;
                pacer.drawn(now, now.elapsed());
                app.dirty = false;
                opened = false;
            }
            if let Some(text) = app.clipboard.take() {
                copy_terminal(terminal.backend_mut(), &text)?;
            }
            writer.send()?;
            // A frame held back by the writer waits for it; otherwise for the budget.
            let busy = writer.busy();
            let budget = pacer.due().filter(|_| app.dirty && !busy);
            tokio::select! {
                event = input.next(), if !opened => match event {
                    Some(Ok(event)) => {
                        opened = apply(&mut host, &mut pacer, event)
                            || drain(&mut input, &mut host, &mut pacer).await?;
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
    // Everything drawn reaches the terminal before it is restored, and it is restored
    // before sessions stop and close, which can take a while: nothing the terminal
    // reports meanwhile is left for the shell.
    drop(terminal);
    let written = writer.join();
    drop(input);
    drop(guard);
    if host.quitting() {
        host.wind_down().await;
    }
    let closed = host.close().await;
    discard_input();
    closed?;
    result?;
    written?;
    Ok(())
}

fn initial_mode(
    config: &skyhook::config::RuntimeConfig,
    explicit: Option<&skyhook::tool::policy::ModeName>,
    remembered: Option<&skyhook::tool::policy::ModeName>,
    opening: Opening<'_>,
    root_mode: RootMode,
) -> Result<skyhook::tool::policy::ModeName, skyhook::config::ConfigError> {
    let configured = match (explicit, opening) {
        // A viewed session may still know a mode no longer present in its configuration.
        (Some(mode), Opening::Resume(_)) => config.select_mode(Some(mode)).ok(),
        (Some(mode), Opening::New(_)) => Some(config.select_mode(Some(mode))?),
        (None, _) => match root_mode {
            RootMode::Switchable => remembered.and_then(|mode| config.select_mode(Some(mode)).ok()),
            RootMode::Fixed => None,
        },
    };
    Ok(configured.unwrap_or_else(|| config.default_mode()).clone())
}

/// Input continuing a gesture (pointer motion, drags, wheel steps) repaints within
/// the frame budget; anything else paints as soon as the writer is free. Whether
/// the event opened a menu.
fn apply(host: &mut host::Host, pacer: &mut frames::Pacer, event: Event) -> bool {
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
    let menu = |host: &mut host::Host| host.app().menu.as_ref().map(|menu| menu.id);
    let shown = menu(host);
    host.app().event(event);
    menu(host).is_some_and(|opened| shown != Some(opened))
}

/// Apply every input event already queued, so one frame answers a burst: a window
/// drag relays out once, for its latest size. A host request is settled first, and
/// a menu an event opened is drawn first: whether one was.
async fn drain(
    input: &mut EventStream,
    host: &mut host::Host,
    pacer: &mut frames::Pacer,
) -> io::Result<bool> {
    while host.app().host.is_none() && !host.app().exit {
        // Poll with this task's waker: crossterm keeps the first waker it is given
        // until input arrives, so a no-op waker would strand later input.
        let next = std::future::poll_fn(|cx| Poll::Ready(input.poll_next_unpin(cx))).await;
        match next {
            Poll::Ready(Some(Ok(event))) => {
                if apply(host, pacer, event) {
                    return Ok(true);
                }
            }
            Poll::Ready(Some(Err(error))) => return Err(error),
            Poll::Ready(None) | Poll::Pending => break,
        }
    }
    Ok(false)
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

#[cfg(test)]
mod tests {
    use super::*;
    use skyhook::tool::policy::ModeName;

    /// Without an explicit mode, the remembered one applies only to a switchable root.
    #[test]
    fn a_fixed_root_ignores_the_remembered_mode() {
        let mut config = crate::interaction::tests::test_config();
        let live: ModeName = "live".parse().unwrap();
        config
            .modes
            .insert(live.clone(), config.modes["general"].clone());
        let config = config.into_runtime().unwrap();
        let select = |preference| {
            initial_mode(&config, None, Some(&live), Opening::New(None), preference).unwrap()
        };
        assert_eq!(select(RootMode::Switchable), live);
        assert_eq!(select(RootMode::Fixed), *config.default_mode());
    }
}
