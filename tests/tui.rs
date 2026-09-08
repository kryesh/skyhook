//! Exercise the real binary and terminal protocol, with a local deterministic provider.
#![cfg(all(unix, feature = "tui"))]
use std::{
    fs::File,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

struct Terminal {
    child: Child,
    master: File,
    parser: vt100::Parser,
    bytes: Vec<u8>,
}
impl Terminal {
    fn launch(root: &std::path::Path, args: &[&str]) -> Self {
        let mut master = 0;
        let mut slave = 0;
        let size = libc::winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: output pointers and winsize are valid; ownership transfers to File below.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &size,
                )
            },
            0
        );
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        // SAFETY: descriptor remains owned by master.
        unsafe {
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_skyhook"));
        command
            .args([
                "--config",
                root.join("config.toml").to_str().unwrap(),
                "--workspace",
                root.to_str().unwrap(),
            ])
            .args(args)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env_remove("NO_COLOR")
            .env("HOME", root)
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // SAFETY: only async-signal-safe system calls run between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Self {
            child: command.spawn().unwrap(),
            master,
            parser: vt100::Parser::new(40, 120, 0),
            bytes: vec![],
        }
    }
    fn read(&mut self) {
        let mut bytes = [0; 16384];
        loop {
            match self.master.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) => {
                    self.parser.process(&bytes[..n]);
                    let start = self.bytes.len().saturating_sub(3);
                    self.bytes.extend_from_slice(&bytes[..n]);
                    let queries = self.bytes[start..]
                        .windows(4)
                        .filter(|w| *w == b"\x1b[6n")
                        .count();
                    for _ in 0..queries {
                        let (row, column) = self.parser.screen().cursor_position();
                        self.master
                            .write_all(format!("\x1b[{};{}R", row + 1, column + 1).as_bytes())
                            .unwrap();
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }
    fn wait(&mut self, text: &str) {
        self.wait_for(text, |screen| screen.contents().contains(text));
    }
    fn wait_for(&mut self, description: &str, ready: impl Fn(&vt100::Screen) -> bool) {
        let end = Instant::now() + Duration::from_secs(15);
        loop {
            self.read();
            if ready(self.parser.screen()) {
                return;
            }
            assert!(
                Instant::now() < end,
                "Missing {description:?}:\n{}\nraw:\n{}",
                self.parser.screen().contents(),
                String::from_utf8_lossy(&self.bytes)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn session(&self) -> String {
        self.parser
            .screen()
            .contents()
            .lines()
            .next()
            .unwrap()
            .split('·')
            .next_back()
            .unwrap()
            .trim()
            .to_owned()
    }
    fn send(&mut self, text: &str) {
        self.master.write_all(text.as_bytes()).unwrap();
    }
    fn resize(&mut self, rows: u16, columns: u16) {
        let size = libc::winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        self.parser.screen_mut().set_size(rows, columns);
        // SAFETY: master is a live PTY descriptor and size points to a valid winsize.
        assert_eq!(
            unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size) },
            0
        );
    }
    fn quit(&mut self) {
        self.send("\x18q");
        let end = Instant::now() + Duration::from_secs(5);
        let mut confirmed = false;
        loop {
            self.read();
            if !confirmed && self.parser.screen().contents().contains("Confirm action") {
                self.send("\x1b[B\r");
                confirmed = true;
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(
                Instant::now() < end,
                "TUI failed to exit: {}",
                self.parser.screen().contents()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            self.bytes.windows(8).any(|w| w == b"\x1b[?1049l"),
            "alternate screen not restored"
        );
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn config(root: &std::path::Path, url: &str) {
    std::fs::write(root.join("config.toml"),format!("default_model_profile='obsolete'\n[providers.test]\nkind='openai'\napi='chat_completions'\nbase_url='{url}'\n[models.first]\nprovider='test'\nmodel='fixture'\nmax_context=128000\nmax_output=4096\n")).unwrap();
}
// Own the listener and worker so failed tests cannot leave a server behind.
struct Provider {
    url: String,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Provider {
    fn new(delay: Duration) -> Self {
        Self::streaming(delay, None, vec![])
    }

    // Empty chunks select numbered replies; otherwise each supplied SSE chunk is gated.
    fn streaming(delay: Duration, gates: Option<Receiver<()>>, chunks: Vec<String>) -> Self {
        Self::streaming_responses(delay, gates, chunks, false)
    }

    // Only the first response uses custom chunks; later requests get numbered final replies.
    // Every response remains gated so tests can inspect a continuation before it finishes.
    fn streaming_first(gates: Receiver<()>, chunks: Vec<String>) -> Self {
        Self::streaming_responses(Duration::ZERO, Some(gates), chunks, true)
    }

    fn streaming_responses(
        delay: Duration,
        gates: Option<Receiver<()>>,
        chunks: Vec<String>,
        first_only: bool,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(e) => panic!("fixture accept: {e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_millis(50)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                let Ok(request) = read_request(&mut stream, &stopped) else {
                    continue;
                };
                let count = {
                    let mut requests = captured.lock().unwrap();
                    requests.push(request);
                    requests.len()
                };
                let replies = if chunks.is_empty() || (first_only && count > 1) {
                    vec![reply(count)]
                } else {
                    chunks.clone()
                };
                if write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").is_err() {
                    continue;
                }
                for chunk in replies {
                    if !wait_chunk(&stopped, gates.as_ref(), delay)
                        || stream.write_all(chunk.as_bytes()).is_err()
                    {
                        break;
                    }
                }
            }
        });
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let result = self.worker.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

fn wait_chunk(stop: &AtomicBool, gate: Option<&Receiver<()>>, delay: Duration) -> bool {
    let end = Instant::now() + gate.map_or(delay, |_| Duration::from_secs(15));
    while !stop.load(Ordering::Relaxed) {
        let remaining = end.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return gate.is_none();
        }
        let tick = remaining.min(Duration::from_millis(10));
        if let Some(gate) = gate {
            match gate.recv_timeout(tick) {
                Ok(()) => return true,
                Err(mpsc::RecvTimeoutError::Disconnected) => return false,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        } else {
            thread::sleep(tick);
        }
    }
    false
}

fn read_request(stream: &mut TcpStream, stop: &AtomicBool) -> std::io::Result<serde_json::Value> {
    let end = Instant::now() + Duration::from_secs(5);
    let mut request = Vec::new();
    let mut body = None;
    let mut buf = [0; 4096];
    while !stop.load(Ordering::Relaxed) && Instant::now() < end && request.len() < 1024 * 1024 {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => request.extend_from_slice(&buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(e) => return Err(e),
        }
        if body.is_none()
            && let Some(pos) = request.windows(4).position(|b| b == b"\r\n\r\n")
        {
            let headers = String::from_utf8_lossy(&request[..pos]).to_lowercase();
            let len = headers
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length:")?
                        .trim()
                        .parse::<usize>()
                        .ok()
                })
                .expect("fixture requests have content-length");
            body = Some((pos + 4, len));
        }
        if let Some((start, len)) = body
            && request.len() - start >= len
        {
            return Ok(serde_json::from_slice(&request[start..start + len])?);
        }
    }
    Err(std::io::Error::other("incomplete fixture request"))
}

fn reply(count: usize) -> String {
    format!(
        "data: {{\"id\":\"fixture\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"fixture\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"Fixture response {count}\"}},\"finish_reason\":null}}]}}\n\ndata: {{\"id\":\"fixture\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"fixture\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":100,\"completion_tokens\":84,\"total_tokens\":184,\"prompt_tokens_details\":{{\"cached_tokens\":20}}}}}}\n\ndata: [DONE]\n\n"
    )
}

fn session_directories(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut sessions = std::fs::read_dir(root.join(".skyhook/sessions"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    sessions.sort();
    sessions
}

#[test]
fn pristine_launch_typing_and_commands_do_not_create_a_session() {
    let root = tempfile::tempdir().unwrap();
    let provider = Provider::new(Duration::ZERO);
    config(root.path(), &provider.url);
    let sessions = root.path().join(".skyhook/sessions");
    let mut terminal = Terminal::launch(root.path(), &[]);
    terminal.wait("Start a conversation");
    assert!(
        !sessions.exists(),
        "idle startup must not create session storage"
    );

    terminal.send("Unsent draft");
    terminal.wait("Unsent draft");
    assert!(!sessions.exists(), "typing must not create session storage");
    terminal.send("\x18m");
    terminal.wait("Model");
    assert!(
        !sessions.exists(),
        "opening the model picker must remain a draft"
    );
    terminal.send("\r");
    terminal.wait_for("model picker closed with draft intact", |screen| {
        let contents = screen.contents();
        contents.contains("Unsent draft") && !contents.contains("Model")
    });
    assert!(!sessions.exists(), "selecting a model must remain a draft");

    terminal.send("\x03/");
    terminal.wait("Commands");
    assert!(
        !sessions.exists(),
        "slash commands must not create a session"
    );
    terminal.send("\x1b");
    terminal.wait_for("command palette closed", |screen| {
        !screen.contents().contains("Commands")
    });
    terminal.quit();
    assert!(
        !sessions.exists(),
        "quitting a draft must not create session storage"
    );
    assert!(provider.requests.lock().unwrap().is_empty());
}

#[test]
fn new_stays_a_draft_until_submission_and_quitting_does_not_save_it() {
    // Cover both abandoning the new draft and submitting it for a second session.
    for submit_second in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let provider = Provider::new(Duration::ZERO);
        config(root.path(), &provider.url);
        let sessions = root.path().join(".skyhook/sessions");
        let mut terminal = Terminal::launch(root.path(), &[]);
        terminal.wait("Start a conversation");
        terminal.send("/new");
        terminal.wait("Commands");
        terminal.send("\r");
        terminal.wait_for("new draft with command palette closed", |screen| {
            let contents = screen.contents();
            contents.contains("Start a conversation") && !contents.contains("Commands")
        });
        assert!(
            !sessions.exists(),
            "/new in a pristine launch must not create storage"
        );
        assert!(provider.requests.lock().unwrap().is_empty());

        terminal.send("First submitted message\r");
        terminal.wait("Fixture response 1");
        terminal.wait("84 · 100(80)");
        let first = session_directories(root.path());
        assert_eq!(
            first.len(),
            1,
            "first submission must create exactly one session"
        );
        assert!(first[0].join("events.jsonl").is_file());
        terminal.send("/new\r");
        terminal.wait("Start a conversation");
        assert_eq!(
            session_directories(root.path()),
            first,
            "/new must only return to a draft"
        );
        assert!(
            !terminal
                .parser
                .screen()
                .contents()
                .contains("Fixture response 1")
        );

        terminal.send("Second draft");
        terminal.wait("Second draft");
        assert_eq!(
            session_directories(root.path()),
            first,
            "unsent new draft must not create a session"
        );
        if submit_second {
            terminal.send("\r");
            terminal.wait("Fixture response 2");
            terminal.wait("84 · 100(80)");
            let second = session_directories(root.path());
            assert_eq!(
                second.len(),
                2,
                "submitting the new draft must create one more session"
            );
            assert!(
                second.contains(&first[0]),
                "the original session must remain"
            );
        }
        terminal.quit();
        assert_eq!(
            session_directories(root.path()).len(),
            if submit_second { 2 } else { 1 }
        );
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            if submit_second { 2 } else { 1 }
        );
    }
}

#[test]
fn model_selection_waits_for_submitted_messages_and_reply_footers_survive_resume() {
    let root = tempfile::tempdir().unwrap();
    let (advance, gates) = std::sync::mpsc::channel();
    let provider = Provider::streaming(Duration::ZERO, Some(gates), vec![]);
    let requests = &provider.requests;
    config(root.path(), &provider.url);
    let config_path = root.path().join("config.toml");
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    config.push_str(
        "\n[models.second]\nprovider='test'\nmodel='model-b'\nmax_context=64000\nmax_output=2048\n",
    );
    std::fs::write(&config_path, &config).unwrap();
    let mut terminal = Terminal::launch(root.path(), &["--prompt", "First question"]);
    terminal.wait_for("first request", |_| requests.lock().unwrap().len() == 1);
    terminal.wait("Working");
    let session = terminal.session();
    let journal = root
        .path()
        .join(".skyhook/sessions")
        .join(&session)
        .join("events.jsonl");
    let before = std::fs::read(&journal).unwrap();
    terminal.send("Queued with A\r");
    terminal.wait("1 follow-up(s) queued");
    terminal.send("\x18m");
    terminal.wait("Model");
    terminal.send("\x1b[B\r");
    terminal.wait_for("selected model B", |screen| {
        screen
            .contents()
            .lines()
            .last()
            .unwrap_or_default()
            .contains("model-b")
    });
    assert_eq!(
        std::fs::read(&journal).unwrap(),
        before,
        "selection must not write the session"
    );
    terminal.send("Queued with B\r");
    terminal.wait("2 follow-up(s) queued");
    // Select A again before either queued message runs; their captured choices must win.
    terminal.send("\x18m\x1b[A\r");
    advance.send(()).unwrap();
    terminal.wait_for("batched queued request", |_| {
        requests.lock().unwrap().len() == 2
    });
    {
        let requests = requests.lock().unwrap();
        // Both follow-ups enter the next request; the last captured model wins,
        // not the unsent selection made afterward.
        assert_eq!(requests[1]["model"], "model-b");
        let messages = requests[1]["messages"].to_string();
        assert!(messages.find("Queued with A").unwrap() < messages.find("Queued with B").unwrap());
    }
    advance.send(()).unwrap();
    terminal.wait("Fixture response 2");
    terminal.wait_for("recorded reply footer", |screen| {
        screen
            .contents()
            .lines()
            .filter(|line| line.trim() == "model-b")
            .count()
            == 1
    });
    terminal.quit();
    let records = std::fs::read_to_string(&journal).unwrap();
    assert!(records.contains("\"type\":\"model_changed\",\"model_profile\":\"second\""));
    // The pending A selection was never sent, so reopening restores B.
    let mut resumed = Terminal::launch(root.path(), &["--resume", &session]);
    resumed.wait("Fixture response 2");
    resumed.wait_for("restored model B", |screen| {
        screen
            .contents()
            .lines()
            .last()
            .unwrap_or_default()
            .contains("model-b")
    });
    let screen = resumed.parser.screen().contents();
    assert_eq!(
        screen
            .lines()
            .filter(|line| line.trim() == "fixture")
            .count(),
        1
    );
    assert_eq!(
        screen
            .lines()
            .filter(|line| line.trim() == "model-b")
            .count(),
        1
    );
    resumed.quit();
}

#[test]
fn queued_input_reaches_next_tool_continuation_request_in_fifo_order() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("queue-fixture.txt"),
        "Queue fixture contents",
    )
    .unwrap();
    let (advance, gates) = mpsc::channel();
    let tool_call = serde_json::json!({
        "id":"fixture", "object":"chat.completion.chunk", "created":0,
        "model":"fixture", "choices":[{"index":0,"delta":{
            "role":"assistant", "tool_calls":[{
                "index":0, "id":"call_queue_read", "type":"function",
                "function":{"name":"read", "arguments":"{\"path\":\"queue-fixture.txt\"}"}
            }]
        },"finish_reason":null}],
    });
    let tool_done = serde_json::json!({
        "id":"fixture", "object":"chat.completion.chunk", "created":0,
        "model":"fixture", "choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],
    });
    let provider = Provider::streaming_first(
        gates,
        vec![format!(
            "data: {tool_call}\n\ndata: {tool_done}\n\ndata: [DONE]\n\n"
        )],
    );
    config(root.path(), &provider.url);
    let requests = &provider.requests;
    let mut terminal = Terminal::launch(root.path(), &["--prompt", "First question"]);
    terminal.wait_for("first provider request", |_| {
        requests.lock().unwrap().len() == 1
    });
    terminal.wait("Working");
    terminal.send("Second question\r");
    terminal.wait("1 follow-up(s) queued");
    terminal.send("Discard this queued question\r");
    terminal.wait("2 follow-up(s) queued");
    terminal.send("Third question\r");
    terminal.wait("3 follow-up(s) queued");

    // Remove the middle item through the actual terminal queue UI, not a runtime API.
    terminal.send("/queue\r");
    terminal.wait("Queued follow-ups");
    terminal.wait("Discard this queued question");
    terminal.send("\x1b[B\x1b[3~");
    terminal.wait_for("middle queued item removed", |screen| {
        let contents = screen.contents();
        contents.contains("Queued follow-ups")
            && contents.contains("Second question")
            && contents.contains("Third question")
            && !contents.contains("Discard this queued question")
    });
    terminal.send("\x1b");
    terminal.wait_for("queue closed with two pending inputs", |screen| {
        let contents = screen.contents();
        !contents.contains("Queued follow-ups") && contents.contains("2 follow-up(s) queued")
    });
    assert_eq!(requests.lock().unwrap().len(), 1);

    // The first response cannot finish until both inputs have visibly been queued.
    // A tool call causes request #2 within this same turn, before any final answer.
    advance.send(()).unwrap();
    terminal.wait_for("tool continuation request", |_| {
        requests.lock().unwrap().len() >= 2
    });
    let captured = requests.lock().unwrap().clone();
    assert_eq!(captured.len(), 2);
    let messages = captured[1]["messages"].as_array().unwrap();
    let user_texts = messages
        .iter()
        .filter(|message| message["role"] == "user")
        .map(|message| {
            let content = &message["content"];
            if let Some(text) = content.as_str() {
                text.to_owned()
            } else {
                content
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        })
        // The runtime also sends its transient state as a synthetic user message.
        .filter(|text| !text.starts_with("<skyhook_state>"))
        .collect::<Vec<_>>();
    assert_eq!(
        user_texts,
        ["First question", "Second question", "Third question"],
        "queued inputs must reach request #2 together in FIFO order, not later turns: {}",
        captured[1]
    );
    assert!(
        messages.iter().any(|message| {
            message["role"] == "assistant" && message["tool_calls"][0]["id"] == "call_queue_read"
        }),
        "request #2 must continue the tool-calling turn: {}",
        captured[1]
    );
    assert!(
        messages.iter().any(|message| {
            message["role"] == "tool"
                && message["tool_call_id"] == "call_queue_read"
                && message["content"]
                    .to_string()
                    .contains("Queue fixture contents")
        }),
        "normal read permissions must produce a real tool result: {}",
        captured[1]
    );
    assert!(
        !captured[1]
            .to_string()
            .contains("Discard this queued question")
    );
    assert!(!captured[1].to_string().contains("Fixture response"));

    advance.send(()).unwrap();
    terminal.wait("Fixture response 2");
    terminal.quit();
    assert_eq!(
        requests.lock().unwrap().len(),
        2,
        "consumed or removed inputs must not trigger third/fourth requests"
    );
}

#[test]
fn reasoning_collapses_when_answer_starts_and_survives_completion_and_resume() {
    let (advance, gates) = mpsc::channel();
    // Exercise standard Responses reasoning rather than Chat's nonstandard
    // reasoning_content extension (deliberately unsupported by native Chat).
    let reasoning = serde_json::json!({"id":"rs_1","type":"reasoning",
        "summary":[{"type":"summary_text","text":"Reasoning retained from deltas\nMore detail"}],
        "encrypted_content":"fixture-opaque"});
    let answer = serde_json::json!({"id":"msg_1","type":"message","role":"assistant",
        "status":"completed","content":[{"type":"output_text","text":"The real answer","annotations":[]}]});
    let event = |value: serde_json::Value| format!("data: {value}\n\n");
    let chunks = vec![
        event(
            serde_json::json!({"type":"response.output_item.added","output_index":0,
            "item":{"id":"rs_1","type":"reasoning","summary":[]}}),
        ) + &event(
            serde_json::json!({"type":"response.reasoning_summary_text.delta","output_index":0,
            "item_id":"rs_1","summary_index":0,"delta":"Reasoning retained from deltas\nMore detail"}),
        ),
        event(
            serde_json::json!({"type":"response.output_item.done","output_index":0,"item":reasoning}),
        ) + &event(
            serde_json::json!({"type":"response.output_item.added","output_index":1,
            "item":{"id":"msg_1","type":"message","role":"assistant","status":"in_progress","content":[]}}),
        ) + &event(
            serde_json::json!({"type":"response.output_text.delta","output_index":1,
            "item_id":"msg_1","content_index":0,"delta":"The real answer"}),
        ),
        event(
            serde_json::json!({"type":"response.output_item.done","output_index":1,"item":answer}),
        ) + &event(
            serde_json::json!({"type":"response.completed","response":{"id":"resp_1","status":"completed",
            "output":[reasoning,answer],"usage":{"input_tokens":10,"output_tokens":5}}}),
        ),
    ];
    let provider = Provider::streaming(Duration::ZERO, Some(gates), chunks);
    advance.send(()).unwrap();
    let root = tempfile::tempdir().unwrap();
    config(root.path(), &provider.url);
    let config_path = root.path().join("config.toml");
    let text = std::fs::read_to_string(&config_path)
        .unwrap()
        .replace("api='chat_completions'", "api='responses'");
    std::fs::write(config_path, text).unwrap();
    let mut terminal = Terminal::launch(root.path(), &["--prompt", "Reasoning question"]);
    terminal.wait("Reasoning retained from deltas");
    terminal.wait_for("expanded live reasoning header", |screen| {
        screen
            .contents()
            .lines()
            .any(|line| line.contains("Reasoning") && line.contains('▾'))
    });
    let screen = terminal.parser.screen().contents();
    assert!(!screen.contains("streaming"));
    let symbol = screen
        .lines()
        .find(|line| line.contains("Reasoning") && line.contains('▾'))
        .unwrap_or_else(|| panic!("missing expanded reasoning header: {screen}"))
        .trim()
        .chars()
        .nth(2)
        .unwrap();
    assert!("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(symbol));
    terminal.wait_for("animated reasoning spinner", |screen| {
        screen
            .contents()
            .lines()
            .any(|line| line.contains("Reasoning") && line.contains('▾') && !line.contains(symbol))
    });
    advance.send(()).unwrap();
    terminal.wait("The real answer");
    let screen = terminal.parser.screen().contents();
    assert!(screen.contains("▸ Reasoning"));
    assert!(!screen.contains("Reasoning retained from deltas"));
    advance.send(()).unwrap();
    // Wait for the actual journal commit before checking the live-to-saved transition.
    let session = terminal.session();
    let journal = root
        .path()
        .join(".skyhook/sessions")
        .join(&session)
        .join("events.jsonl");
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let saved = std::fs::read_to_string(&journal).unwrap_or_default();
        if saved.contains("Reasoning retained from deltas") {
            break;
        }
        assert!(Instant::now() < end, "reasoning was not journaled: {saved}");
        std::thread::sleep(Duration::from_millis(10));
    }
    terminal.read();
    let screen = terminal.parser.screen().contents();
    assert_eq!(screen.matches("▸ Reasoning").count(), 1);
    let row = screen
        .lines()
        .position(|line| line.contains("▸ Reasoning"))
        .unwrap();
    terminal.send(&format!("\x1b[<0;3;{}M\x1b[<0;3;{}m", row + 1, row + 1));
    terminal.wait("Reasoning retained from deltas");
    terminal.quit();
    let mut resumed = Terminal::launch(root.path(), &["--resume", &session]);
    resumed.wait("The real answer");
    let screen = resumed.parser.screen().contents();
    assert_eq!(screen.matches("▸ Reasoning").count(), 1);
    assert!(!screen.contains("Reasoning retained from deltas"));
    let row = screen
        .lines()
        .position(|line| line.contains("▸ Reasoning"))
        .unwrap();
    resumed.send(&format!("\x1b[<0;3;{}M\x1b[<0;3;{}m", row + 1, row + 1));
    resumed.wait("Reasoning retained from deltas");
    resumed.quit();
}

#[test]
fn prompt_launch_stays_open_supports_followups_and_restores_terminal() {
    let root = tempfile::tempdir().unwrap();
    let provider = Provider::new(Duration::from_millis(30));
    config(root.path(), &provider.url);
    let mut terminal = Terminal::launch(root.path(), &["--prompt", "Initial question"]);
    terminal.wait("Fixture response 1");
    terminal.wait("84 · 100(80)");
    assert!(terminal.child.try_wait().unwrap().is_none());
    terminal.send("Follow-up\r");
    terminal.wait("Fixture response 2");
    terminal.wait("168 · 200(160)");
    terminal.send("\x18m");
    terminal.wait("Model");
    terminal.send("\x1b");
    terminal.send("\x1b[200~pasted\nsecond line\x1b[201~");
    terminal.wait("second line");
    // Exercise real Shift-Left decoding and selection deletion, not just editor methods.
    terminal.send("\x03abcdef\x1b[1;2D\x1b[1;2D\x1b[1;2D\x15x");
    terminal.wait("xdef");
    terminal.send("\r");
    terminal.wait("Fixture response 3");
    terminal.quit();
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join("state/skyhook/ui.json")).unwrap())
            .unwrap();
    assert_eq!(state["model"], "first");
}

#[test]
fn script_launch_uses_regular_approval_and_remains_interactive() {
    let root = tempfile::tempdir().unwrap();
    config(root.path(), "http://127.0.0.1:1");
    let script = root.path().join("workflow.js");
    std::fs::write(
        &script,
        "return await tool.exec({argv:['sh','-c',\"printf 'script marker'; sleep 1; printf '%s%s' final output\"], name:'run-fixture'});",
    )
    .unwrap();
    let mut terminal = Terminal::launch(root.path(), &["--script", script.to_str().unwrap()]);
    terminal.wait("Permission");
    assert_eq!(
        session_directories(root.path()).len(),
        1,
        "an explicit script must create a session"
    );
    terminal.send("\r");
    terminal.send("\x10");
    terminal.send("details\r");
    terminal.wait("script marker");
    terminal.wait("Completed");
    terminal.wait("finaloutput");
    assert!(terminal.child.try_wait().unwrap().is_none());
    terminal.quit();
}

#[test]
fn interruption_status_stays_in_history_after_followup_and_session_resume() {
    let root = tempfile::tempdir().unwrap();
    let provider = Provider::new(Duration::from_millis(300));
    config(root.path(), &provider.url);
    let mut terminal = Terminal::launch(root.path(), &["--prompt", "Initial interrupted question"]);
    terminal.wait("Initial interrupted question");
    terminal.send("\x1b");
    terminal.wait("Status · Interrupted");
    // The completed interruption result confirms that a new prompt can be submitted.
    terminal.wait("agent turn was interrupted");
    terminal.send("Follow-up after interruption\r");
    terminal.wait("Follow-up after interruption");
    terminal.wait("Fixture response");
    let screen = terminal.parser.screen().contents();
    assert!(
        screen.find("Status · Interrupted").unwrap()
            < screen.find("Follow-up after interruption").unwrap()
    );
    let session = terminal.session();
    terminal.quit();
    let mut resumed = Terminal::launch(root.path(), &["--resume", &session]);
    resumed.wait("Status · Interrupted");
    resumed.wait("Follow-up after interruption");
    let screen = resumed.parser.screen().contents();
    assert_eq!(screen.matches("Status · Interrupted").count(), 1);
    assert!(
        screen.find("Status · Interrupted").unwrap()
            < screen.find("Follow-up after interruption").unwrap()
    );
    resumed.quit();
}

#[test]
fn scroll_bursts_preserve_distance_without_backlogging_input_or_terminal_output() {
    let root = tempfile::tempdir().unwrap();
    config(root.path(), "http://127.0.0.1:1");
    let script = root.path().join("scroll.js");
    let mut source = (0..600)
        .map(|line| format!("// history line {line:04}: some text to scroll past\n"))
        .collect::<String>();
    source.push_str("return 'scroll tail marker';");
    std::fs::write(&script, source).unwrap();
    let mut terminal = Terminal::launch(root.path(), &["--script", script.to_str().unwrap()]);
    terminal.wait("Completed");
    terminal.send("\x10details\r");
    terminal.wait("scroll tail marker");
    terminal.read();
    let before = terminal.bytes.len();
    let start = Instant::now();
    // SGR mouse wheel up at a point in the conversation, followed immediately by typing.
    terminal.send(&format!(
        "{}scroll input marker",
        "\x1b[<64;50;12M".repeat(250)
    ));
    terminal.wait("scroll input marker");
    terminal.wait("history line 0000");
    let bytes = terminal.bytes.len() - before;
    eprintln!(
        "250 wheel events: {:?}, {bytes} terminal bytes",
        start.elapsed()
    );
    assert!(
        bytes < 32 * 1024,
        "scroll events generated {bytes} bytes of redundant frames"
    );
    terminal.quit();
}

#[test]
fn theme_and_resize_preserve_draft_and_navigation() {
    let root = tempfile::tempdir().unwrap();
    let provider = Provider::new(Duration::from_millis(10));
    config(root.path(), &provider.url);
    let mut terminal = Terminal::launch(root.path(), &["--prompt", "Initial question"]);
    terminal.wait("Fixture response 1");
    terminal.send("Unsent draft");
    terminal.wait("Unsent draft");
    let dark = terminal.parser.screen().cell(0, 0).unwrap().bgcolor();
    let state_path = root.path().join("state/skyhook/ui.json");
    let saved = std::fs::read(&state_path).unwrap();
    terminal.send("\x18t");
    terminal.wait("Theme");
    terminal.send("\x1b[B");
    terminal.wait_for("light theme preview", |screen| {
        screen.cell(2, 0).unwrap().bgcolor() == vt100::Color::Rgb(245, 245, 245)
    });
    assert_eq!(std::fs::read(&state_path).unwrap(), saved);
    terminal.send("\x1b");
    terminal.wait_for("cancelled preview restores black background", |screen| {
        screen.cell(2, 0).unwrap().bgcolor() == vt100::Color::Rgb(0, 0, 0)
    });
    assert_eq!(std::fs::read(&state_path).unwrap(), saved);
    terminal.send("\x18tlight\r");
    terminal.wait_for("light theme background", |screen| {
        screen.cell(2, 0).unwrap().bgcolor() == vt100::Color::Rgb(245, 245, 245)
    });
    assert!(!terminal.parser.screen().contents().contains("Theme:"));
    assert_ne!(dark, terminal.parser.screen().cell(0, 0).unwrap().bgcolor());
    assert!(terminal.parser.screen().contents().contains("Unsent draft"));
    terminal.resize(20, 44);
    terminal.wait("Chat");
    terminal.wait("84 · 100(80)");
    assert!(terminal.parser.screen().contents().contains("Unsent draft"));
    terminal.send("\t]");
    terminal.wait("Request");
    terminal.resize(40, 120);
    terminal.wait("Conversation");
    terminal.send("\x18i\t");
    terminal.send("\r");
    terminal.wait("Fixture response 2");
    terminal.quit();
}

#[test]
fn redirected_invocations_do_not_fall_back_to_the_removed_cli() {
    let output = Command::new(env!("CARGO_BIN_EXE_skyhook"))
        .arg("--prompt")
        .arg("hello")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires an interactive terminal"));
}

#[test]
fn failed_session_switch_preserves_draft_and_original_agent_then_can_retry() {
    let root = tempfile::tempdir().unwrap();
    let provider = Provider::new(Duration::from_millis(10));
    config(root.path(), &provider.url);
    let mut owner = Terminal::launch(root.path(), &["--prompt", "Locked session"]);
    owner.wait("Fixture response 1");
    let locked_id = std::fs::read_dir(root.path().join(".skyhook/sessions"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name()
        .to_str()
        .unwrap()
        .to_owned();
    let mut terminal = Terminal::launch(root.path(), &["--prompt", "Original session"]);
    terminal.wait("Fixture response 2");
    terminal.send("Unsent draft");
    terminal.send("\x18l");
    terminal.wait("Resume session");
    terminal.send(&format!("{locked_id}\r"));
    terminal.wait("already open");
    assert!(terminal.parser.screen().contents().contains("Unsent draft"));
    terminal.send("\x03Follow-up after rejected switch\r");
    terminal.wait("Fixture response 3");
    owner.quit();
    terminal.send("\x18l");
    terminal.wait("Resume session");
    terminal.send(&format!("{locked_id}\r"));
    terminal.wait_for("locked session successfully resumed", |screen| {
        screen
            .contents()
            .lines()
            .next()
            .unwrap_or_default()
            .contains(&locked_id)
    });
    terminal.send("Message in resumed session\r");
    terminal.wait("Fixture response 4");
    terminal.quit();
}
