//! Security host behaviour observable only through the application: admission, runs kept
//! in their sessions, the host state on the wire, configuration dumps and native tools.
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use skyhook::identity::SessionId;

const SECURITY: &str = env!("CARGO_BIN_EXE_skyhook-security");

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            root: tempfile::tempdir().unwrap(),
        };
        fixture.config("http://127.0.0.1:1/v1");
        fixture
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn write(&self, name: &str, contents: &str) {
        let path = self.path(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn config(&self, endpoint: &str) {
        self.write(
            "config/skyhook/config.yaml",
            &format!(
                "default_model: test/first\nproviders:\n  test:\n    dialect: compatible\n    codec: chat_completions\n    base_url: '{endpoint}'\n    models:\n      first:\n        model: fixture-first\n        max_context: 128000\n        max_output: 4096\n        hint: UNASSIGNED_MODEL_HINT\n      reasoner:\n        model: fixture-reasoner\n        max_context: 128000\n        max_output: 4096\n      explorer:\n        model: fixture-explorer\n        max_context: 128000\n        max_output: 4096\n"
            ),
        );
    }

    fn command(&self, application: &str) -> Command {
        let mut command = Command::new(application);
        command
            .current_dir(self.root.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.path("config"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn batch(&self) -> Command {
        let mut command = self.command(SECURITY);
        command.arg("batch");
        command
    }

    fn script(&self, extra: &[&str]) -> Output {
        self.write("run.js", "return 1");
        output(
            self.batch()
                .arg("--script")
                .arg(self.path("run.js"))
                .args(extra),
        )
    }

    /// Every session's id; each security session is one run.
    fn runs(&self) -> Vec<String> {
        let directory = self.path(".skyhook/sessions");
        if !directory.exists() {
            return Vec::new();
        }
        let mut ids: Vec<_> = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        ids.sort();
        ids
    }

    fn no_history(&self) {
        assert!(!self.path(".skyhook/sessions").exists());
    }

    fn session_directory(&self, run: &str) -> PathBuf {
        self.path(&format!(".skyhook/sessions/{run}"))
    }

    /// The first column of each row `sql` reads from `run`'s session database.
    fn column(&self, run: &str, sql: &str) -> Vec<String> {
        block_on(async {
            let database =
                libsql::Builder::new_local(self.session_directory(run).join("session.db"))
                    .flags(libsql::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .build()
                    .await
                    .unwrap();
            let connection = database.connect().unwrap();
            let mut rows = connection.query(sql, ()).await.unwrap();
            let mut values = Vec::new();
            while let Some(row) = rows.next().await.unwrap() {
                values.push(row.get(0).unwrap());
            }
            values
        })
    }

    fn stats(&self, session: SessionId) -> Value {
        let result = output(self.command(SECURITY).args([
            "stats",
            &session.to_string(),
            "--format",
            "json",
        ]));
        assert_success(&result);
        serde_json::from_slice(&result.stdout).unwrap()
    }
}

fn block_on<T>(future: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

fn output(command: &mut Command) -> Output {
    let mut child = command.spawn().unwrap();
    // Drain both pipes while waiting so a diagnostic cannot fill one and stall the child.
    let drain = |mut pipe: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).unwrap();
            bytes
        })
    };
    let stdout = drain(Box::new(child.stdout.take().unwrap()));
    let stderr = drain(Box::new(child.stderr.take().unwrap()));
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("command did not terminate: {command:?}");
        }
        thread::sleep(Duration::from_millis(5));
    };
    Output {
        status,
        stdout: stdout.join().unwrap(),
        stderr: stderr.join().unwrap(),
    }
}

fn assert_success(result: &Output) {
    assert!(
        result.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

fn dumped_config(result: &Output) -> Value {
    assert_success(result);
    let yaml = std::str::from_utf8(&result.stdout).unwrap();
    skyhook::yaml::parse::<skyhook_cli::HostConfig>(yaml).unwrap();
    serde_saphyr::from_str(yaml).unwrap()
}

fn assert_admission_failure(result: &Output) {
    assert!(!result.status.success(), "{result:?}");
    assert!(result.stdout.is_empty(), "{result:?}");
    assert!(!result.stderr.is_empty());
}

fn session_id(result: &Output) -> SessionId {
    assert_success(result);
    std::str::from_utf8(&result.stdout)
        .unwrap()
        .strip_suffix('\n')
        .expect("session ID ends with a newline")
        .parse()
        .expect("stdout contains only one session ID")
}

/// One local streamed response; all socket waits are bounded and no credentials are used.
fn mock_provider() -> (String, thread::JoinHandle<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let (mut socket, _) = loop {
            match listener.accept() {
                Ok(socket) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "provider was never called");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("mock provider accept: {error}"),
            }
        };
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        let mut length = None;
        loop {
            let mut line = String::new();
            assert_ne!(reader.read_line(&mut line).unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            let lower = line.to_ascii_lowercase();
            assert!(!lower.starts_with("authorization:"));
            if let Some(value) = lower.strip_prefix("content-length:") {
                length = Some(value.trim().parse().unwrap());
            }
        }
        let mut body = vec![0; length.expect("request content length")];
        reader.read_exact(&mut body).unwrap();
        let response = concat!(
            "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"fixture-answer\"},\"finish_reason\":null}]}\n\n",
            "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
        serde_json::from_slice(&body).unwrap()
    });
    (endpoint, server)
}

/// The host section of the research state sent with the request.
fn host_state(request: &Value) -> String {
    fn text(value: &Value, strings: &mut String) {
        match value {
            Value::String(value) => {
                strings.push_str(value);
                strings.push('\n');
            }
            Value::Array(values) => {
                for value in values {
                    text(value, strings);
                }
            }
            Value::Object(values) => {
                for value in values.values() {
                    text(value, strings);
                }
            }
            _ => {}
        }
    }
    let mut messages = String::new();
    text(&request["messages"], &mut messages);
    let (_, state) = messages.split_once("host:\n").expect("research state");
    let (state, _) = state.split_once("</skyhook_state>").unwrap();
    state.to_owned()
}

#[test]
fn admission_fails_before_any_history_exists() {
    let f = Fixture::new();
    f.write("config/skyhook/security.yaml", "invalid [");
    for arguments in [
        vec!["--help"],
        vec!["batch", "--help"],
        vec!["auth", "--help"],
        vec!["dump", "skills"],
        vec!["stats", "--list"],
        vec!["runs"],
    ] {
        assert_success(&output(f.command(SECURITY).args(arguments)));
        f.no_history();
    }
    assert_admission_failure(&output(f.command(SECURITY).args(["dump", "config"])));
    f.no_history();

    f.write(
        ".skyhook/security.yaml",
        "models:\n  test/missing:\n    roles: [orchestrator]\n",
    );
    assert_admission_failure(&output(f.command(SECURITY).args(["dump", "config"])));
    assert_admission_failure(&f.script(&[]));
    fs::remove_file(f.path(".skyhook/security.yaml")).unwrap();
    f.write("config/skyhook/security.yaml", "poc: ask\n");
    assert_admission_failure(&f.script(&[]));
    for arguments in [vec!["--poc", "ask"], vec!["--mode", "orchestrate"]] {
        assert_admission_failure(&output(f.batch().args(["-p", "x"]).args(arguments)));
    }
    f.no_history();
}

#[test]
fn each_session_is_its_own_run_and_a_resume_continues_it() {
    let f = Fixture::new();
    let first = session_id(&f.script(&[]));
    let first_run = first.to_string();
    assert_eq!(f.runs(), std::slice::from_ref(&first_run));
    assert_eq!(f.stats(first)["host"], "skyhook-security");

    let second = session_id(&f.script(&[]));
    assert_ne!(first, second);
    let runs = f.runs();
    assert_eq!(runs.len(), 2);

    assert_eq!(
        session_id(&f.script(&["--resume", &first.to_string()])),
        first
    );
    assert_eq!(f.runs(), runs);

    let listed = output(f.command(SECURITY).arg("runs"));
    assert_success(&listed);
    let text = String::from_utf8(listed.stdout).unwrap();
    for id in &runs {
        assert!(text.contains(id.as_str()), "{text}");
    }
    assert_success(&output(f.command(SECURITY).args(["runs", &first_run])));
}

#[test]
fn a_resumed_run_keeps_its_threshold_and_workers_unless_a_flag_changes_them() {
    let f = Fixture::new();
    f.write(
        "config/skyhook/security.yaml",
        "shortlist: high\nworkers: 3\n",
    );
    let first = session_id(&f.script(&["--workers", "2", "--shortlist", "medium"]));
    let run = first.to_string();
    assert_eq!(f.column(&run, "SELECT threshold FROM run"), ["medium"]);
    let workers_sql = "SELECT CAST(workers AS TEXT) FROM run";
    assert_eq!(f.column(&run, workers_sql), ["2"]);

    // Later configuration does not resize the run; a flag on a later launch does.
    f.write(
        "config/skyhook/security.yaml",
        "shortlist: critical\nworkers: 9\n",
    );
    assert_eq!(session_id(&f.script(&["--resume", &run])), first);
    assert_eq!(f.column(&run, "SELECT threshold FROM run"), ["medium"]);
    assert_eq!(f.runs(), std::slice::from_ref(&run));
    assert_eq!(f.column(&run, workers_sql), ["2"]);
    session_id(&f.script(&["--resume", &run, "--workers", "4"]));
    assert_eq!(f.column(&run, workers_sql), ["4"]);
}

#[test]
fn a_run_sends_host_state_with_layered_roles() {
    let f = Fixture::new();
    let (endpoint, provider) = mock_provider();
    f.config(&endpoint);
    // The shipped example is the user layer; the workspace layer overrides whole entries.
    let example = include_str!("../../../security.example.yaml")
        .replace("main/reasoner", "test/reasoner")
        .replace("main/reviewer", "test/first")
        .replace("local/coder", "test/explorer");
    f.write("config/skyhook/security.yaml", &example);
    f.write(
        ".skyhook/security.yaml",
        "models:\n  test/reasoner:\n    roles: [orchestrator, validator]\n    hint: WORKSPACE_ROLE_HINT\nshortlist: high\n",
    );
    let result = output(f.batch().args(["--prompt", "Start.", "--poc", "yes"]));
    let request = provider.join().unwrap();
    let session = session_id(&result);
    let run = session.to_string();
    assert_eq!(request["model"], "fixture-reasoner");
    let state = host_state(&request);
    assert!(state.contains(&format!("run: {run}\n")));
    assert!(state.contains("poc: yes\n"));
    assert_eq!(f.column(&run, "SELECT threshold FROM run"), ["high"]);
    assert!(!state.contains("drift:"));
    let encoded = request.to_string();
    assert!(encoded.contains("WORKSPACE_ROLE_HINT"));
    assert!(!encoded.contains("UNASSIGNED_MODEL_HINT"));
}

#[test]
fn dump_config_layers_security_files() {
    let f = Fixture::new();
    let mut main = fs::read_to_string(f.path("config/skyhook/config.yaml")).unwrap();
    main.push_str("mcp:\n  main:\n    transport: stdio\n    start_command: [/bin/true]\n");
    f.write("config/skyhook/config.yaml", &main);
    f.write(
        "config/skyhook/security.yaml",
        "models:\n  test/reasoner:\n    roles: [orchestrator]\n    hint: USER_ROLE_HINT\n  test/explorer:\n    roles: [explorer]\n    hint: EXPLORER_ROLE_HINT\nmodes:\n  report:\n    capabilities: [read]\n    instructions: USER_REPORT_INSTRUCTIONS\n    hint: USER_REPORT_HINT\n",
    );
    f.write(
        ".skyhook/security.yaml",
        "models:\n  test/reasoner:\n    roles: [orchestrator, validator]\n    hint: WORKSPACE_ROLE_HINT\npoc: ask\nmodes:\n  report:\n    capabilities: [read, write, security.verdict]\n    instructions: WORKSPACE_REPORT_INSTRUCTIONS\nmcp:\n  security:\n    transport: stdio\n    start_command: [/bin/true]\n",
    );
    let config = dumped_config(&output(f.command(SECURITY).args(["dump", "config"])));
    // Both security layers apply, the workspace's replacing whole entries.
    assert_eq!(config["default_model"], "test/reasoner");
    let models = &config["providers"]["test"]["models"];
    let hint = |model: &str| {
        models[model]["hint"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };
    assert!(hint("reasoner").ends_with("WORKSPACE_ROLE_HINT"));
    assert!(hint("explorer").ends_with("EXPLORER_ROLE_HINT"));
    let report = &config["modes"]["report"];
    assert_eq!(report["instructions"], "WORKSPACE_REPORT_INSTRUCTIONS");
    assert!(report["hint"].is_null());
    assert_eq!(
        report["capabilities"],
        serde_json::json!(["read", "write", "security.verdict"])
    );
    assert!(config["mcp"]["main"].is_object());
    assert!(config["mcp"]["security"].is_object());
    f.no_history();

    // An explicit security file disables security discovery only; the main layering stays.
    f.write(
        ".skyhook/config.yaml",
        &fs::read_to_string(f.path("config/skyhook/config.yaml"))
            .unwrap()
            .replace("fixture-", "workspace-"),
    );
    f.write(
        "selected-security.yaml",
        "models:\n  test/explorer:\n    roles: [orchestrator]\n    hint: EXPLICIT_ROLE_HINT\npoc: no\n",
    );
    let config = dumped_config(&output(
        f.command(SECURITY)
            .args(["dump", "config", "--security-config"])
            .arg(f.path("selected-security.yaml")),
    ));
    assert_eq!(
        config["providers"]["test"]["models"]["explorer"]["model"],
        "workspace-explorer"
    );
    let explorer = &config["providers"]["test"]["models"]["explorer"]["hint"];
    assert!(explorer.as_str().unwrap().ends_with("EXPLICIT_ROLE_HINT"));
    assert!(config["providers"]["test"]["models"]["reasoner"]["hint"].is_null());
    assert_eq!(config["default_model"], "test/explorer");
    assert!(config["mcp"]["security"].is_null());
    f.no_history();
}

const CANDIDATE_SOURCE: &str = "int parse(unsigned len) {\n  char buffer[4] = {0};\n  if (len > sizeof buffer) {\n    return buffer[len];\n  }\n  return 0;\n}\n";

/// Records one scope and one validated candidate, and leaves `flaw` bound for the caller.
const VALIDATED_CANDIDATE: &str = r#"
const research = async action => (await tool.research(action)).unwrap();
await research({action:'add_scope',description:'first-run-scope'});
const at = line => ({file:'fixture.c',start_line:line,end_line:line});
const source = await research({action:'add_source',kind:'network',trust:'unauthenticated',entry:at(1),description:'Untrusted request length in the assumed deployment'});
const component = await research({action:'add_component',name:'parser',description:'fixture parser',files:['fixture.c']});
const path = await research({action:'add_path',description:'request length parsing',sources:[source],reaches:[{component,note:'parse is the entry point'}]});
const flaw = await research({action:'add_flaw',component,title:'Out-of-bounds read',statement:'An oversized length indexes past the buffer',operation:'memory_read',deficiency:{guard:'bounds_check',deficiency:'missing'},effects:[{property:'confidentiality',note:'adjacent memory'}],evidence:[{location:at(4),role:'operation',stance:'supports',note:'Read beyond the allocation'}]});
await research({action:'add_exposure',flaw,path,outcome:'contributes',conditions:'len exceeds the buffer',note:'Oversized request selects the unsafe branch',route:[{location:at(3),basis:'inspected',note:'the length check admits it'}]});
const candidate = await research({action:'add_candidate',title:'Out-of-bounds read',description:'Oversized length reaches an unsafe read',cwe:125,members:[{flaw,step:null}],paths:[path]});
await research({action:'assess',candidate,vector:'CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:N/VA:H/SC:N/SI:N/SA:N',confidence:'established',rationale:'Remote unauthenticated disclosure and process failure in the assumed deployment'});
await research({action:'verdict',candidate,verdict:{kind:'validated'},rationale:'Reviewed the guard, the route and the read'});
"#;

#[test]
fn native_tools_report_into_and_scope_to_their_session_run() {
    let f = Fixture::new();
    f.write("fixture.c", CANDIDATE_SOURCE);
    let content = serde_json::json!({
        "title": "Bounds-check fixture",
        "summary": "Analytically reviewed; no empirical demonstration.",
        "analysis": "The recorded len > capacity guard reaches the out-of-bounds read.",
        "impact": "The assumed deployment exposes adjacent memory to an unauthenticated reader.",
        "remediation": "Reject indexes outside the buffer length before access.",
        "reproduction": {
            "prerequisites": ["Use only the disposable local fixture."],
            "steps": [{
                "instruction": "Review fixture.c at the guarded buffer access.",
                "expected_result": "The selected branch permits an index beyond the buffer."
            }],
            "outcome": {
                "kind": "unobserved",
                "expected_result": "An unsafe read under the recorded preconditions.",
                "limitations": "Code review only; the fixture was not executed."
            },
            "cleanup": ["No live environment was created."]
        }
    });
    f.write(
        "report.js",
        &format!("{VALIDATED_CANDIDATE}\n(await tool.finding_report({{action:'populate', ...{content}, root: flaw}})).unwrap();\nreturn (await tool.finding_report({{action:'generate'}})).unwrap();"),
    );
    let first = session_id(&output(
        f.batch()
            .args(["--approve-all", "-s"])
            .arg(f.path("report.js")),
    ));
    let first_run = first.to_string();
    let root = f
        .column(&first_run, "SELECT CAST(root AS TEXT) FROM finding")
        .pop()
        .unwrap();
    // Reports are the session's own files.
    let report = f
        .session_directory(&first_run)
        .join(format!("reports/{root}"));
    let document: Value =
        serde_json::from_slice(&fs::read(report.join("finding.json")).unwrap()).unwrap();
    assert_eq!(document["run"], first_run);

    // Native tools are scoped to the session's run: a new session sees no scopes, a
    // resume sees its own, and a resume cannot narrow the fixed root's capabilities.
    f.write(
        "query.js",
        "return (await tool.research_query({action:'scopes'})).unwrap();",
    );
    let query = |extra: &[&str]| {
        output(
            f.batch()
                .args(["--approve-all", "-s"])
                .arg(f.path("query.js"))
                .args(extra),
        )
    };
    let second = session_id(&query(&[]));
    assert_ne!(first, second);
    let runs = f.runs();
    assert_eq!(runs.len(), 2);
    assert!(
        f.column(&second.to_string(), "SELECT description FROM scope")
            .is_empty()
    );
    assert_eq!(session_id(&query(&["--resume", &first.to_string()])), first);
    assert_admission_failure(&query(&[
        "--resume",
        &first.to_string(),
        "--capabilities",
        "read",
    ]));
    assert_eq!(
        f.column(&first_run, "SELECT description FROM scope"),
        ["first-run-scope"]
    );
    assert_eq!(f.runs(), runs);
}
