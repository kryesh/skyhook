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
use skyhook::{
    agent::{Observation, SessionHandle},
    config::Config,
    identity::SessionId,
    remote::EmbeddedShimCatalog,
    session::SessionStore,
    tool::policy::AllowAll,
};
use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct Launch {
    config: Arc<Config>,
    pub model: String,
    pub profile: Option<String>,
    pub workspace: PathBuf,
    pub sessions: PathBuf,
    catalog: EmbeddedShimCatalog,
    interaction: Arc<UiInteraction>,
    approve_all: bool,
}
impl Launch {
    pub async fn create(&self, resume: Option<SessionId>) -> Result<SessionHandle, String> {
        let mut model = self.model.clone();
        let mut profile = self.profile.clone();
        if let Some(id) = resume {
            let records = SessionStore::read_records(&self.sessions, id)
                .await
                .map_err(|e| e.to_string())?;
            if let Some((m, p)) =
                skyhook::session::agent_selection(&records, &skyhook::identity::AgentId::root(id))
            {
                model = m;
                profile = p;
            }
        }
        if !self.config.models.contains_key(&model) {
            return Err(format!(
                "Model profile {model} is missing. Restore it in the configuration before resuming."
            ));
        }
        let mut config = (*self.config).clone();
        config.default_agent_profile = profile;
        let builder = config
            .harness_builder(&self.workspace, &model)
            .map_err(|e| e.to_string())?
            .shim_catalog(self.catalog.clone());
        let builder = if self.approve_all {
            builder.policy(Arc::new(AllowAll))
        } else {
            builder.policy(self.interaction.clone())
        };
        let harness = builder
            .question_handler(self.interaction.clone())
            .sensitive_prompt_handler(self.interaction.clone())
            .build()
            .await
            .map_err(|e| e.to_string())?;
        match resume {
            Some(id) => harness.resume_session(id).await,
            None => harness.new_session().await,
        }
        .map_err(|e| e.to_string())
    }
}

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
    let config = Arc::new(Config::load(args.config.as_deref()).await?);
    let (saved, warning) = state::load();
    let model = state::select(&config, args.model.as_deref(), saved.model.as_deref())?;
    let settings = state::settings()?;
    let keymap = keys::KeyMap::new(&settings.keybinds)?;
    if !matches!(settings.theme.as_str(), "dark" | "light") {
        return Err("tui.toml theme must be dark or light".into());
    }
    let workspace = tokio::fs::canonicalize(&args.workspace).await?;
    let sessions = config
        .session_root
        .clone()
        .unwrap_or_else(|| workspace.join(".skyhook/sessions"));
    let (interaction, mut prompts) = UiInteraction::new();
    let launch = Launch {
        model,
        profile: args
            .agent_profile
            .or_else(|| config.default_agent_profile.clone()),
        workspace,
        sessions,
        approve_all: args.approve_all || config.approve_all,
        config,
        catalog: EmbeddedShimCatalog::from_assets(super::embedded_shims::EMBEDDED_SHIMS)?,
        interaction: Arc::new(interaction),
    };
    let session = launch.create(args.resume).await?;
    let Observation {
        snapshot,
        mut updates,
    } = session.observe().await;
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
                event = updates.recv() => match event {
                    Ok(event) => {
                        let mut records = app.observe(event);
                        // Reduce a burst once instead of rebuilding the projection per token.
                        for _ in 0..255 {
                            match updates.try_recv() {
                                Ok(event) => records |= app.observe(event),
                                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                                    let observation = app.session.observe().await;
                                    app.snapshot = observation.snapshot;
                                    updates = observation.updates;
                                    app.reset_projection();
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                        if records { app.projection.rebuild(&app.snapshot); }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let observation = app.session.observe().await;
                        app.snapshot = observation.snapshot;
                        updates = observation.updates;
                        app.reset_projection();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                Some(prompt) = prompts.recv() => app.prompt(prompt),
                Some(work) = rx.recv() => {
                    if let Work::SessionReady(Ok(session)) = work {
                        let observation = session.observe().await;
                        updates = observation.updates;
                        app.set_session(session, observation.snapshot);
                    } else {
                        app.work(work);
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
    app.status.flush().await;
    app.session.shutdown().await?;
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
