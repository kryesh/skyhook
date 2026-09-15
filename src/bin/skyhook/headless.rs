//! A single root operation with journal-only diagnostics and deterministic cleanup.
use super::{
    cli::{ExecutionRequest, InitialInput},
    launch::{self, Launch},
};
use skyhook::agent::SessionHandle;
use std::io::{self, Write};

pub async fn run(
    request: ExecutionRequest,
    input: InitialInput,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = launch::load_config(&request.config, false).await?;
    // Model memory is shared with terminal launches, but UI settings are never read.
    let (saved, state_warning) = super::tui::state::load();
    let model = launch::select_model(&config, request.model.as_deref(), saved.model.as_deref())?;
    let launch = Launch::from_request(&request.config, model, None).await?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let session = launch.create(request.resume).await?;

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
            if request.resume.is_none()
                && saved.model.as_deref() != Some(launch.model.name())
                && let Err(error) = super::tui::state::remember(launch.model.name())
            {
                session
                    .record_status(
                        session.root_agent().clone(),
                        format!("Could not save model selection: {error}"),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
            }
            run_input(&session, &launch.workspace, input).await
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

async fn run_input(
    session: &SessionHandle,
    workspace: &std::path::Path,
    input: InitialInput,
) -> Result<(), String> {
    match input {
        InitialInput::Script(path) => {
            let source = tokio::fs::read_to_string(path)
                .await
                .map_err(|error| error.to_string())?;
            session
                .run_script(source)
                .await
                .map_err(|error| error.to_string())?;
        }
        InitialInput::Prompt { text, images } => {
            let attachments = launch::read_images(workspace, &images).await?;
            session
                .prompt_with_options(text, &attachments, skyhook::agent::PromptOptions::default())
                .await
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::tests::{Fixture, RunningHeadless, mock_provider, output, wait_bounded};
    use std::{
        fs,
        io::{BufRead, BufReader, Read},
        path::PathBuf,
        process::{Child, Command, Output, Stdio},
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    const READ_CONFIG: &str = "return await tool.read({path:'config/skyhook/config.toml'});";

    fn read(f: &Fixture, name: &str) -> String {
        fs::read_to_string(f.path(name)).unwrap()
    }

    /// Spawn a run whose session ID must be flushed before its workflow runs;
    /// the returned reader collects the rest of stdout.
    fn spawn_with_id(f: &Fixture, args: &[&str]) -> (Child, thread::JoinHandle<Vec<u8>>) {
        let mut child = f.command().args(args).spawn().unwrap();
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
        let id = rx.recv_timeout(Duration::from_secs(15));
        let id = id.expect("the session ID is flushed before the workflow runs");
        assert!(id.trim().parse::<skyhook::identity::SessionId>().is_ok());
        (child, reader)
    }

    fn finish(mut child: Child, reader: thread::JoinHandle<Vec<u8>>) -> Output {
        wait_bounded(&mut child);
        let mut output = child.wait_with_output().unwrap();
        output.stdout = reader.join().unwrap();
        output
    }

    #[test]
    fn script_results_console_and_status_are_journal_only_and_resume_appends() {
        let f = Fixture::new();
        let out = f.script(
            "console.log('journal-only-console'); return {answer: 42};",
            &[],
        );
        let before = f.journal(&out);
        assert!(out.status.success(), "{before}");
        let artifacts = f.artifacts(&out);
        assert!(artifacts.contains("journal-only-console") && artifacts.contains("\"answer\":42"));
        assert!(before.contains("Completed"));
        let id = std::str::from_utf8(&out.stdout).unwrap().trim();
        let source = "console.log('resumed-console'); return 'resumed-result';";
        let resumed = f.script(source, &["--resume", id]);
        assert!(resumed.status.success());
        assert_eq!(out.stdout, resumed.stdout);
        assert!(f.journal(&resumed).starts_with(&before));
        assert!(f.artifacts(&resumed).contains("resumed-result"));
    }

    #[test]
    fn script_and_input_failures_are_silent_and_persisted_after_open() {
        let f = Fixture::new();
        let source = "console.log('before-throw'); throw new Error('deliberate-failure');";
        let out = f.script(source, &[]);
        assert!(!out.status.success());
        let log = f.journal(&out);
        assert!(f.artifacts(&out).contains("before-throw"));
        assert!(log.contains("deliberate-failure") && log.contains("Failed:"));
        for args in [
            &["-s", "missing.js"][..],
            &["-p", "image", "--image", "missing.png"],
        ] {
            let out = output(f.command().arg("--non-interactive").args(args));
            assert!(!out.status.success());
            assert!(f.journal(&out).contains("Failed:"));
        }
    }

    #[test]
    fn exact_capability_override_and_approve_all_respect_permissions() {
        let f = Fixture::new();
        f.config("http://127.0.0.1:1/v1", "capabilities=[]");
        let empty = f.script("return 7;", &[]);
        assert!(empty.status.success(), "{empty:?}");
        f.journal(&empty);
        let denied = f.script(READ_CONFIG, &["--approve-all"]);
        assert!(!denied.status.success());
        assert!(f.journal(&denied).contains("Failed:"));
        let allowed = f.script(READ_CONFIG, &["--capabilities", "read"]);
        assert!(allowed.status.success(), "{}", f.journal(&allowed));
        f.config("http://127.0.0.1:1/v1", "");
        let unapproved = "return await tool.exec({argv:['sh','-c','echo not-approved']});";
        for (source, args) in [
            (READ_CONFIG, &["--capabilities="][..]),
            (unapproved, &["--capabilities", "exec"]),
        ] {
            let out = f.script(source, args);
            assert!(!out.status.success());
            f.journal(&out);
        }
        let approved = f.script("return await tool.exec({argv:['sh','-c','echo approved-output; echo approved-stderr >&2']});", &["--capabilities", "exec", "--approve-all"]);
        assert!(approved.status.success());
        f.journal(&approved);
        let artifacts = f.artifacts(&approved);
        assert!(artifacts.contains("approved-output") && artifacts.contains("approved-stderr"));
    }

    #[test]
    fn session_id_is_flushed_before_work_and_sigterm_drains_background_jobs() {
        let f = Fixture::new();
        fs::write(f.path("run.js"), "await tool.exec({argv:['sh','-c','echo started > started; sleep 2; echo leaked > leaked'],bg:true}); await sleep(60000);").unwrap();
        let args = ["--non-interactive", "--approve-all", "-s", "run.js"];
        let (child, reader) = spawn_with_id(&f, &args);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !f.path("started").exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(f.path("started").exists());
        let pid = child.id().to_string();
        assert!(
            Command::new("kill")
                .args(["-TERM", &pid])
                .status()
                .unwrap()
                .success()
        );
        let output = finish(child, reader);
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
    fn prompt_stream_is_saved_without_terminal_output() {
        use base64::Engine;
        let f = Fixture::new();
        let pixel = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aL1sAAAAASUVORK5CYII=";
        let pixel = base64::engine::general_purpose::STANDARD
            .decode(pixel)
            .unwrap();
        fs::write(f.path("pixel.png"), pixel).unwrap();
        let (endpoint, server) = mock_provider();
        f.config(&endpoint, "");
        let args = [
            "--non-interactive",
            "-p",
            "mock-user-prompt",
            "--image",
            "pixel.png",
        ];
        let out = output(f.command().args(args));
        assert!(out.status.success(), "{}", f.journal(&out));
        let log = f.journal(&out);
        assert!(log.contains("mock-user-prompt") && log.contains("mock-final-answer"));
        let (headers, request) = server.join().unwrap();
        assert!(headers.starts_with("POST /v1/chat/completions"));
        assert!(request.contains("mock-user-prompt"));
        assert!(request.contains("data:image/png;base64,"));
    }

    #[test]
    fn model_memory_explicit_selection_resume_and_startup_warnings_are_shared() {
        let f = Fixture::new();
        let mut config = read(&f, "config/skyhook/config.toml");
        config.push_str("\n[models.second]\nprovider='test'\nmodel='second-model'\nmax_context=128000\nmax_output=4096\n");
        fs::write(f.path("config/skyhook/config.toml"), config).unwrap();
        fs::create_dir_all(f.path("state/skyhook")).unwrap();
        fs::write(f.path("state/skyhook/ui.json"), r#"{"model":"second"}"#).unwrap();
        let saved = f.script("return 'saved';", &[]);
        assert!(saved.status.success());
        assert!(f.journal(&saved).contains(r#""model_profile":"second""#));
        let explicit = f.script("return 'explicit';", &["-m", "first"]);
        assert!(explicit.status.success());
        assert!(f.journal(&explicit).contains(r#""model_profile":"first""#));
        let remembered: serde_json::Value =
            serde_json::from_str(&read(&f, "state/skyhook/ui.json")).unwrap();
        assert_eq!(remembered["model"], "first");
        let id = std::str::from_utf8(&saved.stdout).unwrap().trim();
        let resumed = f.script("return 'resume-model';", &["--resume", id, "-m", "first"]);
        assert!(resumed.status.success());
        let journal = f.journal(&resumed);
        let records: Vec<skyhook::session::EventRecord> = journal
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let root = skyhook::identity::AgentId::root(id.parse().unwrap());
        assert_eq!(
            skyhook::session::agent_selection(&records, &root).unwrap(),
            "second"
        );
        fs::write(f.path("state/skyhook/ui.json"), "invalid JSON").unwrap();
        let warning = f.script("return 'warning';", &[]);
        assert!(warning.status.success());
        assert!(f.journal(&warning).contains("Could not read UI state"));
    }

    #[test]
    fn missing_resume_fails_before_id_and_authentication_fails_without_prompting() {
        let f = Fixture::new();
        let missing_id = "00000000000000000000000000000001";
        let args = ["--non-interactive", "-p", "hello", "--resume", missing_id];
        let out = output(f.command().args(args));
        assert!(!out.status.success());
        assert!(out.stdout.is_empty() && out.stderr.is_empty());
        fs::write(f.path("config/skyhook/config.toml"), "[providers.test]\nkind='codex'\n[models.first]\nprovider='test'\nmodel='fixture'\nmax_context=128000\nmax_output=4096\n").unwrap();
        let out = output(
            f.command()
                .args(["--non-interactive", "--approve-all", "-p", "hello"]),
        );
        assert!(!out.status.success());
        assert!(f.journal(&out).contains("Failed:"));
    }

    #[test]
    fn id_is_flushed_before_reading_the_workflow() {
        let f = Fixture::new();
        let fifo = f.path("workflow.fifo");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let (child, reader) = spawn_with_id(&f, &["--non-interactive", "-s", "workflow.fifo"]);
        fs::write(fifo, "return 'fifo-result';").unwrap();
        let output = finish(child, reader);
        assert!(output.status.success());
        f.journal(&output);
        assert!(f.artifacts(&output).contains("fifo-result"));
    }

    #[test]
    fn completion_and_failure_both_drain_background_processes() {
        for (ending, marker, success) in [
            ("return 'root-finished';", "root-finished", true),
            ("throw new Error('root-failure');", "root-failure", false),
        ] {
            let f = Fixture::new();
            let source = format!(
                "await tool.exec({{argv:['sh','-c','sleep 2; echo leaked > leaked'],bg:true}}); {ending}"
            );
            let out = f.script(&source, &["--approve-all"]);
            assert_eq!(out.status.success(), success);
            let log = f.journal(&out);
            assert!(log.contains(marker));
            assert!(log.contains("cancelled") || log.contains("interrupted"));
            thread::sleep(Duration::from_secs(3));
            assert!(!f.path("leaked").exists());
        }
    }

    #[test]
    fn concurrent_headless_instances_have_private_rejecting_askpass_without_targets() {
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};
        let mode =
            |path: &std::path::Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let unavailable = "authentication interaction unavailable";
        let fixtures = [Fixture::new(), Fixture::new()];
        let mut running = Vec::new();
        for fixture in &fixtures {
            let ambient = fixture.path("ambient-askpass");
            fs::write(
                &ambient,
                "#!/bin/sh\necho invoked > ambient-invoked\necho ambient-secret\n",
            )
            .unwrap();
            fs::set_permissions(&ambient, fs::Permissions::from_mode(0o700)).unwrap();
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
            let argv = serde_json::json!({"argv":["/bin/sh", "-c", command]});
            fs::write(
                fixture.path("run.js"),
                format!("return await tool.exec({argv});"),
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
            cmd.env_clear();
            cmd.args(["-c", "umask 000; exec \"$@\"", "headless-permissions-test"])
                .arg(base.get_program())
                .args(base.get_args())
                .current_dir(base.get_current_dir().unwrap())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            for (key, value) in base.get_envs() {
                match value {
                    Some(value) => cmd.env(key, value),
                    None => cmd.env_remove(key),
                };
            }
            cmd.env("SSH_ASKPASS", ambient)
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
        let sockets: Vec<_> = fixtures
            .iter()
            .map(|fixture| PathBuf::from(read(fixture, "socket-path").trim()))
            .collect();
        assert_ne!(sockets[0], sockets[1]);
        assert_ne!(sockets[0].parent(), sockets[1].parent());
        for (fixture, socket) in fixtures.iter().zip(&sockets) {
            assert!(fs::metadata(socket).unwrap().file_type().is_socket());
            assert_eq!(mode(socket), 0o600);
            assert_eq!(mode(socket.parent().unwrap()), 0o700);
            let helper = PathBuf::from(read(fixture, "helper-path").trim());
            assert_eq!(helper.parent(), socket.parent());
            assert_eq!(mode(&helper), 0o700);
            assert_eq!(read(fixture, "askpass-require"), "force\n");
            assert_eq!(read(fixture, "helper-status"), "1\n");
            assert!(read(fixture, "helper-stdout").is_empty());
            assert!(read(fixture, "helper-stderr").contains(unavailable));
            assert!(!fixture.path("ambient-invoked").exists());
            let agent = fixture.path("existing-agent.sock");
            assert_eq!(
                read(fixture, "inherited-agent").trim(),
                agent.to_str().unwrap()
            );
        }

        // Finishing one Skyhook must remove only its own socket, leaving the other live.
        for (index, fixture) in fixtures.iter().enumerate() {
            fs::write(fixture.path("release"), "").unwrap();
            let out = running.remove(0).finish();
            assert!(out.status.success());
            fixture.journal(&out);
            assert!(!sockets[index].exists());
            assert!(!sockets[index].parent().unwrap().exists());
            assert!(index == 1 || sockets[1].exists());
        }
        // The surviving broker must answer, not merely fail to connect.
        assert_eq!(read(&fixtures[1], "second-status"), "1\n");
        assert!(read(&fixtures[1], "second-stdout").is_empty());
        assert!(read(&fixtures[1], "second-stderr").contains(unavailable));
    }
}
