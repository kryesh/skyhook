#![cfg(all(feature = "tui", unix))]
//! Black-box coverage keeps environment mutation out of the threaded test runner.
//! These tests exercise host startup and LOCAL subprocess inheritance only.
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

const KEY: &str = "SKYHOOK_DOTENV_TEST_KEY";

struct Fixture {
    root: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            root: tempfile::tempdir().unwrap(),
        };
        for directory in ["invocation", "workspace", "config/skyhook", "home"] {
            fs::create_dir_all(fixture.path(directory)).unwrap();
        }
        fixture.config("http://127.0.0.1:1/v1", "");
        fixture
    }
    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }
    fn config(&self, endpoint: &str, credential: &str) {
        fs::write(
            self.path("config/skyhook/config.toml"),
            format!(
                "[providers.test]\nkind='openai'\napi='chat_completions'\nbase_url='{endpoint}'\n{credential}\n[models.first]\nprovider='test'\nmodel='fixture'\nmax_context=128000\nmax_output=4096\n"
            ),
        )
        .unwrap();
    }
    fn dotenv(&self, contents: &str) {
        fs::write(self.path("invocation/.env"), contents).unwrap();
    }
    fn bare_command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_skyhook"));
        command
            .current_dir(self.path("invocation"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_STATE_HOME", self.path("state"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
    fn command(&self) -> Command {
        let mut command = self.bare_command();
        command
            .arg("--config")
            .arg(self.path("config/skyhook/config.toml"))
            .arg("--workspace")
            .arg(self.path("workspace"));
        command
    }
    fn script(&self, shell: &str) -> Command {
        let source = format!(
            "const result = await tool.exec({{argv: ['sh', '-c', {}]}}); if (result.exit_code !== 0) throw new Error('command failed'); return result;",
            serde_json::to_string(shell).unwrap()
        );
        fs::write(self.path("invocation/run.js"), source).unwrap();
        let mut command = self.command();
        command.args(["--non-interactive", "--approve-all", "-s", "run.js"]);
        command
    }
}

fn output(command: &mut Command) -> Output {
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("CLI timed out: {output:?}");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn assert_headless_success(output: &Output) {
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    std::str::from_utf8(&output.stdout)
        .unwrap()
        .trim()
        .parse::<skyhook::identity::SessionId>()
        .expect("stdout must contain only the session ID");
}

#[test]
fn invocation_dotenv_not_workspace_or_config_reaches_local_commands() {
    let f = Fixture::new();
    f.dotenv(&format!("{KEY}=invocation-value\n"));
    for path in [
        ".env",
        "workspace/.env",
        "config/.env",
        "config/skyhook/.env",
    ] {
        fs::write(f.path(path), format!("{KEY}=wrong-directory\n")).unwrap();
    }
    let out = output(&mut f.script(&format!("printf '%s' \"${KEY}\" > env.out")));
    assert_headless_success(&out);
    assert_eq!(
        fs::read_to_string(f.path("workspace/env.out")).unwrap(),
        "invocation-value"
    );
}

#[test]
fn inherited_values_win_including_empty_and_non_unicode_values() {
    use std::os::unix::ffi::OsStringExt;
    for inherited in [
        std::ffi::OsString::from("inherited-value"),
        std::ffi::OsString::from(""),
        std::ffi::OsString::from_vec(b"non-unicode-\xff".to_vec()),
    ] {
        let f = Fixture::new();
        f.dotenv(&format!("{KEY}=dotenv-value\n"));
        let mut command = f.script(&format!("printf '%s' \"${KEY}\" > env.out"));
        command.env(KEY, &inherited);
        let out = output(&mut command);
        assert_headless_success(&out);
        assert_eq!(
            fs::read(f.path("workspace/env.out")).unwrap(),
            inherited.into_vec()
        );
    }
}

#[test]
fn missing_dotenv_does_not_search_ancestors_workspace_or_config() {
    let f = Fixture::new();
    for path in [
        ".env",
        "workspace/.env",
        "config/.env",
        "config/skyhook/.env",
    ] {
        // Would abort startup if any of these files were read.
        fs::write(f.path(path), "BROKEN='unterminated-secret\n").unwrap();
    }
    let out = output(&mut f.script(&format!("printf '%s' \"${{{KEY}-absent}}\" > env.out")));
    assert_headless_success(&out);
    assert_eq!(
        fs::read_to_string(f.path("workspace/env.out")).unwrap(),
        "absent"
    );
}

#[test]
fn standard_quotes_comments_export_bom_and_interpolation() {
    let f = Fixture::new();
    f.dotenv(concat!(
        "\u{feff}# dotenv comment\n",
        "export SKYHOOK_DOTENV_TEST_KEY=\"quoted # value\" # trailing comment\n",
        "SKYHOOK_DOTENV_LITERAL='literal $SKYHOOK_DOTENV_TEST_KEY # text'\n",
        "SKYHOOK_DOTENV_EXPANDED=\"${SKYHOOK_DOTENV_TEST_KEY}/${SKYHOOK_DOTENV_PARENT}\"\n",
        "SKYHOOK_DOTENV_MULTILINE=\"first\nsecond\"\n",
        "SKYHOOK_DOTENV_ESCAPED=\"first\\nsecond\"\n",
    ));
    let mut command = f.script("printf '%s\n' \"$SKYHOOK_DOTENV_TEST_KEY\" \"$SKYHOOK_DOTENV_LITERAL\" \"$SKYHOOK_DOTENV_EXPANDED\" \"$SKYHOOK_DOTENV_MULTILINE\" \"$SKYHOOK_DOTENV_ESCAPED\" > env.out");
    command.env("SKYHOOK_DOTENV_PARENT", "parent-value");
    let out = output(&mut command);
    assert_headless_success(&out);
    assert_eq!(
        fs::read_to_string(f.path("workspace/env.out")).unwrap(),
        "quoted # value\nliteral $SKYHOOK_DOTENV_TEST_KEY # text\nquoted # value/parent-value\nfirst\nsecond\nfirst\nsecond\n"
    );
}

#[test]
fn dotenv_is_loaded_before_default_config_and_auth_path_selection() {
    let f = Fixture::new();
    // With inherited XDG_CONFIG_HOME absent, startup must use the dotenv value
    // rather than the empty HOME configuration directory.
    f.dotenv(&format!(
        "XDG_CONFIG_HOME='{}'\n",
        f.path("config").display()
    ));
    fs::write(f.path("invocation/run.js"), "return 'configured';").unwrap();
    let mut command = f.bare_command();
    command
        .env_remove("XDG_CONFIG_HOME")
        .arg("--workspace")
        .arg(f.path("workspace"))
        .args(["--non-interactive", "-s", "run.js"]);
    assert_headless_success(&output(&mut command));

    let credentials = f.path("config/skyhook/codex-oauth.json");
    fs::write(&credentials, "fixture credentials removed without parsing").unwrap();
    let mut command = f.bare_command();
    command
        .env_remove("XDG_CONFIG_HOME")
        .args(["auth", "logout"]);
    let out = output(&mut command);
    assert!(out.status.success(), "{out:?}");
    assert!(out.stderr.is_empty());
    assert!(
        !credentials.exists(),
        "auth must use the dotenv config directory"
    );
}

#[test]
fn malformed_dotenv_is_sanitized_silent_in_headless_and_ignored_for_help_version() {
    let f = Fixture::new();
    for source in [
        "PRIVATE_TOKEN='secret-never-print\n",
        "PRIVATE_TOKEN=secret\0never-print\n",
    ] {
        f.dotenv(source);
        let out = output(&mut f.command());
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
        assert_eq!(
            String::from_utf8(out.stderr).unwrap(),
            "skyhook: invalid invocation directory .env file\n"
        );
        let out = output(&mut f.command().args(["--non-interactive", "-p", "unused"]));
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
        assert!(out.stderr.is_empty());
        for flag in ["--help", "--version"] {
            let out = output(&mut f.bare_command().args(["--non-interactive", flag]));
            assert!(out.status.success(), "{out:?}");
            assert!(!out.stdout.is_empty());
            assert!(out.stderr.is_empty());
        }
    }
}

#[test]
fn askpass_never_loads_dotenv_or_accepts_its_socket_override() {
    let f = Fixture::new();
    f.dotenv("SKYHOOK_ASKPASS_SOCKET=/dotenv-must-not-control-askpass\n");
    let out = output(&mut f.bare_command().args(["--askpass", "Password:"]));
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    // Missing inherited socket exits silently; a loaded socket would report an IO error.
    assert!(out.stderr.is_empty());
    f.dotenv("PRIVATE_TOKEN='malformed-secret\n");
    let out = output(&mut f.bare_command().args(["--askpass", "Password:"]));
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(out.stderr.is_empty());
}

fn mock_provider() -> (String, thread::JoinHandle<String>) {
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
                    thread::sleep(Duration::from_millis(10))
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
        reader.read_exact(&mut vec![0; length]).unwrap();
        let response = concat!(
            "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
            "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
        headers
    });
    (endpoint, server)
}

#[test]
fn dotenv_api_key_env_reaches_provider_and_inherited_key_wins() {
    for inherited in [None, Some("inherited-api-key")] {
        let f = Fixture::new();
        let (endpoint, server) = mock_provider();
        f.config(&endpoint, &format!("api_key_env='{KEY}'"));
        f.dotenv(&format!("export {KEY}=\"dotenv-api-key\" # credential\n"));
        let mut command = f.command();
        command.args(["--non-interactive", "-p", "hello"]);
        if let Some(key) = inherited {
            command.env(KEY, key);
        }
        let out = output(&mut command);
        let headers = server.join().unwrap();
        assert_headless_success(&out);
        assert!(headers.to_ascii_lowercase().contains(&format!(
            "authorization: bearer {}\r\n",
            inherited.unwrap_or("dotenv-api-key")
        )));
    }
}

#[test]
fn dotenv_reaches_local_api_key_command() {
    let f = Fixture::new();
    let (endpoint, server) = mock_provider();
    f.config(
        &endpoint,
        "api_key_command = '''printf '%s' \"$SKYHOOK_DOTENV_TEST_KEY\"''' ",
    );
    f.dotenv(&format!("{KEY}=command-dotenv-api-key\n"));
    let out = output(&mut f.command().args(["--non-interactive", "-p", "hello"]));
    let headers = server.join().unwrap();
    assert_headless_success(&out);
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("authorization: bearer command-dotenv-api-key\r\n")
    );
}
