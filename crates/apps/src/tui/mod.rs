//! The terminal UI: one or more sessions of a product, opened through the host's launch.
mod app;
mod commands;
mod composer;
mod editor;
mod format;
mod frames;
mod markdown;
mod model;
mod render;
mod shell;
mod status;
mod theme;
pub use theme::{Color, Look, Palette, SyntaxPalette, Theme};
mod tool_view;
mod viewport;
mod wrapping;

use crate::args::{ConfigArgs, ProductArgs};
use crate::{signals::Signals, state::Prefs};
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
use skyhook::config::mode::ModeName;
use skyhook::launch::{
    request::{ExecutionRequest, InitialInput, PermissionRequest},
    startup,
};
use skyhook::product::{Attendance, Identity, Opening, Product};
use std::{
    io::{self, IsTerminal, Write},
    task::Poll,
    time::Duration,
};
use tokio::{sync::mpsc, time::Instant};

/// The period of toasts, prompt cleanup and animation.
const TICK: Duration = Duration::from_millis(100);

/// The terminal while the TUI owns it. Every exit path (the end of `run`, an error
/// unwinding it, the panic hook) restores it through [`restore`](Self::restore),
/// which runs once whichever comes first.
struct TerminalGuard;

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

/// Refuse to start without an interactive terminal on stdin and stdout.
fn check_terminal(identity: &Identity) -> Result<(), String> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(format!(
            "{} requires an interactive terminal. Run {} in a terminal; redirected input/output is not supported.",
            identity.display_name,
            identity.name()
        ));
    }
    Ok(())
}

/// A session in the terminal, with its first input and the product's options. The terminal
/// always runs in a configured mode; an explicit name may be one only the resumed session
/// still knows.
pub struct InteractiveRequest<A> {
    pub(crate) config: ConfigArgs,
    pub(crate) execution: ExecutionRequest,
    pub(crate) mode: Option<ModeName>,
    pub(crate) input: Option<InitialInput>,
    pub(crate) options: A,
}

/// Refuse a non-terminal, configure `product` for `request`, its UI state in `state_file`,
/// then run the terminal with `look` until the user quits.
pub async fn run<P: Product>(
    product: &P,
    request: InteractiveRequest<impl ProductArgs<P::Options>>,
    state_file: &str,
    look: Look,
) -> Result<(), Box<dyn std::error::Error>> {
    let InteractiveRequest {
        config,
        execution: request,
        mode: explicit,
        input: initial_input,
        options,
    } = request;
    check_terminal(&product.identity())?;
    let attendance = Attendance::Attended;
    let catalog = crate::shims::catalog()?;
    let configured = crate::config::configure(product, options, &config, &request, attendance);
    let (host, warnings) = configured.await?;
    theme::install(look);
    let name = host.identity.host.clone();
    let permissions = PermissionRequest::Mode(explicit.clone());
    let (mut prefs, unreadable) = Prefs::load(host.workspace(), state_file);
    let saved = prefs.saved.get_mut();
    let remembered = (saved.model.as_ref(), saved.mode.as_ref());
    let launched = startup::prepare(host, &request, &permissions, remembered, catalog);
    let (ui, prompts) = shell::prompts();
    let opened = async {
        let launch = launched?;
        let signals = Signals::watch()?;
        let observation = match request.resume {
            Some(id) => {
                let opening = Opening::Resume(id);
                Some(shell::open(&launch, &ui, opening, request.model.as_ref()).await?)
            }
            None => None,
        };
        Ok::<_, Box<dyn std::error::Error>>((launch, signals, observation))
    };
    let (launch, mut signals, observation) = match opened.await {
        Ok(opened) => opened,
        Err(error) => {
            // With no screen to show them on, they precede the error that ends startup.
            for warning in &warnings {
                let warning = skyhook_tools::diagnostic::escape_controls(warning);
                eprintln!("{name} config: {warning}");
            }
            return Err(error);
        }
    };
    let (tx, rx) = mpsc::unbounded_channel();
    let mode = launch.mode().cloned();
    let prefs = std::rc::Rc::new(prefs);
    let mut app = App::new(observation, launch, ui, mode.as_ref(), prefs, tx);
    // An explicit mode outranks the one a resumed session was last in, if the
    // session has it.
    if let Some(mode) = explicit
        && !app.select_mode(&mode)
    {
        app.local_notice(format!("Unknown mode: {mode}"));
    }
    app.show_startup(warnings, unreadable);
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
    match initial_input {
        Some(InitialInput::Script(path)) => app.start_script(path),
        Some(InitialInput::Prompt { text, images }) => app.start_prompt(text, images).await,
        None => {}
    }
    let mut host = shell::Host::new(app, rx, prompts);
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
                _ = signals.recv() => host.quit(),
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

/// Input continuing a gesture (pointer motion, drags, wheel steps) repaints within
/// the frame budget; anything else paints as soon as the writer is free. Whether
/// the event opened a menu.
fn apply(host: &mut shell::Host, pacer: &mut frames::Pacer, event: Event) -> bool {
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
    let menu = |host: &mut shell::Host| host.app().menu.as_ref().map(|menu| menu.id);
    let shown = menu(host);
    host.app().event(event);
    menu(host).is_some_and(|opened| shown != Some(opened))
}

/// Apply every input event already queued, so one frame answers a burst: a window
/// drag relays out once, for its latest size. A host request is settled first, and
/// a menu an event opened is drawn first: whether one was.
async fn drain(
    input: &mut EventStream,
    host: &mut shell::Host,
    pacer: &mut frames::Pacer,
) -> io::Result<bool> {
    while host.app().host.is_none() && host.app().stage != app::Stage::Stopped {
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
