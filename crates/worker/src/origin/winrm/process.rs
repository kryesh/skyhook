//! A command in a WinRS shell as a process: writes to its input become Send requests,
//! and its output comes from a loop of Receive requests.

use std::{
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream},
    sync::{oneshot, watch},
};
use tokio_util::sync::{CancellationToken, DropGuard};

use super::{
    Client,
    error::WinrmError,
    soap::{CommandId, Request, Response, SEND_BYTES, ShellId, Stream},
};
use crate::op::ExitStatus;

/// Output buffered between Receive replies and its reader. A reader that falls behind
/// stops the Receive loop, and the service then holds the command's output.
const OUTPUT_BYTES: usize = 256 * 1024;

/// A running command. Its streams behave as a child process's pipes: both outputs
/// must be read for the command to make progress, a dropped one is discarded, and
/// shutting down or dropping `stdin` ends the command's input.
pub struct Process {
    pub stdin: DuplexStream,
    pub stdout: DuplexStream,
    pub stderr: DuplexStream,
    pub handle: Handle,
}

/// How long a dropped shell may take to be deleted before it is given up: its Delete,
/// and before that what was left of its Create when its start was dropped.
pub(crate) const DELETE_BOUND: Duration = Duration::from_secs(5);

/// The command's exit, and its shell's lifetime: dropping the handle deletes the
/// shell, which ends the command.
pub struct Handle {
    exit: watch::Receiver<Option<Result<ExitStatus, WinrmError>>>,
    /// Stops the command's loops and has its shell deleted when dropped.
    _held: DropGuard,
}

/// A command and the shell it runs in.
struct Command {
    client: Client,
    shell: ShellId,
    id: CommandId,
    /// Why sending input failed, which then decides the command's result.
    input: Mutex<Option<WinrmError>>,
}

pub(super) async fn start(client: Client, command: &str) -> Result<Process, WinrmError> {
    let dropped = CancellationToken::new();
    let held = dropped.clone().drop_guard();
    let shell = shell(&client, dropped.clone()).await?;
    let started = Request::Command {
        shell: &shell,
        command,
    };
    let command = Arc::new(Command {
        id: client.request(&started).await?.command()?,
        client,
        shell,
        input: Mutex::new(None),
    });
    let (stdin, pending) = tokio::io::duplex(SEND_BYTES);
    let (stdout, stdout_sink) = tokio::io::duplex(OUTPUT_BYTES);
    let (stderr, stderr_sink) = tokio::io::duplex(OUTPUT_BYTES);
    let (exited, exit) = watch::channel(None);
    let (work, runtime) = (&command.client.0.work, tokio::runtime::Handle::current());
    let send = until(dropped.clone(), send(command.clone(), pending));
    work.spawn_on(send, &runtime);
    let receive = receive(command.clone(), [stdout_sink, stderr_sink], exited);
    work.spawn_on(until(dropped, receive), &runtime);
    Ok(Process {
        stdin,
        stdout,
        stderr,
        handle: Handle { exit, _held: held },
    })
}

/// Have the service create a shell, and delete it once `dropped`, as one task of the
/// client's work: a shell outlives the connection, and the caller, that made it. From
/// the drop, what is left of the Create and the Delete after it share one bound, the
/// one an origin's end allows a shell.
async fn shell(client: &Client, dropped: CancellationToken) -> Result<ShellId, WinrmError> {
    let (created, shell) = oneshot::channel();
    let owner = client.clone();
    let owned = async move {
        let life = async {
            let made = owner.request(&Request::Create).await;
            let made = made.and_then(|reply| Ok(reply.shell()?));
            let _ = created.send(made.clone());
            let Ok(shell) = made else { return };
            dropped.cancelled().await;
            let _ = owner.request(&Request::Delete { shell: &shell }).await;
        };
        tokio::pin!(life);
        tokio::select! {
            () = &mut life => {}
            () = dropped.cancelled() => {
                let _ = tokio::time::timeout(DELETE_BOUND, life).await;
            }
        }
    };
    let runtime = tokio::runtime::Handle::current();
    client.0.work.spawn_on(owned, &runtime);
    match shell.await {
        Ok(shell) => shell,
        // The runtime is shutting down, and this is dropped with it.
        Err(_) => std::future::pending().await,
    }
}

async fn until(stop: CancellationToken, work: impl Future<Output = ()>) {
    tokio::select! {
        () = work => {}
        () = stop.cancelled() => {}
    }
}

/// Send input as it arrives, each Send carrying what has gathered while the last one
/// was in flight, until the input ends. A failed Send ends the command.
async fn send(command: Arc<Command>, mut pending: DuplexStream) {
    let mut buffer = vec![0; SEND_BYTES];
    loop {
        let read = pending.read(&mut buffer).await.unwrap_or(0);
        let request = Request::Send {
            shell: &command.shell,
            command: &command.id,
            data: &buffer[..read],
            end: read == 0,
        };
        if let Err(error) = command.client.request(&request).await {
            *command.input.lock().unwrap_or_else(PoisonError::into_inner) = Some(error);
            let _ = command.terminate().await;
            return;
        }
        if read == 0 {
            return;
        }
    }
}

/// Receive output into `sinks` until the command is done, then report how it ended.
async fn receive(
    command: Arc<Command>,
    mut sinks: [DuplexStream; 2],
    exited: watch::Sender<Option<Result<ExitStatus, WinrmError>>>,
) {
    let request = Request::Receive {
        shell: &command.shell,
        command: &command.id,
    };
    let ended = loop {
        let reply = match command.client.exchange(&request).await {
            Ok(Response::Reply(reply)) => reply,
            Ok(Response::TimedOut(_)) => continue,
            Ok(Response::Fault(fault)) => break Err(WinrmError::Fault(fault)),
            Err(error) => break Err(error),
        };
        for (stream, bytes) in reply.output {
            let sink = &mut sinks[match stream {
                Stream::Stdout => 0,
                Stream::Stderr => 1,
            }];
            // A reader that went away discards its stream.
            let _ = sink.write_all(&bytes).await;
        }
        if let Some(code) = reply.exit {
            break Ok(ExitStatus::Exited(code));
        }
    };
    let input = command
        .input
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    exited.send_replace(Some(match (ended, input) {
        (Ok(_), Some(error)) => Err(error),
        (ended, _) => ended,
    }));
}

impl Command {
    async fn terminate(&self) -> Result<(), WinrmError> {
        let request = Request::Terminate {
            shell: &self.shell,
            command: &self.id,
        };
        self.client.request(&request).await.map(drop)
    }
}

impl Handle {
    /// How the command ended, once it has, whether or not the handle is still held: a
    /// command whose handle dropped first never ends here.
    pub fn exit(&self) -> impl Future<Output = Result<ExitStatus, WinrmError>> + Send + 'static {
        let mut exit = self.exit.clone();
        async move {
            let ended = exit
                .wait_for(Option::is_some)
                .await
                .map(|ended| ended.clone());
            match ended {
                Ok(ended) => ended.expect("waited for an exit"),
                Err(_) => std::future::pending().await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use futures_util::FutureExt as _;
    use tokio::sync::{mpsc, oneshot};

    use std::assert_matches;

    use super::super::{
        Tls,
        soap::tests::answer,
        tests::{Answer, identified},
    };
    use super::*;
    use crate::protocol::{
        ClientFrame, WorkerFrame,
        serve::tests::{Peer, Served, call, connect, receive, send},
        tests::call_id,
    };
    use crate::tests::bounded;

    /// What answers a held request.
    type Release = Box<dyn FnOnce() + Send>;

    /// A service that creates shell `S` running command `C`, never answers a Receive,
    /// holds the requests `holds` picks by their action and how many of it came
    /// before, and reports each request's action as it arrives, with what answers it
    /// if it is held.
    async fn service(
        holds: fn(&str, usize) -> bool,
    ) -> (
        super::super::Endpoint,
        mpsc::UnboundedReceiver<(String, Option<Release>)>,
    ) {
        let (arrived, actions) = mpsc::unbounded_channel();
        let counts = Mutex::new(HashMap::<String, usize>::new());
        let endpoint = identified(move |request| {
            let action = request.split_once("</wsa:Action>").unwrap().0;
            let action = action.rsplit_once('>').unwrap().1;
            let name = action.rsplit('/').next().unwrap().to_owned();
            let earlier = {
                let mut counts = counts.lock().unwrap();
                let count = counts.entry(name.clone()).or_default();
                *count += 1;
                *count - 1
            };
            let held = name == "Receive" || holds(&name, earlier);
            let body = match name.as_str() {
                "Create" => "<rsp:Shell><rsp:ShellId>S</rsp:ShellId></rsp:Shell>",
                "Command" => {
                    "<rsp:CommandResponse><rsp:CommandId>C</rsp:CommandId></rsp:CommandResponse>"
                }
                _ => "",
            };
            let reply = (200, Vec::new(), answer(action, body));
            let (answered, release) = match held {
                false => (Answer::Now(reply), None),
                true => {
                    let (release, reply_held) = oneshot::channel();
                    let release: Release = Box::new(move || drop(release.send(reply)));
                    (Answer::Held(reply_held), Some(release))
                }
            };
            arrived.send((name, release)).unwrap();
            answered
        })
        .await;
        (endpoint, actions)
    }

    async fn connected(endpoint: super::super::Endpoint, work: skyhook_base::Work) -> Client {
        let basic = super::super::tests::basic();
        let connected = super::super::connect(endpoint, basic, Tls::Off, work);
        bounded(connected).await.unwrap()
    }

    /// What answers the next request for `name`, if it is held.
    async fn arrival(
        actions: &mut mpsc::UnboundedReceiver<(String, Option<Release>)>,
        name: &str,
    ) -> Option<Release> {
        loop {
            let (action, release) = actions.recv().await.unwrap();
            if action == name {
                return release;
            }
        }
    }

    /// A start cancelled before its command has started deletes the shell it made,
    /// one the service had yet to answer for once it does. From the cancellation its
    /// work has what an origin's end allows: a Create answered late leaves the Delete
    /// after it the rest of the bound, and one never answered is given up with it.
    #[tokio::test]
    async fn a_cancelled_start_deletes_the_shell_it_made() {
        for held in ["Create", "Command"] {
            let (endpoint, mut actions) = service(|_, _| true).await;
            let work = skyhook_base::Work::default();
            let client = connected(endpoint, work.clone()).await;
            let starting = tokio::spawn(async move { client.run("x").await.map(drop) });
            let mut answer = bounded(arrival(&mut actions, "Create")).await.unwrap();
            if held == "Command" {
                answer();
                answer = bounded(arrival(&mut actions, "Command")).await.unwrap();
            }
            // Virtual time only while no exchange is awaited: one is real socket I/O,
            // which a paused clock would skip past.
            tokio::time::pause();
            let cancelled = tokio::time::Instant::now();
            starting.abort();
            assert!(bounded(starting).await.unwrap_err().is_cancelled());
            tokio::time::sleep(DELETE_BOUND / 2).await;
            tokio::time::resume();
            answer();
            bounded(arrival(&mut actions, "Delete")).await;
            tokio::time::pause();
            bounded(work.finished()).await;
            assert!(cancelled.elapsed() <= DELETE_BOUND + crate::protocol::END_GRACE);
            tokio::time::resume();
        }
    }

    /// A served connection's client, once its handshake is done and its call 1 has
    /// started relaying command `x` over WinRM to `endpoint`, and the worker serving it.
    async fn relaying(
        endpoint: super::super::Endpoint,
        actions: &mut mpsc::UnboundedReceiver<(String, Option<Release>)>,
    ) -> (Peer, Served) {
        let root = tempfile::tempdir().unwrap();
        let root = crate::fs::canonical(root.path()).unwrap();
        let root = root.to_str().unwrap().to_owned();
        let settings = crate::target::Settings {
            authorization_root: root,
        };
        let (mut peer, served) = connect(settings).await;
        send(&mut peer, ClientFrame::Hello).await;
        assert_matches!(receive(&mut peer).await, Some(WorkerFrame::Ready(_)));
        let password = crate::secret::Credential::Literal("password".into());
        let auth = super::super::Auth::Basic {
            user: "user".into(),
            password,
        };
        let route = super::super::WinrmRoute {
            target: "win".into(),
            endpoint,
            auth,
            tls: Tls::Off,
        };
        let open = crate::op::Open {
            leg: crate::origin::Leg::Winrm(route),
            command: "x".into(),
            input: Default::default(),
        };
        call(&mut peer, call_id(1), open).await;
        bounded(arrival(actions, "Command")).await;
        (peer, served)
    }

    /// An origin acknowledges its connection's end once the shells its calls opened are
    /// deleted: a relayed stream's shell is gone by the time the client reads `Ended`.
    #[tokio::test]
    async fn an_origin_ends_its_connection_once_its_shells_are_deleted() {
        let (endpoint, mut actions) = service(|_, _| false).await;
        let (mut peer, served) = relaying(endpoint, &mut actions).await;
        send(&mut peer, ClientFrame::End).await;
        assert_matches!(receive(&mut peer).await, Some(WorkerFrame::Ended));
        let arrived: Vec<_> = std::iter::from_fn(|| actions.try_recv().ok())
            .map(|(action, _)| action)
            .collect();
        assert!(
            arrived.iter().any(|action| action == "Delete"),
            "{arrived:?}"
        );
        bounded(served).await.unwrap().unwrap();
    }

    /// A connection's end waits only for its own shells: while another connection's
    /// Delete is held, this one ends, and that one ends only once its Delete is answered.
    #[tokio::test]
    async fn a_connection_ends_without_waiting_on_anothers_shells() {
        let (endpoint, mut actions) =
            service(|action, earlier| action == "Delete" && earlier == 0).await;
        let (mut held, held_served) = relaying(endpoint.clone(), &mut actions).await;
        let (mut peer, served) = relaying(endpoint, &mut actions).await;
        send(&mut held, ClientFrame::End).await;
        let release = bounded(arrival(&mut actions, "Delete")).await.unwrap();
        send(&mut peer, ClientFrame::End).await;
        assert_matches!(receive(&mut peer).await, Some(WorkerFrame::Ended));
        let early = held.frames.next::<WorkerFrame>().now_or_never();
        assert!(early.is_none(), "{early:?}");
        release();
        assert_matches!(receive(&mut held).await, Some(WorkerFrame::Ended));
        bounded(served).await.unwrap().unwrap();
        bounded(held_served).await.unwrap().unwrap();
    }
}
