mod dotenv;
mod embedded_shims;
mod headless;
mod interaction;
mod launch;
mod tui;
use clap::{Parser, Subcommand, ValueEnum};
use skyhook::{identity::SessionId, tool::policy::Capability};
use std::path::PathBuf;
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(name = "skyhook", version, about = "Programmable coding-agent harness")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// Explicit TOML config used instead of the user config.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Workspace visible to coding tools.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Resume an existing session id.
    #[arg(long)]
    resume: Option<SessionId>,
    /// Select and remember the root model for a new session.
    #[arg(short = 'm', long = "model")]
    model: Option<String>,
    /// Approve every tool invocation without prompting.
    #[arg(long)]
    approve_all: bool,
    /// Run without terminal interaction; requires --prompt or --script.
    #[arg(long, requires = "input")]
    non_interactive: bool,
    /// Exact comma-separated policy capabilities; interaction follows the runtime mode.
    #[arg(long, value_name = "LIST", value_parser = parse_capabilities)]
    capabilities: Option<Capabilities>,
    /// Attach images to the first prompt.
    #[arg(long = "image", requires = "prompt", conflicts_with = "script")]
    images: Vec<PathBuf>,
    /// Submit this initial prompt.
    #[arg(short, long, conflicts_with = "script", group = "input")]
    prompt: Option<String>,
    /// Run this JavaScript workflow.
    #[arg(
        short,
        long,
        value_name = "PATH",
        conflicts_with = "prompt",
        group = "input"
    )]
    script: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct Capabilities(Vec<Capability>);

fn parse_capabilities(value: &str) -> Result<Capabilities, String> {
    if value.is_empty() {
        return Ok(Capabilities(Vec::new()));
    }
    value
        .split(',')
        .map(|name| {
            let capability = name.trim().parse::<Capability>().map_err(|error| error.to_string())?;
            if capability == Capability::Interactive {
                return Err("interactive is controlled by the runtime mode; use --non-interactive to disable it".into());
            }
            Ok(capability)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Capabilities)
}

#[derive(Subcommand)]
enum Command {
    /// Manage Skyhook-owned provider credentials.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
}
#[derive(Clone, Copy, ValueEnum)]
enum AuthProvider {
    Codex,
}
#[derive(Subcommand)]
enum AuthCommand {
    /// Sign in using a browser, or the public device flow on a headless machine.
    Login {
        #[arg(value_enum, default_value = "codex")]
        provider: AuthProvider,
        #[arg(long)]
        headless: bool,
    },
    /// Inspect Skyhook's credentials without displaying tokens.
    Status {
        #[arg(value_enum, default_value = "codex")]
        provider: AuthProvider,
    },
    /// Delete only Skyhook's locally stored credentials.
    Logout {
        #[arg(value_enum, default_value = "codex")]
        provider: AuthProvider,
    },
}
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
    let silent = cli
        .iter()
        .skip(1)
        .take_while(|arg| arg.as_os_str() != "--")
        .any(|arg| {
            arg == "--non-interactive"
                || arg
                    .to_str()
                    .is_some_and(|arg| arg.starts_with("--non-interactive="))
        });
    let args = match Args::try_parse_from(cli) {
        Ok(args) => args,
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
    if args.non_interactive && args.command.is_some() {
        std::process::exit(2);
    }
    // SAFETY: startup is still single-threaded: no Tokio runtime, terminal,
    // tracing subscriber, provider, or background worker has been started.
    // Parse Clap first so help/version do not depend on a valid .env file.
    if let Err(error) = unsafe { dotenv::load_invocation_env() } {
        if !args.non_interactive {
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
            if !args.non_interactive {
                eprintln!("skyhook: could not start async runtime");
            }
            std::process::exit(1);
        }
    };
    runtime.block_on(run(args));
}

async fn run(mut args: Args) {
    if let Some(Command::Auth { command }) = args.command.take() {
        if let Err(error) = run_auth(command).await {
            eprintln!("skyhook auth: {error}");
            std::process::exit(1);
        }
        return;
    }
    if args.non_interactive {
        // Do not install any terminal, renderer, or tracing subscriber here.
        if headless::run(args).await.is_err() {
            std::process::exit(1);
        }
        return;
    }
    if let Err(error) = tui::run(args).await {
        eprintln!("skyhook: {error}");
        std::process::exit(1);
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
    #[test]
    fn auth_commands_parse_without_starting_tui() {
        assert!(matches!(
            Args::try_parse_from(["skyhook", "auth", "login", "--headless"])
                .unwrap()
                .command,
            Some(Command::Auth {
                command: AuthCommand::Login { headless: true, .. }
            })
        ));
        assert!(matches!(
            Args::try_parse_from(["skyhook", "auth", "status", "codex"])
                .unwrap()
                .command,
            Some(Command::Auth {
                command: AuthCommand::Status { .. }
            })
        ));
        assert!(matches!(
            Args::try_parse_from(["skyhook", "auth", "logout"])
                .unwrap()
                .command,
            Some(Command::Auth {
                command: AuthCommand::Logout { .. }
            })
        ));
        assert!(
            Args::try_parse_from(["skyhook", "--prompt", "hello"])
                .unwrap()
                .command
                .is_none()
        );
    }

    #[test]
    fn headless_requires_exactly_one_input() {
        assert!(Args::try_parse_from(["skyhook", "--non-interactive"]).is_err());
        assert!(
            Args::try_parse_from(["skyhook", "--non-interactive", "-p", "hello"])
                .unwrap()
                .non_interactive
        );
        assert!(Args::try_parse_from(["skyhook", "--non-interactive", "-s", "run.js"]).is_ok());
        assert!(
            Args::try_parse_from([
                "skyhook",
                "--non-interactive",
                "-p",
                "hello",
                "-s",
                "run.js"
            ])
            .is_err()
        );
    }

    #[test]
    fn capability_allowlist_accepts_empty_and_rejects_unknown_names() {
        let empty = Args::try_parse_from(["skyhook", "--capabilities="]).unwrap();
        assert!(empty.capabilities.unwrap().0.is_empty());
        let selected =
            Args::try_parse_from(["skyhook", "--capabilities", "read,exec,targets"]).unwrap();
        assert_eq!(
            selected.capabilities.unwrap().0,
            vec![Capability::Read, Capability::Exec, Capability::Targets]
        );
        assert!(Args::try_parse_from(["skyhook", "--capabilities", "read,typo"]).is_err());
        assert!(Args::try_parse_from(["skyhook", "--capabilities", "read,"]).is_err());
    }
    // Unit tests do not receive CARGO_BIN_EXE. Build the real CLI explicitly once,
    // and use Cargo's artifact message rather than guessing a profile/target path.
    fn cli_binary() -> &'static std::path::Path {
        static BINARY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        BINARY.get_or_init(|| {
            let features = if cfg!(feature = "embed-shims") {
                "tui,embed-shims"
            } else {
                "tui"
            };
            let output = ProcessCommand::new(env!("CARGO"))
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .args([
                    "build",
                    "--locked",
                    "--manifest-path",
                    concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
                    "--bin",
                    "skyhook",
                    "--no-default-features",
                    "--features",
                    features,
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
            // Bad terminal settings must not prevent headless execution.
            fs::create_dir_all(fixture.path("config/skyhook")).unwrap();
            fs::write(fixture.path("config/skyhook/tui.toml"), "not valid TOML [").unwrap();
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
                .env("XDG_STATE_HOME", self.path("state"))
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
        pub(crate) fn artifacts(&self, output: &Output) -> String {
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
        pub(crate) fn journal(&self, output: &Output) -> String {
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
    fn parse_and_early_runtime_failures_are_silent() {
        let f = Fixture::new();
        for args in [
            vec!["--non-interactive", "--unknown"],
            vec!["--non-interactive=true", "-p", "x"],
            vec!["--non-interactive", "-p", "x", "-m", "missing"],
        ] {
            let out = f.command().args(&args).output().unwrap();
            assert!(!out.status.success(), "{args:?}");
            assert!(out.stdout.is_empty(), "{args:?}: {:?}", out.stdout);
            assert!(out.stderr.is_empty(), "{args:?}: {:?}", out.stderr);
        }
        fs::write(f.path("config/skyhook/config.toml"), "invalid [").unwrap();
        let out = f
            .command()
            .args(["--non-interactive", "-p", "x"])
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(out.stdout.is_empty() && out.stderr.is_empty());
    }

    #[test]
    fn terminal_mode_still_rejects_redirected_io() {
        let f = Fixture::new();
        let out = f.command().args(["-p", "hello"]).output().unwrap();
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
        assert!(String::from_utf8_lossy(&out.stderr).contains("interactive terminal"));
    }
}
