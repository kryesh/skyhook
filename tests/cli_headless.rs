#![cfg(all(feature = "tui", unix))]
//! Black-box tests: no terminal, credentials, or external provider is needed.
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    root: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            root: tempfile::tempdir().unwrap(),
        };
        fixture.config("http://127.0.0.1:1/v1", "");
        // Bad terminal settings must not prevent headless execution.
        fs::create_dir_all(fixture.path("config/skyhook")).unwrap();
        fs::write(fixture.path("config/skyhook/tui.toml"), "not valid TOML [").unwrap();
        fixture
    }
    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }
    fn config(&self, endpoint: &str, top: &str) {
        fs::write(self.path("skyhook.toml"), format!(
            "{top}\n[providers.test]\nkind='openai'\napi='chat_completions'\nbase_url='{endpoint}'\n[models.first]\nprovider='test'\nmodel='fixture'\nmax_context=128000\nmax_output=4096\nsupports_images=true\n"
        )).unwrap();
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_skyhook"));
        cmd.current_dir(self.root.path())
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_STATE_HOME", self.path("state"))
            .arg("--config")
            .arg(self.path("skyhook.toml"))
            .arg("--workspace")
            .arg(self.root.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }
    fn script(&self, source: &str, extra: &[&str]) -> Output {
        fs::write(self.path("run.js"), source).unwrap();
        self.command()
            .args(["--non-interactive", "-s", "run.js"])
            .args(extra)
            .output()
            .unwrap()
    }
    fn artifacts(&self, output: &Output) -> String {
        fn collect(path: &std::path::Path, text: &mut String) {
            for entry in fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    collect(&path, text);
                } else {
                    text.push_str(&fs::read_to_string(path).unwrap_or_default());
                }
            }
        }
        let id = std::str::from_utf8(&output.stdout).unwrap().trim();
        let mut text = String::new();
        collect(
            &self.path(&format!(".skyhook/sessions/{id}/jobs")),
            &mut text,
        );
        text
    }
    fn journal(&self, output: &Output) -> String {
        assert!(
            output.stderr.is_empty(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = std::str::from_utf8(&output.stdout).unwrap();
        let id = stdout
            .strip_suffix('\n')
            .expect("session id ends with newline");
        let _: skyhook::identity::SessionId =
            id.parse().expect("stdout contains only one session ID");
        fs::read_to_string(self.path(&format!(".skyhook/sessions/{id}/events.jsonl"))).unwrap()
    }
}

#[test]
fn script_results_console_and_status_are_journal_only_and_resume_appends() {
    let f = Fixture::new();
    let out = f.script(
        "console.log('journal-only-console'); return {answer: 42};",
        &[],
    );
    assert!(out.status.success());
    let before = f.journal(&out);
    let artifacts = f.artifacts(&out);
    assert!(artifacts.contains("journal-only-console"));
    assert!(artifacts.contains("\"answer\":42"));
    assert!(before.contains("Completed"));
    let id = std::str::from_utf8(&out.stdout).unwrap().trim();
    let resumed = f.script(
        "console.log('resumed-console'); return 'resumed-result';",
        &["--resume", id],
    );
    assert!(resumed.status.success());
    assert_eq!(out.stdout, resumed.stdout);
    let after = f.journal(&resumed);
    assert!(after.starts_with(&before));
    assert!(f.artifacts(&resumed).contains("resumed-result"));
}

#[test]
fn script_and_input_failures_are_silent_and_persisted_after_open() {
    let f = Fixture::new();
    let out = f.script(
        "console.log('before-throw'); throw new Error('deliberate-failure');",
        &[],
    );
    assert!(!out.status.success());
    let log = f.journal(&out);
    assert!(f.artifacts(&out).contains("before-throw"));
    assert!(log.contains("deliberate-failure"));
    assert!(log.contains("Failed:"));
    let missing = f
        .command()
        .args(["--non-interactive", "-s", "missing.js"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(f.journal(&missing).contains("Failed:"));
    let image = f
        .command()
        .args(["--non-interactive", "-p", "image", "--image", "missing.png"])
        .output()
        .unwrap();
    assert!(!image.status.success());
    assert!(f.journal(&image).contains("Failed:"));
}

#[test]
fn parse_and_early_runtime_failures_have_no_output_but_help_and_version_do() {
    let f = Fixture::new();
    for args in [
        vec!["--non-interactive"],
        vec!["--non-interactive", "-p", "x", "-s", "x"],
        vec!["--non-interactive", "-p", "x", "--capabilities", "bogus"],
        vec![
            "--non-interactive",
            "-p",
            "x",
            "--capabilities",
            "read,,write",
        ],
        vec!["--non-interactive", "--unknown"],
        vec!["--non-interactive=true", "-p", "x"],
        vec!["--non-interactive", "-s", "x", "--image", "x.png"],
        vec!["--non-interactive", "-p", "x", "-m", "missing"],
        vec!["--non-interactive", "-p", "x", "--resume", "not-an-id"],
    ] {
        let out = f.command().args(&args).output().unwrap();
        assert!(!out.status.success(), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}: {:?}", out.stdout);
        assert!(out.stderr.is_empty(), "{args:?}: {:?}", out.stderr);
    }
    for flag in ["--help", "--version"] {
        let out = f
            .command()
            .args(["--non-interactive", flag])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(!out.stdout.is_empty());
        assert!(out.stderr.is_empty());
    }
    fs::write(f.path("skyhook.toml"), "invalid [").unwrap();
    let out = f
        .command()
        .args(["--non-interactive", "-p", "x"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
}

#[test]
fn exact_capability_override_and_approve_all_never_restore_interactive() {
    let f = Fixture::new();
    f.config("http://127.0.0.1:1/v1", "capabilities=[]");
    let empty = f.script("return 7;", &[]);
    assert!(empty.status.success(), "{:?}", empty);
    f.journal(&empty);
    let denied = f.script(
        "return await tool.read({path:'skyhook.toml'});",
        &["--approve-all"],
    );
    assert!(!denied.status.success());
    assert!(f.journal(&denied).contains("Failed:"));
    let allowed = f.script(
        "return await tool.read({path:'skyhook.toml'});",
        &["--capabilities", "read"],
    );
    assert!(allowed.status.success(), "{}", f.journal(&allowed));
    f.config("http://127.0.0.1:1/v1", "");
    let removed = f.script(
        "return await tool.read({path:'skyhook.toml'});",
        &["--capabilities="],
    );
    assert!(!removed.status.success());
    f.journal(&removed);
    let questions = f.script(
        "return await tool.ask({id:'q',prompt:'Must not prompt'});",
        &["--capabilities", "interactive", "--approve-all"],
    );
    assert!(!questions.status.success());
    f.journal(&questions);
    let approval = f.script(
        "return await tool.exec({argv:['sh','-c','echo not-approved']});",
        &["--capabilities", "exec"],
    );
    assert!(!approval.status.success());
    f.journal(&approval);
    let approved = f.script("return await tool.exec({argv:['sh','-c','echo approved-output; echo approved-stderr >&2']});", &["--capabilities", "exec", "--approve-all"]);
    assert!(approved.status.success());
    f.journal(&approved);
    let artifacts = f.artifacts(&approved);
    assert!(artifacts.contains("approved-output") && artifacts.contains("approved-stderr"));
}

fn wait_bounded(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("headless process did not terminate");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn session_id_is_flushed_before_work_and_sigterm_drains_background_jobs() {
    let f = Fixture::new();
    fs::write(f.path("run.js"), "await tool.exec({argv:['sh','-c','echo started > started; sleep 2; echo leaked > leaked'],bg:true}); await sleep(60000);").unwrap();
    let mut child = f
        .command()
        .args(["--non-interactive", "--approve-all", "-s", "run.js"])
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut id = String::new();
        reader.read_line(&mut id).unwrap();
        tx.send(id.clone()).unwrap();
        reader.read_to_string(&mut id).unwrap();
        id.into_bytes()
    });
    let id = rx
        .recv_timeout(Duration::from_secs(15))
        .expect("ID must flush while workflow is running");
    assert!(id.trim().parse::<skyhook::identity::SessionId>().is_ok());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f.path("started").exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(f.path("started").exists());
    assert!(
        Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    wait_bounded(&mut child);
    let mut output = child.wait_with_output().unwrap();
    output.stdout = reader.join().unwrap();
    assert!(!output.status.success());
    let log = f.journal(&output);
    assert!(log.contains("SIGTERM"));
    assert!(log.contains("cancelled") || log.contains("interrupted"));
    thread::sleep(Duration::from_secs(3));
    assert!(
        !f.path("leaked").exists(),
        "background process survived shutdown"
    );
}

#[test]
fn normal_completion_cancels_background_work() {
    let f = Fixture::new();
    let out = f.script("await tool.exec({argv:['sh','-c','sleep 2; echo leaked > leaked'],bg:true}); return 'root-finished';", &["--approve-all"]);
    assert!(out.status.success());
    let log = f.journal(&out);
    assert!(log.contains("root-finished"));
    assert!(log.contains("cancelled") || log.contains("interrupted"));
    thread::sleep(Duration::from_secs(3));
    assert!(!f.path("leaked").exists());
}

#[test]
fn prompt_stream_is_saved_without_terminal_output() {
    use base64::Engine;
    let f = Fixture::new();
    fs::write(f.path("pixel.png"), base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aL1sAAAAASUVORK5CYII=").unwrap()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    f.config(&format!("http://{}/v1", listener.local_addr().unwrap()), "");
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        let (mut socket, _) = loop {
            match listener.accept() {
                Ok(socket) => break socket,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(10))
                }
                Err(error) => panic!("mock provider accept: {error}"),
            }
        };
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        let mut request = String::new();
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
            request.push_str(&line);
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let response = concat!(
            "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"mock-final-answer\"},\"finish_reason\":null}]}\n\n",
            "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
        (request, String::from_utf8(body).unwrap())
    });
    let mut child = f
        .command()
        .args([
            "--non-interactive",
            "-p",
            "mock-user-prompt",
            "--image",
            "pixel.png",
        ])
        .spawn()
        .unwrap();
    wait_bounded(&mut child);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", f.journal(&out));
    let log = f.journal(&out);
    assert!(log.contains("mock-user-prompt"));
    assert!(log.contains("mock-final-answer"));
    let (headers, request) = server.join().unwrap();
    assert!(headers.starts_with("POST /v1/chat/completions"));
    assert!(request.contains("mock-user-prompt"));
    assert!(request.contains("data:image/png;base64,"));
}

#[test]
fn model_memory_explicit_selection_resume_and_startup_warnings_are_shared() {
    let f = Fixture::new();
    let mut config = fs::read_to_string(f.path("skyhook.toml")).unwrap();
    config.push_str("\n[models.second]\nprovider='test'\nmodel='second-model'\nmax_context=128000\nmax_output=4096\n");
    fs::write(f.path("skyhook.toml"), config).unwrap();
    fs::create_dir_all(f.path("state/skyhook")).unwrap();
    fs::write(f.path("state/skyhook/ui.json"), r#"{"model":"second"}"#).unwrap();
    let saved = f.script("return 'saved';", &[]);
    assert!(saved.status.success());
    assert!(f.journal(&saved).contains(r#""model_profile":"second""#));
    let explicit = f.script("return 'explicit';", &["-m", "first"]);
    assert!(explicit.status.success());
    assert!(f.journal(&explicit).contains(r#""model_profile":"first""#));
    let remembered: serde_json::Value =
        serde_json::from_slice(&fs::read(f.path("state/skyhook/ui.json")).unwrap()).unwrap();
    assert_eq!(remembered["model"], "first");
    let id = std::str::from_utf8(&saved.stdout).unwrap().trim();
    let resumed = f.script("return 'resume-model';", &["--resume", id, "-m", "first"]);
    assert!(resumed.status.success());
    let records = f
        .journal(&resumed)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect::<Vec<skyhook::session::EventRecord>>();
    let selection = skyhook::session::agent_selection(
        &records,
        &skyhook::identity::AgentId::root(id.parse().unwrap()),
    )
    .unwrap();
    assert_eq!(selection.0, "second");
    fs::write(f.path("state/skyhook/ui.json"), "invalid JSON").unwrap();
    let warning = f.script("return 'warning';", &[]);
    assert!(warning.status.success());
    assert!(f.journal(&warning).contains("Could not read UI state"));
}

#[test]
fn missing_resume_fails_before_id_and_authentication_fails_without_prompting() {
    let f = Fixture::new();
    let missing_id = "00000000000000000000000000000001";
    let out = f
        .command()
        .args(["--non-interactive", "-p", "hello", "--resume", missing_id])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    fs::write(f.path("skyhook.toml"), "[providers.test]\nkind='codex'\n[models.first]\nprovider='test'\nmodel='fixture'\nmax_context=128000\nmax_output=4096\n").unwrap();
    let mut child = f
        .command()
        .args(["--non-interactive", "--approve-all", "-p", "hello"])
        .spawn()
        .unwrap();
    wait_bounded(&mut child);
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(f.journal(&out).contains("Failed:"));
}

#[test]
fn id_is_flushed_before_reading_the_workflow() {
    let f = Fixture::new();
    assert!(
        Command::new("mkfifo")
            .arg(f.path("workflow.fifo"))
            .status()
            .unwrap()
            .success()
    );
    let mut child = f
        .command()
        .args(["--non-interactive", "-s", "workflow.fifo"])
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut output = String::new();
        reader.read_line(&mut output).unwrap();
        tx.send(output.clone()).unwrap();
        reader.read_to_string(&mut output).unwrap();
        output.into_bytes()
    });
    let id = rx
        .recv_timeout(Duration::from_secs(15))
        .expect("opening a workflow must not precede the flushed ID");
    assert!(id.trim().parse::<skyhook::identity::SessionId>().is_ok());
    fs::write(f.path("workflow.fifo"), "return 'fifo-result';").unwrap();
    wait_bounded(&mut child);
    let mut output = child.wait_with_output().unwrap();
    output.stdout = reader.join().unwrap();
    assert!(output.status.success());
    f.journal(&output);
    assert!(f.artifacts(&output).contains("fifo-result"));
}

#[test]
fn terminal_mode_still_rejects_redirected_io() {
    let f = Fixture::new();
    let out = f.command().args(["-p", "hello"]).output().unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("interactive terminal"));
}

#[test]
fn script_failure_also_drains_background_processes() {
    let f = Fixture::new();
    let out = f.script("await tool.exec({argv:['sh','-c','sleep 2; echo leaked > leaked'],bg:true}); throw new Error('root-failure');", &["--approve-all"]);
    assert!(!out.status.success());
    let log = f.journal(&out);
    assert!(log.contains("Failed:") && log.contains("root-failure"));
    assert!(log.contains("cancelled") || log.contains("interrupted"));
    thread::sleep(Duration::from_secs(3));
    assert!(!f.path("leaked").exists());
}

/// Keep cleanup reliable even if a socket/permission assertion fails mid-run.
struct RunningHeadless(Option<Child>);
impl RunningHeadless {
    fn finish(mut self) -> Output {
        wait_bounded(self.0.as_mut().unwrap());
        self.0.take().unwrap().wait_with_output().unwrap()
    }
}
impl Drop for RunningHeadless {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let deadline = Instant::now() + Duration::from_secs(3);
            while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn concurrent_headless_instances_have_private_rejecting_askpass_without_targets() {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    let fixtures = [Fixture::new(), Fixture::new()];
    let mut running = Vec::new();
    for fixture in &fixtures {
        fs::write(
            fixture.path("ambient-askpass"),
            "#!/bin/sh\necho invoked > ambient-invoked\necho ambient-secret\n",
        )
        .unwrap();
        fs::set_permissions(
            fixture.path("ambient-askpass"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        // Do not enable targets: ordinary exec must override inherited helpers too.
        let command = r#"
            printf '%s\n' "$SKYHOOK_ASKPASS_SOCKET" > socket-path
            printf '%s\n' "$SSH_ASKPASS" > helper-path
            printf '%s\n' "$SSH_ASKPASS_REQUIRE" > askpass-require
            printf '%s\n' "$SSH_AUTH_SOCK" > inherited-agent
            "$SSH_ASKPASS" 'Password:' > helper-stdout 2> helper-stderr
            printf '%s\n' "$?" > helper-status
            touch ready
            while [ ! -f release ]; do sleep 0.02; done
            "$SSH_ASKPASS" 'Enter passphrase:' > second-stdout 2> second-stderr
            printf '%s\n' "$?" > second-status
        "#;
        fs::write(
            fixture.path("run.js"),
            format!(
                "return await tool.exec({});",
                serde_json::json!({"argv":["/bin/sh", "-c", command]})
            ),
        )
        .unwrap();
        let mut base = fixture.command();
        base.args([
            "--non-interactive",
            "--approve-all",
            "--capabilities",
            "exec",
            "-s",
            "run.js",
        ]);
        // Set umask only inside this subprocess, never in the parallel test runner.
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "umask 000; exec \"$@\"", "headless-permissions-test"])
            .arg(base.get_program())
            .args(base.get_args())
            .current_dir(base.get_current_dir().unwrap())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in base.get_envs() {
            if let Some(value) = value {
                cmd.env(key, value);
            } else {
                cmd.env_remove(key);
            }
        }
        cmd.env("SSH_ASKPASS", fixture.path("ambient-askpass"))
            .env("SKYHOOK_ASKPASS_SOCKET", fixture.path("ambient.sock"))
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("DISPLAY", "ambient")
            .env("SSH_AUTH_SOCK", fixture.path("existing-agent.sock"));
        running.push(RunningHeadless(Some(cmd.spawn().unwrap())));
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    while fixtures
        .iter()
        .any(|fixture| !fixture.path("ready").exists())
    {
        assert!(
            Instant::now() < deadline,
            "headless askpass helpers failed to respond"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let sockets = fixtures
        .iter()
        .map(|fixture| {
            PathBuf::from(
                fs::read_to_string(fixture.path("socket-path"))
                    .unwrap()
                    .trim(),
            )
        })
        .collect::<Vec<_>>();
    assert_ne!(sockets[0], sockets[1]);
    assert_ne!(sockets[0].parent(), sockets[1].parent());
    for (fixture, socket) in fixtures.iter().zip(&sockets) {
        assert!(fs::metadata(socket).unwrap().file_type().is_socket());
        assert_eq!(
            fs::metadata(socket).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(socket.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let helper = PathBuf::from(
            fs::read_to_string(fixture.path("helper-path"))
                .unwrap()
                .trim(),
        );
        assert_eq!(helper.parent(), socket.parent());
        assert_eq!(
            fs::metadata(helper).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::read_to_string(fixture.path("askpass-require")).unwrap(),
            "force\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.path("helper-status")).unwrap(),
            "1\n"
        );
        assert!(fs::read(fixture.path("helper-stdout")).unwrap().is_empty());
        assert!(
            fs::read_to_string(fixture.path("helper-stderr"))
                .unwrap()
                .contains("authentication interaction unavailable")
        );
        assert!(!fixture.path("ambient-invoked").exists());
        assert_eq!(
            fs::read_to_string(fixture.path("inherited-agent"))
                .unwrap()
                .trim(),
            fixture.path("existing-agent.sock").to_str().unwrap()
        );
    }

    // Finishing one Skyhook must remove only its own socket, leaving the other live.
    fs::write(fixtures[0].path("release"), "").unwrap();
    let out = running.remove(0).finish();
    assert!(out.status.success());
    fixtures[0].journal(&out);
    assert!(!sockets[0].exists());
    assert!(!sockets[0].parent().unwrap().exists());
    assert!(sockets[1].exists());
    fs::write(fixtures[1].path("release"), "").unwrap();
    let out = running.remove(0).finish();
    assert!(out.status.success());
    fixtures[1].journal(&out);
    assert_eq!(
        fs::read_to_string(fixtures[1].path("second-status")).unwrap(),
        "1\n"
    );
    assert!(
        fs::read(fixtures[1].path("second-stdout"))
            .unwrap()
            .is_empty()
    );
    assert!(
        fs::read_to_string(fixtures[1].path("second-stderr"))
            .unwrap()
            .contains("authentication interaction unavailable"),
        "the surviving broker must answer, not merely fail to connect"
    );
    assert!(!sockets[1].exists());
    assert!(!sockets[1].parent().unwrap().exists());
}
