mod cli;
mod dotenv;
mod dump;
mod embedded_shims;
mod headless;
mod interaction;
mod launch;
mod tui;
use cli::{AuthCommand, AuthProvider, Invocation};
#[cfg(test)]
use std::path::PathBuf;
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

async fn run_auth(
    command: AuthCommand,
) -> Result<(), skyhook::provider::backends::codex::auth::AuthError> {
    use skyhook::provider::backends::codex::auth;
    match command {
        AuthCommand::Login {
            provider: AuthProvider::Codex,
            headless,
        } => {
            auth::login(headless).await?;
            println!("Signed in to Codex for Skyhook.");
        }
        AuthCommand::Status {
            provider: AuthProvider::Codex,
        } => match auth::status().await? {
            auth::AuthStatus::LoggedOut => {
                println!("Codex: not signed in to Skyhook. Run skyhook auth login.")
            }
            auth::AuthStatus::LoggedIn { expires_at, .. } => println!(
                "Codex: Skyhook credentials present (access token expires at Unix time {expires_at}; refreshed automatically when needed)."
            ),
        },
        AuthCommand::Logout {
            provider: AuthProvider::Codex,
        } => {
            auth::logout().await?;
            println!(
                "Removed Skyhook's Codex credentials. Other applications' credentials were not changed."
            );
        }
    }
    Ok(())
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--askpass") {
        let Some(socket) = std::env::var_os("SKYHOOK_ASKPASS_SOCKET") else {
            std::process::exit(1)
        };
        let prompt = std::env::args().nth(2).unwrap_or_default();
        if let Err(error) =
            skyhook::remote::run_askpass_helper(std::path::Path::new(&socket), prompt)
        {
            eprintln!("skyhook askpass: {error}");
            std::process::exit(1);
        }
        return;
    }
    // Clap normally prints parse errors. Machine-mode failures are silent even
    // before a session exists; explicit help/version retain their normal output.
    let cli: Vec<_> = std::env::args_os().collect();
    let dumping = cli
        .iter()
        .skip(1)
        .take_while(|arg| arg.as_os_str() != "--")
        .any(|arg| arg == "--dump" || arg.to_str().is_some_and(|arg| arg.starts_with("--dump=")));
    let silent = !dumping
        && cli
            .iter()
            .skip(1)
            .take_while(|arg| arg.as_os_str() != "--")
            .any(|arg| {
                arg == "--non-interactive"
                    || arg
                        .to_str()
                        .is_some_and(|arg| arg.starts_with("--non-interactive="))
            });
    let invocation = match cli::parse_from(cli) {
        Ok(invocation) => invocation,
        Err(error) => {
            if !silent
                || matches!(
                    error.kind(),
                    clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
                )
            {
                error.exit();
            }
            std::process::exit(2);
        }
    };
    // SAFETY: startup is still single-threaded: no Tokio runtime, terminal,
    // tracing subscriber, provider, or background worker has been started.
    // Parse Clap first so help/version do not depend on a valid .env file.
    if let Err(error) = unsafe { dotenv::load_invocation_env() } {
        if !silent {
            eprintln!("skyhook: {error}");
        }
        std::process::exit(1);
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            if !silent {
                eprintln!("skyhook: could not start async runtime");
            }
            std::process::exit(1);
        }
    };
    runtime.block_on(run(invocation));
}

async fn run(invocation: Invocation) {
    match invocation {
        Invocation::Inspect(request) => {
            if let Err(error) = dump::run(request).await {
                eprintln!("skyhook dump: {}", dump::diagnostic_text(error));
                std::process::exit(1);
            }
        }
        Invocation::Auth(command) => {
            if let Err(error) = run_auth(command).await {
                eprintln!("skyhook auth: {error}");
                std::process::exit(1);
            }
        }
        Invocation::Headless(request, input) => {
            // Do not install any terminal, renderer, or tracing subscriber here.
            if headless::run(request, input).await.is_err() {
                std::process::exit(1);
            }
        }
        Invocation::Interactive(request, input) => {
            if let Err(error) = tui::run(request, input).await {
                eprintln!("skyhook: {error}");
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        process::{Child, Command as ProcessCommand, Output, Stdio},
        thread,
        time::{Duration, Instant},
    };
    // Unit tests do not receive CARGO_BIN_EXE. Build the real CLI explicitly once,
    // and use Cargo's artifact message rather than guessing a profile/target path.
    fn cli_binary() -> &'static std::path::Path {
        static BINARY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        BINARY.get_or_init(|| {
            let output = ProcessCommand::new(env!("CARGO"))
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .args([
                    "build",
                    "--locked",
                    "--manifest-path",
                    concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
                    "--bin",
                    "skyhook",
                    "--features",
                    "tui",
                    "--message-format=json",
                ])
                .output()
                .expect("build CLI fixture");
            assert!(
                output.status.success(),
                "CLI fixture build failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .find_map(|message| {
                    (message["reason"] == "compiler-artifact"
                        && message["target"]["name"] == "skyhook")
                        .then(|| message["executable"].as_str().map(PathBuf::from))
                        .flatten()
                })
                .expect("Cargo did not report the skyhook executable")
        })
    }
    pub(crate) struct Fixture {
        pub(crate) root: tempfile::TempDir,
        pub(crate) cwd: PathBuf,
    }
    impl Fixture {
        pub(crate) fn new() -> Self {
            // Finish compilation before starting provider/signal test deadlines.
            cli_binary();
            let root = tempfile::tempdir().unwrap();
            let fixture = Self {
                cwd: root.path().to_owned(),
                root,
            };
            fs::create_dir_all(fixture.path("config/skyhook")).unwrap();
            fixture.config("http://127.0.0.1:1/v1", "");
            fixture
        }
        pub(crate) fn path(&self, name: &str) -> PathBuf {
            self.root.path().join(name)
        }
        pub(crate) fn config(&self, endpoint: &str, top: &str) {
            self.provider_config(endpoint, top, "");
        }
        pub(crate) fn provider_config(&self, endpoint: &str, top: &str, credential: &str) {
            fs::write(self.path("config/skyhook/config.toml"), format!(
            "{top}\n[providers.test]\nkind='openai'\napi='chat_completions'\nbase_url='{endpoint}'\n{credential}\n[models.first]\nprovider='test'\nmodel='fixture'\nmax_context=128000\nmax_output=4096\nsupports_images=true\n"
        )).unwrap();
        }
        pub(crate) fn bare_command(&self) -> ProcessCommand {
            let mut cmd = ProcessCommand::new(cli_binary());
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
        pub(crate) fn command(&self) -> ProcessCommand {
            let mut cmd = self.bare_command();
            cmd.arg("--config")
                .arg(self.path("config/skyhook/config.toml"))
                .arg("--workspace")
                .arg(self.root.path());
            cmd
        }
        pub(crate) fn script(&self, source: &str, extra: &[&str]) -> Output {
            fs::write(self.path("run.js"), source).unwrap();
            output(
                self.command()
                    .args(["--non-interactive", "-s"])
                    .arg(self.path("run.js"))
                    .args(extra),
            )
        }
        /// Saved job outputs and captures as text.
        pub(crate) fn artifacts(&self, output: &Output) -> String {
            let id = std::str::from_utf8(&output.stdout)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let sessions = self.path(".skyhook/sessions");
            on_thread(async move {
                let text = skyhook::session::SessionStore::read_output_text(&sessions, id).await;
                text.unwrap().concat()
            })
        }
        /// Records as JSON lines.
        pub(crate) fn journal(&self, output: &Output) -> String {
            let records = self.records(output);
            let lines = records
                .iter()
                .map(|record| serde_json::to_string(record).unwrap());
            lines.map(|line| line + "\n").collect()
        }
        /// The committed records of the session whose id `output` printed.
        pub(crate) fn records(&self, output: &Output) -> Vec<skyhook::session::EventRecord> {
            assert!(
                output.stderr.is_empty(),
                "stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = std::str::from_utf8(&output.stdout).unwrap();
            let id = stdout
                .strip_suffix('\n')
                .expect("session id ends with newline");
            let id: skyhook::identity::SessionId =
                id.parse().expect("stdout contains only one session ID");
            let sessions = self.path(".skyhook/sessions");
            on_thread(async move {
                let records = skyhook::session::SessionStore::read_records(&sessions, id).await;
                records.unwrap()
            })
        }
    }

    /// Run `future` on its own thread's runtime, outside any test runtime.
    fn on_thread<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> T {
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(future)
        })
        .join()
        .unwrap()
    }

    pub(crate) fn output(command: &mut ProcessCommand) -> Output {
        RunningHeadless(Some(command.spawn().unwrap())).finish()
    }

    pub(crate) fn wait_bounded(child: &mut Child) {
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

    /// Keep cleanup reliable even if a socket/permission assertion fails mid-run.
    pub(crate) struct RunningHeadless(pub(crate) Option<Child>);
    impl RunningHeadless {
        pub(crate) fn finish(mut self) -> Output {
            wait_bounded(self.0.as_mut().unwrap());
            self.0.take().unwrap().wait_with_output().unwrap()
        }
    }
    impl Drop for RunningHeadless {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = ProcessCommand::new("kill")
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

    pub(crate) fn mock_provider() -> (String, thread::JoinHandle<(String, String)>) {
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
        let run = |args: &[&str]| f.command().args(args).output().unwrap();
        for args in [
            &["--non-interactive", "--unknown"][..],
            &["--non-interactive=true", "-p", "x"],
            &["--non-interactive", "-p", "x", "-m", "missing"],
        ] {
            let out = run(args);
            assert!(!out.status.success(), "{args:?}");
            assert!(
                out.stdout.is_empty() && out.stderr.is_empty(),
                "{args:?}: {out:?}"
            );
        }
        // Terminal mode still rejects redirected IO.
        let out = run(&["-p", "hello"]);
        assert!(!out.status.success() && out.stdout.is_empty());
        assert!(String::from_utf8_lossy(&out.stderr).contains("interactive terminal"));
        fs::write(f.path("config/skyhook/config.toml"), "invalid [").unwrap();
        let out = run(&["--non-interactive", "-p", "x"]);
        assert!(!out.status.success());
        assert!(out.stdout.is_empty() && out.stderr.is_empty());
    }
}
