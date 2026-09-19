//! End-to-end behaviour of the `skyhook` binary: dotenv loading, `--dump`, and
//! headless runs, each observed from outside the process.
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    root: tempfile::TempDir,
    cwd: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let fixture = Self {
            cwd: root.path().to_owned(),
            root,
        };
        fs::create_dir_all(fixture.path("config/skyhook")).unwrap();
        fixture.config("http://127.0.0.1:1/v1", "");
        fixture
    }
    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }
    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.path(name)).unwrap()
    }
    fn write(&self, name: &str, text: &str) {
        let path = self.path(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn config(&self, endpoint: &str, top: &str) {
        self.provider_config(endpoint, top, "");
    }
    fn provider_config(&self, endpoint: &str, top: &str, credential: &str) {
        self.write("config/skyhook/config.toml", &format!(
            "{top}\n[providers.test]\nkind='openai'\napi='chat_completions'\nbase_url='{endpoint}'\n{credential}\n[models.first]\nprovider='test'\nmodel='fixture'\nmax_context=128000\nmax_output=4096\nsupports_images=true\n"
        ));
    }
    fn bare_command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_skyhook"));
        cmd.current_dir(&self.cwd)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.path("config"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }
    fn command(&self) -> Command {
        let mut cmd = self.bare_command();
        cmd.arg("--config")
            .arg(self.path("config/skyhook/config.toml"))
            .arg("--workspace")
            .arg(self.root.path());
        cmd
    }
    fn script(&self, source: &str, extra: &[&str]) -> Output {
        self.write("run.js", source);
        output(
            self.command()
                .args(["--non-interactive", "-s"])
                .arg(self.path("run.js"))
                .args(extra),
        )
    }
    /// Saved job outputs and captures as text.
    fn artifacts(&self, output: &Output) -> String {
        let id = std::str::from_utf8(&output.stdout).unwrap().trim();
        let id = id.parse().unwrap();
        let sessions = self.path(".skyhook/sessions");
        let text = block_on(skyhook::session::SessionStore::read_output_text(
            &sessions, id,
        ));
        text.unwrap().concat()
    }
    /// Records as JSON lines.
    fn journal(&self, output: &Output) -> String {
        let records = self.records(output);
        let lines = records
            .iter()
            .map(|record| serde_json::to_string(record).unwrap());
        lines.map(|line| line + "\n").collect()
    }
    /// The committed records of the session whose id `output` printed.
    fn records(&self, output: &Output) -> Vec<skyhook::session::EventRecord> {
        assert!(
            output.stderr.is_empty(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = std::str::from_utf8(&output.stdout).unwrap();
        let id = stdout
            .strip_suffix('\n')
            .expect("session id ends with newline");
        let id = id.parse().expect("stdout contains only one session ID");
        let sessions = self.path(".skyhook/sessions");
        block_on(skyhook::session::SessionStore::read_records(&sessions, id)).unwrap()
    }
}

fn block_on<T>(future: impl Future<Output = T>) -> T {
    let runtime = tokio::runtime::Builder::new_current_thread().build();
    runtime.unwrap().block_on(future)
}

fn output(command: &mut Command) -> Output {
    let mut child = command.spawn().unwrap();
    wait_bounded(&mut child);
    child.wait_with_output().unwrap()
}

fn wait_bounded(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("skyhook did not terminate");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Poll briefly for a condition another process establishes.
fn eventually(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

fn assert_silent_failure(output: &Output) {
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty() && output.stderr.is_empty(),
        "{output:?}"
    );
}

/// Serves one streamed completion and returns the request's headers and body.
fn mock_provider() -> (String, thread::JoinHandle<(String, String)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        let (mut socket, _) = loop {
            match listener.accept() {
                Ok(socket) => break socket,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("mock provider accept: {error}"),
            }
        };
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        let mut headers = String::new();
        let mut length = 0;
        loop {
            let mut line = String::new();
            assert_ne!(reader.read_line(&mut line).unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
            headers.push_str(&line);
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let response = concat!(
            "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"mock-final-answer\"},\"finish_reason\":null}]}\n\n",
            "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
        (headers, String::from_utf8(body).unwrap())
    });
    (endpoint, server)
}

#[test]
fn parse_early_runtime_and_redirected_terminal_failures_are_reported_correctly() {
    let f = Fixture::new();
    let run = |args: &[&str]| output(f.command().args(args));
    for args in [
        &["--non-interactive", "--unknown"][..],
        &["--non-interactive=true", "-p", "x"],
        &["--non-interactive", "-p", "x", "-m", "missing"],
    ] {
        assert_silent_failure(&run(args));
    }
    // Terminal mode still rejects redirected IO.
    let out = run(&["-p", "hello"]);
    assert!(!out.status.success() && out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("interactive terminal"));
    f.write("config/skyhook/config.toml", "invalid [");
    assert_silent_failure(&run(&["--non-interactive", "-p", "x"]));
}

mod dotenv {
    use super::*;
    use std::ffi::OsString;

    const KEY: &str = "SKYHOOK_DOTENV_TEST_KEY";

    fn fixture() -> Fixture {
        let mut f = Fixture::new();
        for dir in ["invocation", "workspace"] {
            fs::create_dir_all(f.path(dir)).unwrap();
        }
        f.cwd = f.path("invocation");
        f
    }

    fn assert_headless_success(output: &Output) {
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        let stdout = std::str::from_utf8(&output.stdout).unwrap().trim();
        let id = stdout.parse::<skyhook::identity::SessionId>();
        id.expect("stdout must contain only the session ID");
    }

    /// What a local command sees for `expansion`, optionally with an inherited value.
    fn observed(f: &Fixture, expansion: &str, inherited: Option<&OsString>) -> Vec<u8> {
        let command = format!("printf '%s' \"{expansion}\" > env.out");
        f.write("invocation/run.js", &format!(
            "const result = await tool.exec({{argv: ['sh', '-c', {}]}}); if (result.exit_code !== 0) throw new Error('command failed'); return result;",
            serde_json::to_string(&command).unwrap()
        ));
        let mut command = f.command();
        command.args(["--non-interactive", "--approve-all", "-s", "run.js"]);
        if let Some(value) = inherited {
            command.env(KEY, value);
        }
        assert_headless_success(&output(&mut command));
        fs::read(f.path("env.out")).unwrap()
    }

    #[test]
    fn only_the_invocation_dotenv_is_loaded_and_inherited_values_win() {
        use std::os::unix::ffi::OsStringExt;
        let f = fixture();
        for path in [
            ".env",
            "workspace/.env",
            "config/.env",
            "config/skyhook/.env",
        ] {
            // Would abort startup if any of these files were read.
            f.write(path, "BROKEN='unterminated-secret\n");
        }
        assert_eq!(observed(&f, &format!("${{{KEY}-absent}}"), None), b"absent");
        let source = format!("\u{feff}export {KEY}=\"invocation-value\" # comment\n");
        f.write("invocation/.env", &source);
        assert_eq!(observed(&f, &format!("${KEY}"), None), b"invocation-value");
        for inherited in [
            OsString::from("inherited-value"),
            OsString::from(""),
            OsString::from_vec(b"non-unicode-\xff".to_vec()),
        ] {
            let seen = observed(&f, &format!("${KEY}"), Some(&inherited));
            assert_eq!(seen, inherited.into_vec());
        }
    }

    #[test]
    fn malformed_dotenv_is_sanitized_silent_in_headless_and_ignored_by_help_and_askpass() {
        let f = fixture();
        for source in [
            "PRIVATE_TOKEN='secret-never-print\n",
            "PRIVATE_TOKEN=secret\0never-print\n",
        ] {
            f.write("invocation/.env", source);
            let out = output(&mut f.command());
            assert!(!out.status.success() && out.stdout.is_empty());
            let stderr = String::from_utf8(out.stderr).unwrap();
            assert_eq!(stderr, "skyhook: invalid invocation directory .env file\n");
            let headless = ["--non-interactive", "-p", "unused"];
            assert_silent_failure(&output(f.command().args(headless)));
            for flag in ["--help", "--version"] {
                let out = output(f.bare_command().args(["--non-interactive", flag]));
                assert!(out.status.success(), "{out:?}");
                assert!(!out.stdout.is_empty() && out.stderr.is_empty());
            }
            // Without an inherited socket the helper exits before reading anything.
            assert_silent_failure(&output(f.bare_command().args(["--askpass", "Password:"])));
        }
    }

    #[test]
    fn dotenv_selects_the_config_directory_and_supplies_provider_credentials() {
        for credential in [
            format!("api_key_env='{KEY}'"),
            format!("api_key_command = \"printf '%s' \\\"${KEY}\\\"\""),
        ] {
            let f = fixture();
            let (endpoint, server) = mock_provider();
            f.provider_config(&endpoint, "", &credential);
            // HOME holds no configuration, so only the dotenv value can locate it.
            let config = f.path("config");
            let source = format!(
                "{KEY}=dotenv-api-key\nXDG_CONFIG_HOME='{}'\n",
                config.display()
            );
            f.write("invocation/.env", &source);
            let mut command = f.bare_command();
            command
                .env_remove("XDG_CONFIG_HOME")
                .arg("--workspace")
                .arg(f.path("workspace"))
                .args(["--non-interactive", "-p", "hello"]);
            assert_headless_success(&output(&mut command));
            let (headers, _) = server.join().unwrap();
            let headers = headers.to_ascii_lowercase();
            assert!(headers.contains("authorization: bearer dotenv-api-key\r\n"));
            // Logout finds the credentials through the dotenv directory as well.
            f.write("config/skyhook/codex-oauth.json", "removed without parsing");
            let out = output(
                f.bare_command()
                    .env_remove("XDG_CONFIG_HOME")
                    .args(["auth", "logout"]),
            );
            assert!(out.status.success() && out.stderr.is_empty(), "{out:?}");
            assert!(!config.join("skyhook/codex-oauth.json").exists());
        }
    }
}

mod dump {
    use super::*;

    fn successful_config(output: &Output) -> toml::Value {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = std::str::from_utf8(&output.stdout).unwrap();
        let _: skyhook::config::Config = toml::from_str(text).unwrap();
        toml::from_str(text).unwrap()
    }

    fn no_session(f: &Fixture) {
        assert!(!f.path(".skyhook/sessions").exists());
        assert!(!f.path("project/.skyhook/sessions").exists());
        assert!(!f.path(".skyhook/state.json").exists());
    }

    #[test]
    fn dump_config_merges_selected_workspace_applies_cli_policy_and_fails_whole() {
        let f = Fixture::new();
        f.config(
            "http://127.0.0.1:1/v1",
            "approve_all=true\ncapabilities=['read']",
        );
        f.write(".skyhook/config.toml", "[models.first]\nmax_output=0\n");
        f.write(
            "project/.skyhook/config.toml",
            "[models.first]\nmax_output=8192\n",
        );
        let project = f.path("project");
        let merged = output(
            f.bare_command()
                .args(["--dump", "--workspace"])
                .arg(&project),
        );
        let config = successful_config(&merged);
        assert_eq!(config["approve_all"].as_bool(), Some(true));
        assert_eq!(
            config["models"]["first"]["max_output"].as_integer(),
            Some(8192)
        );
        let overridden = output(
            f.bare_command()
                .args(["--dump=config", "--workspace"])
                .arg(&project)
                .args(["--capabilities", "read,agents", "--approve-all"]),
        );
        let config = successful_config(&overridden);
        let capabilities = ["read", "agents"].map(|name| toml::Value::String(name.into()));
        assert_eq!(config["capabilities"].as_array().unwrap(), &capabilities);
        assert_eq!(config["approve_all"].as_bool(), Some(true));
        // The invocation directory's own workspace config is invalid: no partial dump.
        let fatal = output(f.bare_command().arg("--dump"));
        assert!(!fatal.status.success());
        assert!(fatal.stdout.is_empty() && !fatal.stderr.is_empty());
        no_session(&f);
    }

    #[test]
    fn dump_config_does_not_resolve_secrets_start_mcp_run_ssh_or_load_terminal_state() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        f.write("bin/ssh", "#!/bin/sh\ntouch SSH_WAS_RUN\nexit 1\n");
        fs::set_permissions(f.path("bin/ssh"), fs::Permissions::from_mode(0o755)).unwrap();
        f.provider_config("http://127.0.0.1:1/v1", "", "api_key_env='MISSING_API_KEY'");
        let mut text = f.read("config/skyhook/config.toml");
        text.push_str(
            "\n[mcp.trap]\ntransport='stdio'\nstart_command=['/bin/sh','-c','touch MCP_WAS_RUN']\n",
        );
        text.push_str("\n[targets.remote]\ntype='ssh'\nhost='remote.test'\nvia='defined-later'\n");
        f.write("config/skyhook/config.toml", &text);
        f.write(".skyhook/state.json", "malformed saved state");
        let path = format!("{}:/usr/bin:/bin", f.path("bin").display());
        let dumped = output(
            f.bare_command()
                .env("PATH", path)
                .args(["--dump", "config"]),
        );
        let config = successful_config(&dumped);
        assert_eq!(
            config["providers"]["test"]["api_key_env"].as_str(),
            Some("MISSING_API_KEY")
        );
        assert_eq!(
            config["targets"]["remote"]["via"].as_str(),
            Some("defined-later")
        );
        assert!(!f.path("SSH_WAS_RUN").exists() && !f.path("MCP_WAS_RUN").exists());
        assert!(!f.path(".skyhook/sessions").exists());
        assert_eq!(f.read(".skyhook/state.json"), "malformed saved state");
    }

    #[test]
    fn dump_skills_shows_frontmatter_assets_and_errors_without_model_config() {
        let f = Fixture::new();
        fs::remove_file(f.path("config/skyhook/config.toml")).unwrap();
        f.write(
            ".agents/skills/release/SKILL.md",
            "---\nname: declared-release\ndescription: Release helper\nmetadata:\n  owner: maintainers\n  checks: [tests, lint]\n---\n# Release\n",
        );
        f.write(
            ".agents/skills/release/references/checklist.md",
            "Private asset body should not be dumped.",
        );
        f.write(
            ".agents/skills/release/scripts/run.sh",
            "touch SHOULD_NOT_EXIST",
        );
        let listed = output(f.bare_command().args(["--dump", "skills"]));
        assert!(
            listed.status.success(),
            "{}",
            String::from_utf8_lossy(&listed.stderr)
        );
        let text = String::from_utf8_lossy(&listed.stdout);
        for expected in [
            "release",
            "declared-release",
            "Release helper",
            "owner",
            "maintainers",
            "checks",
            "checklist.md",
            "run.sh",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(text.contains('├') || text.contains('└'), "{text}");
        assert!(!text.contains("Private asset body"));
        assert!(!f.path("SHOULD_NOT_EXIST").exists());
        f.write(
            ".agents/skills/broken/SKILL.md",
            "---\ndescription: [unterminated\n---\n",
        );
        let errors = output(f.bare_command().args(["--dump", "skills"]));
        assert!(!errors.status.success());
        assert!(String::from_utf8_lossy(&errors.stdout).contains("release"));
        assert!(String::from_utf8_lossy(&errors.stderr).contains("broken"));
        no_session(&f);
    }

    #[test]
    fn dump_diagnostics_escape_repository_controlled_terminal_sequences() {
        let f = Fixture::new();
        f.write(
            ".agents/skills/evil\u{1b}[2J/SKILL.md",
            "# Skill\nDescription.\n",
        );
        let skills = output(f.bare_command().args(["--dump", "skills"]));
        assert!(!skills.status.success());
        assert!(!skills.stderr.contains(&0x1b));
        assert!(String::from_utf8_lossy(&skills.stderr).contains("evil\\u{1b}[2J"));

        let path = f.path("odd\u{1b}[2J.toml");
        fs::copy(f.path("config/skyhook/config.toml"), &path).unwrap();
        let loaded = output(f.bare_command().args(["--dump", "--config"]).arg(&path));
        successful_config(&loaded);
        assert!(!loaded.stderr.contains(&0x1b));
        assert!(String::from_utf8_lossy(&loaded.stderr).contains("odd\\u{1b}[2J.toml"));
        fs::remove_file(&path).unwrap();
        let missing = output(f.bare_command().args(["--dump", "--config"]).arg(path));
        assert!(!missing.status.success() && missing.stdout.is_empty());
        assert!(!missing.stderr.contains(&0x1b));
        no_session(&f);
    }
}

mod headless {
    use super::*;
    use std::sync::mpsc;

    const READ_CONFIG: &str = "return await tool.read({path:'config/skyhook/config.toml'});";

    /// Leaves a background job running, having recorded its PID in `started`.
    const BACKGROUND: &str = "await tool.exec({argv:['sh','-c','echo $$ > started; sleep 60'],bg:true}); await tool.exec({argv:['sh','-c','until [ -s started ]; do sleep 0.01; done']});";

    /// Root completion is not enough: the run must also have ended `BACKGROUND`.
    fn assert_drained(f: &Fixture, log: &str) {
        assert!(log.contains("cancelled") || log.contains("interrupted"));
        let job = Path::new("/proc").join(f.read("started").trim());
        assert!(!job.exists(), "background process survived shutdown");
    }

    /// Run `source` from a FIFO: the session ID must be flushed before the workflow
    /// is readable. The returned reader collects the rest of stdout.
    fn spawn_fifo(f: &Fixture, source: &str) -> (Child, thread::JoinHandle<Vec<u8>>) {
        let fifo = f.path("workflow.fifo");
        let made = Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(made.success());
        let args = ["--non-interactive", "--approve-all", "-s", "workflow.fifo"];
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
        fs::write(fifo, source).unwrap();
        (child, reader)
    }

    fn finish(mut child: Child, reader: thread::JoinHandle<Vec<u8>>) -> Output {
        wait_bounded(&mut child);
        let mut output = child.wait_with_output().unwrap();
        output.stdout = reader.join().unwrap();
        output
    }

    #[test]
    fn script_results_are_journal_only_completion_drains_jobs_and_resume_appends() {
        let f = Fixture::new();
        let source =
            format!("{BACKGROUND} console.log('journal-only-console'); return {{answer: 42}};");
        // A workflow read from a FIFO completes like one read from a file.
        let (child, reader) = spawn_fifo(&f, &source);
        let out = finish(child, reader);
        let before = f.journal(&out);
        assert!(out.status.success(), "{before}");
        let artifacts = f.artifacts(&out);
        assert!(artifacts.contains("journal-only-console") && artifacts.contains("\"answer\":42"));
        assert!(before.contains("Completed"));
        assert_drained(&f, &before);
        let id = std::str::from_utf8(&out.stdout).unwrap().trim();
        let source = "console.log('resumed-console'); return 'resumed-result';";
        let resumed = f.script(source, &["--resume", id]);
        assert!(resumed.status.success());
        assert_eq!(out.stdout, resumed.stdout);
        assert!(f.journal(&resumed).starts_with(&before));
        assert!(f.artifacts(&resumed).contains("resumed-result"));
    }

    #[test]
    fn script_and_input_failures_are_silent_persisted_and_drain_jobs() {
        let f = Fixture::new();
        let source = format!(
            "{BACKGROUND} console.log('before-throw'); throw new Error('deliberate-failure');"
        );
        let out = f.script(&source, &["--approve-all"]);
        assert!(!out.status.success());
        let log = f.journal(&out);
        assert!(f.artifacts(&out).contains("before-throw"));
        assert!(log.contains("deliberate-failure") && log.contains("Failed:"));
        assert_drained(&f, &log);
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
    fn id_is_flushed_before_reading_the_workflow_and_sigterm_drains_background_jobs() {
        let f = Fixture::new();
        let (child, reader) = spawn_fifo(&f, &format!("{BACKGROUND} await sleep(60000);"));
        eventually("the background job", || {
            fs::metadata(f.path("started")).is_ok_and(|file| file.len() > 0)
        });
        let pid = child.id().to_string();
        let killed = Command::new("kill").args(["-TERM", &pid]).status().unwrap();
        assert!(killed.success());
        let output = finish(child, reader);
        assert!(!output.status.success());
        let log = f.journal(&output);
        assert!(log.contains("SIGTERM"));
        assert_drained(&f, &log);
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
        let mut config = f.read("config/skyhook/config.toml");
        config.push_str("\n[models.second]\nprovider='test'\nmodel='second-model'\nmax_context=128000\nmax_output=4096\n");
        f.write("config/skyhook/config.toml", &config);
        f.write(".skyhook/state.json", r#"{"model":"second"}"#);
        let saved = f.script("return 'saved';", &[]);
        assert!(saved.status.success());
        assert!(f.journal(&saved).contains(r#""profile":{"name":"second""#));
        let explicit = f.script("return 'explicit';", &["-m", "first"]);
        assert!(explicit.status.success());
        assert!(
            f.journal(&explicit)
                .contains(r#""profile":{"name":"first""#)
        );
        let remembered: serde_json::Value =
            serde_json::from_str(&f.read(".skyhook/state.json")).unwrap();
        assert_eq!(remembered["model"], "first");
        let id = std::str::from_utf8(&saved.stdout).unwrap().trim();
        let resumed = f.script("return 'resume-model';", &["--resume", id, "-m", "first"]);
        assert!(resumed.status.success());
        let records = f.records(&resumed);
        let root = skyhook::identity::AgentId::root(id.parse().unwrap());
        assert_eq!(
            skyhook::session::agent_selection(&records, &root).unwrap(),
            "second"
        );
        f.write(".skyhook/state.json", "invalid JSON");
        let warning = f.script("return 'warning';", &[]);
        assert!(warning.status.success());
        assert!(f.journal(&warning).contains("Could not read UI state"));
    }

    #[test]
    fn missing_resume_fails_before_id_and_authentication_fails_without_prompting() {
        let f = Fixture::new();
        let missing_id = "00000000000000000000000000000001";
        let args = ["--non-interactive", "-p", "hello", "--resume", missing_id];
        assert_silent_failure(&output(f.command().args(args)));
        f.write("config/skyhook/config.toml", "[providers.test]\nkind='codex'\n[models.first]\nprovider='test'\nmodel='fixture'\nmax_context=128000\nmax_output=4096\n");
        let out = output(
            f.command()
                .args(["--non-interactive", "--approve-all", "-p", "hello"]),
        );
        assert!(!out.status.success());
        assert!(f.journal(&out).contains("Failed:"));
    }

    #[test]
    fn local_commands_get_a_private_rejecting_askpass_instead_of_inherited_helpers() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        let ambient = f.path("ambient-askpass");
        f.write(
            "ambient-askpass",
            "#!/bin/sh\necho invoked > ambient-invoked\necho ambient-secret\n",
        );
        fs::set_permissions(&ambient, fs::Permissions::from_mode(0o700)).unwrap();
        // Targets stay disabled: ordinary exec must override inherited helpers too.
        let command = r#"
            printf '%s\n' "$SKYHOOK_ASKPASS_SOCKET" "$SSH_ASKPASS_REQUIRE" "$SSH_AUTH_SOCK" > askpass-env
            "$SSH_ASKPASS" 'Password:' > helper-stdout 2> helper-stderr
            printf '%s' "$?" > helper-status
        "#;
        let argv = serde_json::json!({"argv":["/bin/sh", "-c", command]});
        f.write("run.js", &format!("return await tool.exec({argv});"));
        let (inherited, agent) = (f.path("ambient.sock"), f.path("existing-agent.sock"));
        let out = output(
            f.command()
                .args(["--non-interactive", "--approve-all", "-s", "run.js"])
                .env("SSH_ASKPASS", ambient)
                .env("SKYHOOK_ASKPASS_SOCKET", &inherited)
                .env("SSH_ASKPASS_REQUIRE", "never")
                .env("SSH_AUTH_SOCK", &agent),
        );
        assert!(out.status.success(), "{}", f.journal(&out));
        let seen = f.read("askpass-env");
        let seen: Vec<_> = seen.lines().map(Path::new).collect();
        assert_ne!(seen[0], inherited);
        assert!(!seen[0].exists(), "the private socket outlived its run");
        assert_eq!(seen[1..], [Path::new("force"), &agent]);
        assert_eq!(f.read("helper-status"), "1");
        assert!(f.read("helper-stdout").is_empty());
        assert!(
            f.read("helper-stderr")
                .contains("authentication interaction unavailable")
        );
        assert!(!f.path("ambient-invoked").exists());
    }
}
