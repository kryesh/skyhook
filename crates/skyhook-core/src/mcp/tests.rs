//! End-to-end manager tests using a small, dependency-free MCP subprocess.
//!
//! Included by manager.rs with `#[cfg(test)] #[path = "tests.rs"] mod tests;`.

use super::super::config::{McpServerConfig, McpTransport};
use super::*;
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, path::Path, time::Duration};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const FIXTURE: &str = r#"
import json, os, sys, threading, time

with open(os.environ['MCP_TEST_PID'] + '.tmp', 'w') as f:
    f.write(str(os.getpid()))
os.replace(os.environ['MCP_TEST_PID'] + '.tmp', os.environ['MCP_TEST_PID'])
lock = threading.Lock()
def send(message):
    with lock:
        print(json.dumps(message), flush=True)
def tool(name):
    return {'name': name, 'description': 'fixture ' + name,
            'inputSchema': {'type': 'object', 'additionalProperties': True}}
def handle(request):
    if request.get('method') == 'notifications/cancelled':
        with open(os.environ['MCP_TEST_CANCELLED'], 'w') as f:
            f.write(json.dumps(request['params']))
    if 'id' not in request:
        return
    ident = request['id']
    method = request['method']
    params = request.get('params', {})
    if method == 'initialize':
        with open(os.environ['MCP_TEST_INITIALIZE'], 'w') as f:
            f.write(json.dumps(params))
        if os.environ.get('MCP_TEST_OVERSIZED_FRAME'):
            sys.stdout.write('x' * (8 * 1024 * 1024 + 1))
            sys.stdout.flush()
            return
        if os.environ.get('MCP_TEST_HANG_INITIALIZE') == '1':
            time.sleep(60)
        result = {'protocolVersion': params['protocolVersion'],
                  'capabilities': ({'resources': {}} if os.environ.get('MCP_TEST_NO_TOOLS') else {'tools': {}}),
                  'serverInfo': {'name': 'skyhook-test', 'version': '1'}}
    elif method == 'tools/list':
        with open(os.environ['MCP_TEST_LIST'], 'w') as f:
            f.write('called')
        mode = os.environ.get('MCP_TEST_DISCOVERY')
        if mode == 'oversized_schema':
            result = {'tools': [dict(tool('huge'), description='x' * (256 * 1024))]}
            send({'jsonrpc': '2.0', 'id': ident, 'result': result})
            return
        if mode == 'many_tools':
            result = {'tools': [tool('tool_' + str(i)) for i in range(1025)]}
            send({'jsonrpc': '2.0', 'id': ident, 'result': result})
            return
        if mode == 'repeated_cursor':
            result = {'tools': [], 'nextCursor': 'repeat'}
            send({'jsonrpc': '2.0', 'id': ident, 'result': result})
            return
        cursor = params.get('cursor')
        if cursor is None:
            result = {'tools': [tool('echo')], 'nextCursor': 'second-page'}
        elif cursor == 'second-page':
            result = {'tools': [tool('tool_error'), tool('rpc_error'), tool('slow')]}
        else:
            send({'jsonrpc': '2.0', 'id': ident,
                  'error': {'code': -32602, 'message': 'unexpected cursor'}})
            return
    elif method == 'tools/call':
        name = params['name']
        if os.environ.get('MCP_TEST_LARGE_RESULT') == '1':
            send({'jsonrpc': '2.0', 'id': ident, 'result': {'content': [{'type': 'text', 'text': 'x' * (4 * 1024 * 1024)}]}})
            return
        if name == 'rpc_error':
            send({'jsonrpc': '2.0', 'id': ident,
                  'error': {'code': -32602, 'message': 'fixture protocol error'}})
            return
        if name == 'slow':
            with open(os.environ['MCP_TEST_STARTED'], 'w') as f:
                f.write('started')
            time.sleep(60)
        if name == 'tool_error':
            result = {'content': [{'type': 'text', 'text': 'fixture tool error'}],
                      'isError': True}
        else:
            payload = {'arguments': params.get('arguments', {}),
                       'cwd': os.getcwd(), 'env': os.environ.get('MCP_TEST_VALUE')}
            result = {'content': [{'type': 'text', 'text': json.dumps(payload)}],
                      'isError': False}
    elif method == 'ping':
        result = {}
    else:
        send({'jsonrpc': '2.0', 'id': ident,
              'error': {'code': -32601, 'message': 'unknown method'}})
        return
    send({'jsonrpc': '2.0', 'id': ident, 'result': result})

# Daemon request workers let a cancelled call remain pending without preventing
# subsequent requests, or keeping the process alive once the client closes stdin.
for line in sys.stdin:
    threading.Thread(target=handle, args=(json.loads(line),), daemon=True).start()
"#;

struct Fixture {
    directory: TempDir,
}

impl Fixture {
    fn new() -> Option<Self> {
        match std::process::Command::new("python3")
            .arg("--version")
            .output()
        {
            Ok(output) if output.status.success() => Some(Self {
                directory: tempfile::tempdir().expect("fixture directory"),
            }),
            _ => {
                eprintln!("skipping MCP subprocess test: python3 is unavailable");
                None
            }
        }
    }

    fn config(&self) -> McpServerConfig {
        McpServerConfig {
            transport: McpTransport::Stdio,
            start_command: Some(vec![
                "python3".into(),
                "-u".into(),
                "-c".into(),
                FIXTURE.into(),
            ]),
            url: None,
            capabilities: vec![],
            startup_timeout_secs: 5,
            call_timeout_secs: 5,
            cwd: Some(self.directory.path().to_owned()),
            env: BTreeMap::from([
                ("MCP_TEST_PID".into(), self.path("pid")),
                ("MCP_TEST_STARTED".into(), self.path("started")),
                ("MCP_TEST_CANCELLED".into(), self.path("cancelled")),
                ("MCP_TEST_INITIALIZE".into(), self.path("initialize")),
                ("MCP_TEST_LIST".into(), self.path("list")),
                ("MCP_TEST_VALUE".into(), "configured child value".into()),
            ]),
            headers_env: BTreeMap::new(),
        }
    }

    fn path(&self, name: &str) -> String {
        self.directory
            .path()
            .join(name)
            .to_str()
            .unwrap()
            .to_owned()
    }

    #[cfg(unix)]
    fn pid(&self) -> libc::pid_t {
        std::fs::read_to_string(self.path("pid"))
            .expect("fixture wrote its pid")
            .parse()
            .expect("fixture pid is numeric")
    }
}

// Process handles (manager-owned or explicit test Children) have kill-on-drop
// fallbacks. Do not kill by saved PID here: a successfully reaped PID can be
// reused by an unrelated process before the fixture directory is dropped.

async fn connect(config: McpServerConfig) -> McpManager {
    tokio::time::timeout(
        Duration::from_secs(10),
        McpManager::connect(
            &BTreeMap::from([("fixture".into(), config)]),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("MCP startup is bounded")
}

async fn shutdown(manager: &McpManager) {
    // Leave room for the client's own five-second graceful-close deadline plus
    // process teardown; this is a deadlock guard, not a timing assertion.
    tokio::time::timeout(Duration::from_secs(10), manager.shutdown())
        .await
        .expect("MCP shutdown is bounded");
}

async fn wait_for_file(path: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture received request");
}

#[cfg(unix)]
async fn assert_process_reaped(pid: libc::pid_t) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            // SAFETY: signal zero only checks whether the recorded child exists.
            if unsafe { libc::kill(pid, 0) } == -1 {
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ESRCH)
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("owned MCP child must be terminated and reaped, not left as a zombie");
}

#[tokio::test]
async fn discovers_all_pages_and_passes_arguments_cwd_and_env() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let manager = connect(fixture.config()).await;
    assert!(manager.warnings().is_empty(), "{:?}", manager.warnings());
    let mut names: Vec<_> = manager
        .catalog()
        .iter()
        .map(|entry| {
            assert_eq!(entry.server, "fixture");
            entry.tool.name.to_string()
        })
        .collect();
    names.sort();
    assert_eq!(names, ["echo", "rpc_error", "slow", "tool_error"]);

    let arguments = json!({"message": "hello", "nested": {"number": 7}, "list": [true, null]});
    let result = manager
        .call(
            "fixture",
            "echo",
            arguments.as_object().unwrap().clone(),
            CancellationToken::new(),
        )
        .await
        .expect("echo call succeeds");
    let result = serde_json::to_value(result).unwrap();
    let payload: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(payload["arguments"], arguments);
    assert_eq!(payload["env"], "configured child value");
    assert_eq!(
        std::fs::canonicalize(payload["cwd"].as_str().unwrap()).unwrap(),
        std::fs::canonicalize(fixture.directory.path()).unwrap(),
    );
    shutdown(&manager).await;
}

#[tokio::test]
async fn preserves_tool_result_errors_and_reports_protocol_errors() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let manager = connect(fixture.config()).await;
    let result = manager
        .call(
            "fixture",
            "tool_error",
            Map::new(),
            CancellationToken::new(),
        )
        .await
        .expect("isError is a valid MCP result, not a transport failure");
    let result = serde_json::to_value(result).unwrap();
    assert_eq!(result["isError"], true);
    assert_eq!(result["content"][0]["text"], "fixture tool error");

    let error = manager
        .call("fixture", "rpc_error", Map::new(), CancellationToken::new())
        .await
        .expect_err("JSON-RPC error must fail the call");
    assert!(
        error.to_string().contains("fixture protocol error"),
        "{error}"
    );
    assert!(
        manager
            .call(
                "missing-server",
                "echo",
                Map::new(),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    // A failed call must not poison a healthy server session.
    assert!(
        manager
            .call("fixture", "echo", Map::new(), CancellationToken::new())
            .await
            .is_ok()
    );
    shutdown(&manager).await;
}

#[tokio::test]
async fn cancellation_interrupts_an_inflight_call_without_poisoning_session() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let manager = connect(fixture.config()).await;
    let cancel = CancellationToken::new();
    let call = manager.call("fixture", "slow", Map::new(), cancel.clone());
    let cancel_when_started = async {
        wait_for_file(&fixture.directory.path().join("started")).await;
        cancel.cancel();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(7), async {
        tokio::join!(call, cancel_when_started)
    })
    .await
    .expect("cancellation interrupts the pending tool call");
    assert!(matches!(result, Err(McpError::Cancelled)), "{result:?}");
    assert!(
        manager
            .call("fixture", "echo", Map::new(), CancellationToken::new())
            .await
            .is_ok()
    );
    shutdown(&manager).await;
}

#[tokio::test]
async fn configured_call_timeout_bounds_an_unresponsive_tool() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let mut config = fixture.config();
    config.call_timeout_secs = 1;
    let manager = connect(config).await;
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        manager.call("fixture", "slow", Map::new(), CancellationToken::new()),
    )
    .await
    .expect("configured timeout must stop a pending call");
    assert!(
        fixture.directory.path().join("started").exists(),
        "call reached server"
    );
    assert!(matches!(result, Err(McpError::Timeout)), "{result:?}");
    assert!(
        manager
            .call("fixture", "echo", Map::new(), CancellationToken::new())
            .await
            .is_ok()
    );
    shutdown(&manager).await;
}

#[tokio::test]
async fn one_failed_server_does_not_hide_healthy_server_tools() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let mut bad = fixture.config();
    bad.start_command = Some(vec![fixture.path("nonexistent-executable")]);
    let configs = BTreeMap::from([("broken".into(), bad), ("fixture".into(), fixture.config())]);
    let manager = tokio::time::timeout(
        Duration::from_secs(10),
        McpManager::connect(&configs, CancellationToken::new()),
    )
    .await
    .expect("partial startup is bounded");
    assert_eq!(manager.catalog().len(), 4);
    assert!(
        manager
            .catalog()
            .iter()
            .all(|entry| entry.server == "fixture")
    );
    assert!(
        manager
            .warnings()
            .iter()
            .any(|warning| warning.contains("broken")),
        "{:?}",
        manager.warnings()
    );
    shutdown(&manager).await;
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_reaps_owned_stdio_child_and_is_idempotent() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let manager = connect(fixture.config()).await;
    assert_eq!(manager.catalog().len(), 4, "{:?}", manager.warnings());
    let pid = fixture.pid();
    shutdown(&manager).await;
    assert_process_reaped(pid).await;
    shutdown(&manager).await;
}

#[cfg(unix)]
#[tokio::test]
async fn startup_timeout_reaps_owned_stdio_child() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let mut config = fixture.config();
    config.startup_timeout_secs = 1;
    config
        .env
        .insert("MCP_TEST_HANG_INITIALIZE".into(), "1".into());
    let manager = connect(config).await;
    assert!(manager.catalog().is_empty());
    assert!(!manager.warnings().is_empty());
    assert_process_reaped(fixture.pid()).await;
    shutdown(&manager).await;
}

#[tokio::test]
async fn reachable_http_errors_never_launch_a_configured_command() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let Some(fixture) = Fixture::new() else {
        return;
    };
    // Authentication, application, and malformed-protocol failures must not be
    // confused with an unreachable endpoint that is eligible for launch.
    for status in [
        "401 Unauthorized",
        "403 Forbidden",
        "500 Internal Server Error",
        "200 OK",
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(10), async {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let count = stream.read(&mut buffer).await.unwrap();
                    assert_ne!(count, 0, "client sent HTTP headers");
                    request.extend_from_slice(&buffer[..count]);
                    assert!(request.len() < 64 * 1024, "bounded request headers");
                }
                // Consume the request body before closing, avoiding a TCP reset
                // from unread bytes that could disguise an HTTP error as refusal.
                let header_end = request.windows(4).position(|window| window == b"\r\n\r\n").unwrap() + 4;
                let headers = std::str::from_utf8(&request[..header_end]).unwrap();
                let length: usize = headers.lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
                }).unwrap_or(0);
                while request.len() < header_end + length {
                    let count = stream.read(&mut buffer).await.unwrap();
                    assert_ne!(count, 0, "client sent complete HTTP body");
                    request.extend_from_slice(&buffer[..count]);
                }
                let body = "{\"error\":\"private-server-diagnostic\"}";
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }).await.expect("HTTP fixture completes");
        });
        let mut config = fixture.config();
        config.transport = McpTransport::StreamableHttp;
        config.url = Some(format!("http://{address}/mcp"));
        let manager = connect(config).await;
        server.await.expect("HTTP fixture task succeeds");
        assert!(manager.catalog().is_empty(), "{status}");
        assert!(!manager.warnings().is_empty(), "{status}");
        assert!(
            !fixture.directory.path().join("pid").exists(),
            "{status} must not launch a process"
        );
        assert!(
            manager
                .warnings()
                .iter()
                .all(|warning| !warning.contains("private-server-diagnostic"))
        );
        shutdown(&manager).await;
    }
}

const HTTP_FIXTURE: &str = r#"
import http.server, json, os, time
with open(os.environ['MCP_TEST_PID'] + '.tmp', 'w') as f:
    f.write(str(os.getpid()))
os.replace(os.environ['MCP_TEST_PID'] + '.tmp', os.environ['MCP_TEST_PID'])

class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass
    def do_GET(self):
        self.send_error(405)
    def do_DELETE(self):
        self.send_response(204)
        self.end_headers()
    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        if 'id' not in request:
            self.send_response(202)
            self.send_header('Content-Length', '0')
            self.end_headers()
            return
        method = request['method']
        if os.environ.get('MCP_TEST_HTTP_COUNTS'):
            with open(os.environ['MCP_TEST_HTTP_COUNTS'], 'a') as f:
                f.write(method + '\n')
        if method == 'tools/call' and os.environ.get('MCP_TEST_HTTP_EXPIRED'):
            self.send_error(404)
            return
        if method == 'initialize':
            result = {'protocolVersion': request['params']['protocolVersion'],
                      'capabilities': {'tools': {}},
                      'serverInfo': {'name': 'http-fixture', 'version': '1'}}
        elif method == 'tools/list':
            result = {'tools': [{'name': 'echo', 'inputSchema': {'type': 'object'}}]}
        elif method == 'tools/call':
            result = {'content': [{'type': 'text', 'text': 'http fixture response'}]}
        else:
            result = {}
        body = json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Mcp-Session-Id', 'fixture-session')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

# Force readiness polling rather than succeeding only if the first retry is late.
time.sleep(0.35)
server = http.server.ThreadingHTTPServer(('127.0.0.1', int(os.environ['MCP_TEST_PORT'])), Handler)
with open(os.environ['MCP_TEST_READY'] + '.tmp', 'w') as f:
    f.write(str(server.server_port))
os.replace(os.environ['MCP_TEST_READY'] + '.tmp', os.environ['MCP_TEST_READY'])
server.serve_forever()
"#;

fn http_fixture_config(fixture: &Fixture, port: u16) -> McpServerConfig {
    let mut config = fixture.config();
    config.transport = McpTransport::StreamableHttp;
    config.url = Some(format!("http://127.0.0.1:{port}/mcp"));
    config.start_command = Some(vec![
        "python3".into(),
        "-u".into(),
        "-c".into(),
        HTTP_FIXTURE.into(),
    ]);
    config.env.insert("MCP_TEST_PORT".into(), port.to_string());
    config
        .env
        .insert("MCP_TEST_READY".into(), fixture.path("ready"));
    config
}

#[cfg(unix)]
#[tokio::test]
async fn refused_http_port_launches_command_waits_for_readiness_and_reaps_child() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    // Release a kernel-selected port immediately before startup. No request can
    // succeed until the manager launches the configured HTTP subprocess.
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    let config = http_fixture_config(&fixture, port);
    drop(reservation);
    let manager = connect(config).await;
    assert!(manager.warnings().is_empty(), "{:?}", manager.warnings());
    assert_eq!(manager.catalog().len(), 1);
    assert_eq!(manager.catalog()[0].tool.name, "echo");
    assert!(fixture.directory.path().join("ready").exists());
    let result = manager
        .call("fixture", "echo", Map::new(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(result).unwrap()["content"][0]["text"],
        "http fixture response"
    );
    let pid = fixture.pid();
    shutdown(&manager).await;
    assert_process_reaped(pid).await;
}

#[tokio::test]
async fn shutdown_does_not_terminate_an_externally_managed_http_server() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let mut config = http_fixture_config(&fixture, 0);
    let argv = config.start_command.as_ref().unwrap();
    let mut child = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .envs(&config.env)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_for_file(&fixture.directory.path().join("ready")).await;
    let port: u16 = std::fs::read_to_string(fixture.path("ready"))
        .unwrap()
        .parse()
        .unwrap();
    config.url = Some(format!("http://127.0.0.1:{port}/mcp"));
    // If a reachable endpoint incorrectly triggers a launch, it fails visibly.
    config.start_command = Some(vec![fixture.path("must-not-be-executed")]);
    let manager = connect(config).await;
    assert!(manager.warnings().is_empty(), "{:?}", manager.warnings());
    assert_eq!(manager.catalog().len(), 1);
    shutdown(&manager).await;
    assert!(
        child.try_wait().unwrap().is_none(),
        "manager does not own existing HTTP server"
    );
    // Establish a fresh session to verify that the external server still works.
    config = http_fixture_config(&fixture, port);
    config.start_command = None;
    config.cwd = None;
    config.env.clear();
    let second = connect(config).await;
    assert_eq!(second.catalog().len(), 1, "{:?}", second.warnings());
    shutdown(&second).await;
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn startup_cancellation_reaps_child_and_does_not_launch_later_servers() {
    let Some(first) = Fixture::new() else { return };
    let Some(second) = Fixture::new() else { return };
    let mut config = first.config();
    config
        .env
        .insert("MCP_TEST_HANG_INITIALIZE".into(), "1".into());
    // Fill all four startup slots with hanging initialization, leaving the
    // fifth queued. Cancellation must drain/reap the started slots but never
    // launch the queued server.
    let fixtures: Vec<_> = (0..3).map(|_| Fixture::new().unwrap()).collect();
    let mut configs = BTreeMap::from([
        ("a-first".into(), config),
        ("z-queued".into(), second.config()),
    ]);
    for (index, fixture) in fixtures.iter().enumerate() {
        let mut config = fixture.config();
        config
            .env
            .insert("MCP_TEST_HANG_INITIALIZE".into(), "1".into());
        configs.insert(format!("b-{index}"), config);
    }
    let cancel = CancellationToken::new();
    let connect = McpManager::connect(&configs, cancel.clone());
    let cancel_when_started = async {
        wait_for_file(&first.directory.path().join("pid")).await;
        for fixture in &fixtures {
            wait_for_file(&fixture.directory.path().join("pid")).await;
        }
        assert!(
            !second.directory.path().join("pid").exists(),
            "only four startup slots"
        );
        cancel.cancel();
    };
    let (manager, ()) = tokio::time::timeout(Duration::from_secs(7), async {
        tokio::join!(connect, cancel_when_started)
    })
    .await
    .expect("startup cancellation is bounded");
    assert!(manager.catalog().is_empty());
    assert!(!manager.warnings().is_empty());
    assert!(
        !second.directory.path().join("pid").exists(),
        "cancelled startup must not spawn later servers"
    );
    assert_process_reaped(first.pid()).await;
    for fixture in &fixtures {
        assert_process_reaped(fixture.pid()).await;
    }
    shutdown(&manager).await;
}

#[tokio::test]
async fn pre_cancelled_and_unknown_calls_are_not_sent_to_server() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let manager = connect(fixture.config()).await;
    assert_eq!(manager.catalog().len(), 4, "{:?}", manager.warnings());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = manager.call("fixture", "slow", Map::new(), cancel).await;
    assert!(matches!(result, Err(McpError::Cancelled)), "{result:?}");
    assert!(!fixture.directory.path().join("started").exists());
    let result = manager
        .call(
            "fixture",
            "not-advertised",
            Map::new(),
            CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(result, Err(McpError::UnknownTool { .. })),
        "{result:?}"
    );
    shutdown(&manager).await;
    let result = manager
        .call("fixture", "echo", Map::new(), CancellationToken::new())
        .await;
    assert!(matches!(result, Err(McpError::Closed)), "{result:?}");
}

#[tokio::test]
async fn discovery_limits_skip_and_reap_unusable_servers() {
    for mode in ["oversized_schema", "many_tools", "repeated_cursor"] {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let mut config = fixture.config();
        config.env.insert("MCP_TEST_DISCOVERY".into(), mode.into());
        let manager = connect(config).await;
        assert!(manager.catalog().is_empty(), "{mode}");
        assert_eq!(manager.warnings().len(), 1, "{mode}");
        #[cfg(unix)]
        assert_process_reaped(fixture.pid()).await;
        shutdown(&manager).await;
    }
}

#[tokio::test]
async fn oversized_result_is_rejected_without_exposing_content() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let mut config = fixture.config();
    config
        .env
        .insert("MCP_TEST_LARGE_RESULT".into(), "1".into());
    let manager = connect(config).await;
    let error = manager
        .call("fixture", "echo", Map::new(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("result exceeds size limit"),
        "{error}"
    );
    shutdown(&manager).await;
}

#[tokio::test]
async fn invalid_direct_config_is_skipped_before_spawning_or_timer_creation() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let mut config = fixture.config();
    config.startup_timeout_secs = u64::MAX;
    let manager = connect(config).await;
    assert!(manager.catalog().is_empty());
    assert_eq!(manager.warnings().len(), 1);
    assert!(!fixture.directory.path().join("pid").exists());
    shutdown(&manager).await;
}

#[test]
fn bounded_serialization_counts_without_unbounded_output_copy() {
    assert_eq!(bounded_json_size(&json!({"a": "b"}), 9), Some(9));
    assert_eq!(bounded_json_size(&json!({"a": "b"}), 8), None);
}

#[tokio::test]
async fn dropping_call_future_sends_cancel_and_keeps_session_usable() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let manager = std::sync::Arc::new(connect(fixture.config()).await);
    let call_manager = manager.clone();
    let task = tokio::spawn(async move {
        call_manager
            .call("fixture", "slow", Map::new(), CancellationToken::new())
            .await
    });
    wait_for_file(&fixture.directory.path().join("started")).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    wait_for_file(&fixture.directory.path().join("cancelled")).await;
    manager
        .call("fixture", "echo", Map::new(), CancellationToken::new())
        .await
        .unwrap();
    shutdown(&manager).await;
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_shutdown_waiter_does_not_discard_cleanup_task() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let manager = std::sync::Arc::new(connect(fixture.config()).await);
    let pid = fixture.pid();
    let closing_manager = manager.clone();
    let task = tokio::spawn(async move { closing_manager.shutdown().await });
    tokio::task::yield_now().await;
    task.abort();
    let _ = task.await;
    shutdown(&manager).await;
    assert_process_reaped(pid).await;
}

#[test]
fn sdk_transport_error_diagnostics_are_redacted() {
    use rmcp::transport::DynamicTransportError;
    let secret = "https://user:password@example.invalid/mcp?token=private-secret";
    type FixtureTransport =
        rmcp::transport::async_rw::AsyncRwTransport<RoleClient, tokio::io::Empty, tokio::io::Sink>;
    let error = ServiceError::TransportSend(DynamicTransportError::new::<
        FixtureTransport,
        RoleClient,
    >(std::io::Error::other(secret)));
    let rendered = request_error(error).to_string();
    assert!(!rendered.contains("private-secret"));
    assert!(!rendered.contains("password"));
    assert!(!rendered.contains("example.invalid"));
}

#[tokio::test]
async fn expired_http_session_never_reinitializes_or_replays_tool_call() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut config = http_fixture_config(&fixture, port);
    config
        .env
        .insert("MCP_TEST_HTTP_COUNTS".into(), fixture.path("http-counts"));
    config
        .env
        .insert("MCP_TEST_HTTP_EXPIRED".into(), "1".into());
    let manager = connect(config).await;
    assert_eq!(manager.catalog().len(), 1, "{:?}", manager.warnings());
    assert!(
        manager
            .call("fixture", "echo", Map::new(), CancellationToken::new())
            .await
            .is_err()
    );
    let counts = std::fs::read_to_string(fixture.path("http-counts")).unwrap();
    assert_eq!(
        counts
            .lines()
            .filter(|method| *method == "initialize")
            .count(),
        1
    );
    assert_eq!(
        counts
            .lines()
            .filter(|method| *method == "tools/call")
            .count(),
        1
    );
    shutdown(&manager).await;
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_kills_entire_owned_process_group_not_unrelated_processes() {
    use std::os::unix::process::ExitStatusExt;
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let manager = connect(fixture.config()).await;
    let pid = fixture.pid();
    // Join a test-owned child to the server group. Retaining its Child handle
    // lets the test reap it (real grandchildren are reaped by their new parent).
    let mut member = tokio::process::Command::new("python3")
        .args(["-c", "import time; time.sleep(60)"])
        .process_group(pid)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut unrelated = tokio::process::Command::new("python3")
        .args(["-c", "import time; time.sleep(60)"])
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    shutdown(&manager).await;
    assert_process_reaped(pid).await;
    let status = tokio::time::timeout(Duration::from_secs(3), member.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    assert!(unrelated.try_wait().unwrap().is_none());
    unrelated.kill().await.unwrap();
    unrelated.wait().await.unwrap();
}

#[tokio::test]
async fn oversized_unterminated_stdio_frame_is_rejected_and_process_cleaned_up() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let mut config = fixture.config();
    config
        .env
        .insert("MCP_TEST_OVERSIZED_FRAME".into(), "1".into());
    let manager = connect(config).await;
    assert!(manager.catalog().is_empty());
    assert_eq!(manager.warnings().len(), 1);
    assert!(
        !manager.warnings()[0].contains("timed out"),
        "frame rejected before startup deadline"
    );
    #[cfg(unix)]
    assert_process_reaped(fixture.pid()).await;
    shutdown(&manager).await;
}

#[tokio::test]
async fn resources_only_server_gets_no_tools_requests_or_extra_client_capabilities() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let mut config = fixture.config();
    config.env.insert("MCP_TEST_NO_TOOLS".into(), "1".into());
    let manager = connect(config).await;
    assert!(manager.catalog().is_empty());
    assert!(manager.warnings().is_empty(), "{:?}", manager.warnings());
    assert!(!fixture.directory.path().join("list").exists());
    let initialize: Value =
        serde_json::from_str(&std::fs::read_to_string(fixture.path("initialize")).unwrap())
            .unwrap();
    let capabilities = initialize["capabilities"].as_object().unwrap();
    assert!(!capabilities.contains_key("sampling"));
    assert!(!capabilities.contains_key("elicitation"));
    assert!(!capabilities.contains_key("roots"));
    shutdown(&manager).await;
}
