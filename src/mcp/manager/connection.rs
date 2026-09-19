//! Per-server startup and cancellation-safe ownership of sessions and processes.
use super::super::{
    config::McpServerConfig,
    transport::{self, Client, OwnedProcess},
};
use super::{McpError, catalog::discover};
use rmcp::{Peer, RoleClient, model::Tool};
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;

struct Resources {
    client: Client,
    process: Option<OwnedProcess>,
}

impl Resources {
    async fn shutdown(mut self) {
        // Kill/reap owned commands even if remote protocol cleanup stalls.
        if let Some(process) = &mut self.process {
            process.shutdown().await;
        }
        let _ = self.client.close_with_timeout(Duration::from_secs(5)).await;
    }
}

enum ResourceState {
    Running(Box<Resources>),
    Closing(tokio::task::JoinHandle<()>),
    Closed,
}

pub(super) struct Server {
    pub(super) peer: Peer<RoleClient>,
    pub(super) timeout: Duration,
    resources: Mutex<ResourceState>,
    pub(super) calls: Semaphore,
}

impl Server {
    pub(super) async fn shutdown(&self) {
        // Store the teardown task before awaiting it. A cancelled shutdown
        // waiter releases the lock but does not lose kill/reap ownership;
        // another caller can still join the same cleanup task.
        let mut state = self.resources.lock().await;
        if matches!(*state, ResourceState::Running(_)) {
            let ResourceState::Running(resources) =
                std::mem::replace(&mut *state, ResourceState::Closed)
            else {
                unreachable!()
            };
            *state = ResourceState::Closing(tokio::spawn(resources.shutdown()));
        }
        if let ResourceState::Closing(task) = &mut *state {
            let _ = task.await;
            *state = ResourceState::Closed;
        }
    }
}

pub(super) async fn connect_one(
    name: String,
    config: McpServerConfig,
    cancel: CancellationToken,
) -> Option<(String, Result<(Vec<Tool>, Server), McpError>)> {
    if cancel.is_cancelled() {
        return None;
    }
    let mut process = None;
    let mut client = None;
    let startup = async {
        client = Some(transport::connect(&config, &mut process).await?);
        discover(client.as_ref().expect("connected client")).await
    };
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(McpError::Cancelled),
        result = tokio::time::timeout(config.startup_timeout(), startup) => {
            result.unwrap_or(Err(McpError::Timeout))
        }
    };
    match result {
        Ok(tools) => {
            let client = client.take().expect("discovery needs client");
            let peer = client.peer().clone();
            Some((
                name.clone(),
                Ok((
                    tools,
                    Server {
                        peer,
                        timeout: config.call_timeout(),
                        resources: Mutex::new(ResourceState::Running(Box::new(Resources {
                            client,
                            process,
                        }))),
                        calls: Semaphore::new(16),
                    },
                )),
            ))
        }
        Err(error) => {
            if let Some(mut client) = client {
                let _ = client.close_with_timeout(Duration::from_secs(5)).await;
            }
            if let Some(mut process) = process {
                process.shutdown().await;
            }
            Some((name.clone(), Err(error)))
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::super::tests::assert_process_reaped;
    use super::super::tests::{Fixture, connect, fixture_call, shutdown, wait_for_file};
    use crate::mcp::{
        config::{McpTransport, RawMcpServerConfig},
        manager::McpManager,
    };
    use std::{collections::BTreeMap, time::Duration};
    use tokio_util::sync::CancellationToken;

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
time.sleep(0.15)
server = http.server.ThreadingHTTPServer(('127.0.0.1', int(os.environ['MCP_TEST_PORT'])), Handler)
with open(os.environ['MCP_TEST_READY'] + '.tmp', 'w') as f:
    f.write(str(server.server_port))
os.replace(os.environ['MCP_TEST_READY'] + '.tmp', os.environ['MCP_TEST_READY'])
server.serve_forever()
"#;

    fn with_env(mut config: RawMcpServerConfig, name: &str, value: String) -> RawMcpServerConfig {
        config.env.insert(name.into(), value);
        config
    }

    fn hanging(fixture: &Fixture) -> RawMcpServerConfig {
        with_env(fixture.config(), "MCP_TEST_HANG_INITIALIZE", "1".into())
    }

    fn http_fixture_config(fixture: &Fixture, port: u16) -> RawMcpServerConfig {
        let mut config = fixture.config();
        config.transport = McpTransport::StreamableHttp;
        config.url = Some(format!("http://127.0.0.1:{port}/mcp"));
        let command = ["python3", "-u", "-c", HTTP_FIXTURE];
        config.start_command = Some(command.map(String::from).to_vec());
        let config = with_env(config, "MCP_TEST_PORT", port.to_string());
        with_env(config, "MCP_TEST_READY", fixture.path("ready"))
    }

    fn free_port() -> u16 {
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        reservation.local_addr().unwrap().port()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_timeout_reaps_owned_stdio_child() {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let mut config = hanging(&fixture);
        config.startup_timeout_secs = 1;
        let manager = connect(config).await;
        assert!(manager.catalog().is_empty());
        assert!(!manager.warnings().is_empty());
        assert_process_reaped(fixture.pid()).await;
        shutdown(&manager).await;
    }

    #[tokio::test]
    async fn reachable_http_errors_never_launch_a_configured_command() {
        use crate::provider::backends::transport::tests::{read_request, reply};
        use tokio::io::AsyncWriteExt;

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
                    // Consume the full request before closing, avoiding a TCP reset
                    // from unread bytes that could disguise an HTTP error as refusal.
                    read_request(&mut stream).await;
                    let body = "{\"error\":\"private-server-diagnostic\"}";
                    let response = reply(status, "Content-Type: application/json\r\n", body);
                    stream.write_all(response.as_bytes()).await.unwrap();
                    stream.shutdown().await.unwrap();
                })
                .await
                .expect("HTTP fixture completes");
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
            let leaked = |warning: &String| warning.contains("private-server-diagnostic");
            assert!(!manager.warnings().iter().any(leaked));
            shutdown(&manager).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refused_http_port_launches_command_waits_for_readiness_and_reaps_child() {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        // No request can succeed until the manager launches the HTTP subprocess.
        let manager = connect(http_fixture_config(&fixture, free_port())).await;
        assert!(manager.warnings().is_empty(), "{:?}", manager.warnings());
        assert_eq!(manager.catalog().len(), 1);
        assert_eq!(manager.catalog()[0].tool.name, "echo");
        assert!(fixture.directory.path().join("ready").exists());
        let result = fixture_call(&manager, "echo").await.unwrap();
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
        let Some(queued) = Fixture::new() else { return };
        // Fill all four startup slots with hanging initialization, leaving the
        // fifth queued. Cancellation must drain/reap the started slots but never
        // launch the queued server.
        let started: Vec<_> = (0..4).map(|_| Fixture::new().unwrap()).collect();
        let mut configs = BTreeMap::from([("z-queued".to_owned(), queued.config())]);
        for (index, fixture) in started.iter().enumerate() {
            configs.insert(format!("b-{index}"), hanging(fixture));
        }
        let configs = configs
            .into_iter()
            .map(|(name, raw)| (name, raw.try_into().unwrap()))
            .collect();
        let cancel = CancellationToken::new();
        let capabilities = crate::tool::policy::CapabilitySet::default();
        let connect = McpManager::connect(&configs, &capabilities, cancel.clone());
        let cancel_when_started = async {
            for fixture in &started {
                wait_for_file(&fixture.directory.path().join("pid")).await;
            }
            let queued_pid = queued.directory.path().join("pid");
            assert!(!queued_pid.exists(), "only four startup slots");
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
            !queued.directory.path().join("pid").exists(),
            "cancelled startup must not spawn later servers"
        );
        for fixture in &started {
            assert_process_reaped(fixture.pid()).await;
        }
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

    #[tokio::test]
    async fn expired_http_session_never_reinitializes_or_replays_tool_call() {
        let Some(fixture) = Fixture::new() else {
            return;
        };
        let config = http_fixture_config(&fixture, free_port());
        let config = with_env(config, "MCP_TEST_HTTP_COUNTS", fixture.path("http-counts"));
        let manager = connect(with_env(config, "MCP_TEST_HTTP_EXPIRED", "1".into())).await;
        assert_eq!(manager.catalog().len(), 1, "{:?}", manager.warnings());
        assert!(fixture_call(&manager, "echo").await.is_err());
        let counts = std::fs::read_to_string(fixture.path("http-counts")).unwrap();
        let count = |name| counts.lines().filter(|method| *method == name).count();
        assert_eq!((count("initialize"), count("tools/call")), (1, 1));
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
        let sleeper = |group| {
            tokio::process::Command::new("python3")
                .args(["-c", "import time; time.sleep(60)"])
                .process_group(group)
                .kill_on_drop(true)
                .spawn()
                .unwrap()
        };
        let (mut member, mut unrelated) = (sleeper(pid), sleeper(0));
        shutdown(&manager).await;
        assert_process_reaped(pid).await;
        let status = tokio::time::timeout(Duration::from_secs(3), member.wait())
            .await
            .unwrap()
            .unwrap();
        shutdown(&manager).await; // repeated shutdown must remain safe
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
        let config = with_env(fixture.config(), "MCP_TEST_OVERSIZED_FRAME", "1".into());
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
}
