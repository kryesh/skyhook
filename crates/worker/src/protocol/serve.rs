//! Serving one connection's operations on this machine: the handshake, every call
//! with its flow-controlled events and input, and the connection's end.
use std::collections::HashMap;

use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt as _},
    sync::{mpsc, oneshot},
    task::JoinSet,
};

use super::{
    CallId, ClientFrame, FrameReader, Outbox, Violation, Wire, WorkerFrame, encoded_bytes,
    flow::{Credits, QUEUE},
    write_frame,
};
use crate::{
    local::Local,
    op::{CHUNK_BYTES, Events, Input, Resolution},
    target::Settings,
};

/// Why a served connection failed.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Violation(#[from] Violation),
    /// A call's task panicked.
    #[error(transparent)]
    Task(#[from] tokio::task::JoinError),
}

/// How a served connection ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// The client ended it.
    Requested,
    /// The client never did: it went away, or the worker refused it at the handshake.
    Unrequested,
}

/// Serve one connection over `input` and `output` under `settings`. Its calls end
/// with it however it ends; nothing outlives it.
pub async fn serve<R, W>(input: R, mut output: W, settings: Settings) -> Result<Ended, ServeError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    output.write_all(super::PREAMBLE).await?;
    output.flush().await?;
    let mut input = FrameReader::new(input);
    match input.next::<ClientFrame>().await? {
        Some(ClientFrame::Hello) => {}
        Some(frame) => {
            let received = format!("{frame:?}");
            return Err(Violation::ExpectedHello { received }.into());
        }
        None => return Ok(Ended::Unrequested),
    }
    let local = match Local::open(&settings) {
        Ok(local) => local,
        Err(error) => {
            write_frame(&mut output, &WorkerFrame::Refused(error)).await?;
            return Ok(Ended::Unrequested);
        }
    };
    write_frame(&mut output, &WorkerFrame::Ready(local.facts())).await?;
    let (outbox, writer) = Outbox::new();
    let mut connection = Connection {
        local,
        outbox,
        calls: HashMap::new(),
        tasks: JoinSet::new(),
    };
    let writing = writer.spawn(output);
    tokio::pin!(writing);
    // The writer outlasts the connection's outbox unless it fails, and a connection
    // that can answer nothing more stops as one whose client departed does.
    let served = tokio::select! {
        ended = connection.run(&mut input) => Ok(ended),
        Err(failed) = &mut writing => Err(failed),
    };
    connection.stop().await;
    let ended = served?;
    if let Ok(Ended::Requested) = ended {
        connection.outbox.send(WorkerFrame::Ended);
    }
    drop(connection);
    writing.await?;
    ended
}

struct Connection {
    local: Local,
    outbox: Outbox<WorkerFrame>,
    calls: HashMap<CallId, Slot>,
    tasks: JoinSet<CallId>,
}

/// A call in progress. Dropping it stops the call.
struct Slot {
    task: tokio::task::AbortHandle,
    input: mpsc::Sender<Option<Vec<u8>>>,
    credits: Credits,
    /// Resumes a held contained call.
    resume: Option<oneshot::Sender<()>>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Connection {
    /// End every call, once its task, and with it its operation, has been dropped,
    /// the blocking work its operation started has stopped or finished, the SSH
    /// processes its operations started are killed and the shells they opened deleted.
    async fn stop(&mut self) {
        self.calls.clear();
        self.tasks.shutdown().await;
        self.local.work().finished().await;
    }

    async fn run<R: AsyncRead + Unpin>(
        &mut self,
        input: &mut FrameReader<R>,
    ) -> Result<Ended, ServeError> {
        loop {
            let frame = tokio::select! {
                biased;
                frame = input.next::<ClientFrame>() => frame?,
                finished = self.tasks.join_next(), if !self.tasks.is_empty() => {
                    match finished.expect("nonempty call set") {
                        Ok(id) => {
                            self.calls.remove(&id);
                        }
                        // A Cancel dropped the call's slot, which aborted it.
                        Err(error) if error.is_cancelled() => {}
                        Err(panicked) => return Err(panicked.into()),
                    }
                    continue;
                }
            };
            match frame {
                None => return Ok(Ended::Unrequested),
                Some(ClientFrame::End) => return Ok(Ended::Requested),
                Some(frame) => self.handle(frame)?,
            }
        }
    }

    fn handle(&mut self, frame: ClientFrame) -> Result<(), ServeError> {
        match frame {
            ClientFrame::Call { id, op, contained } => self.start(id, contained, op)?,
            ClientFrame::Resume { id } => {
                if let Some(slot) = self.calls.get_mut(&id)
                    && let Some(resume) = slot.resume.take()
                {
                    let _ = resume.send(());
                }
            }
            ClientFrame::Input { id, data } => {
                if data.len() > CHUNK_BYTES {
                    return Err(Violation::OversizedInput.into());
                }
                // A call that stopped taking input, or ended, discards it.
                if let Some(slot) = self.calls.get(&id)
                    && let Err(mpsc::error::TrySendError::Full(_)) = slot.input.try_send(Some(data))
                {
                    return Err(Violation::InputOverflow.into());
                }
            }
            ClientFrame::InputEnd { id } => {
                if let Some(slot) = self.calls.get(&id) {
                    let _ = slot.input.try_send(None);
                }
            }
            ClientFrame::EventAck { id } => {
                if let Some(slot) = self.calls.get(&id)
                    && !slot.credits.acknowledge()
                {
                    return Err(Violation::EventCredit.into());
                }
            }
            ClientFrame::Cancel { id } => {
                self.calls.remove(&id);
            }
            ClientFrame::Hello => return Err(Violation::SecondHello.into()),
            ClientFrame::End => unreachable!("handled by the connection loop"),
        }
        Ok(())
    }

    /// Start call `id` performing `op`, contained in the root when its `paths` are
    /// given: performed at once if they all resolve inside it, else once resumed.
    fn start(
        &mut self,
        id: CallId,
        paths: Option<crate::op::Resolve>,
        mut op: Box<super::Op>,
    ) -> Result<(), ServeError> {
        if self.calls.contains_key(&id) {
            return Err(Violation::DuplicateCall(id).into());
        }
        // Each resolution takes the place of the path it resolved, which its op names
        // in the same place.
        let requested = paths
            .iter()
            .flat_map(|paths| paths.paths.iter().map(|(path, _)| path));
        if paths.is_some() && !op.paths().into_iter().map(|path| &*path).eq(requested) {
            return Err(Violation::ContainedPaths(id).into());
        }
        let (input, received) = mpsc::channel(QUEUE);
        let credits = Credits::default();
        let (resume, resumed) = oneshot::channel();
        let call = Call {
            id,
            outbox: self.outbox.clone(),
            credits: credits.clone(),
            input: received,
        };
        let local = self.local.clone();
        let task = self.tasks.spawn(self.local.work().scope(async move {
            if let Some(paths) = paths {
                let resolved = paths.resolve(&local).await;
                let inside = (resolved.iter()).all(|resolved| {
                    matches!(&resolved.resolution, Ok(Resolution::Resolved(path)) if !path.outside)
                });
                crate::op::Resolved::place(&resolved, op.paths());
                let landed = match inside {
                    true => WorkerFrame::Performing { id, resolved },
                    false => WorkerFrame::Held { id, resolved },
                };
                call.outbox.send(landed);
                if !inside && resumed.await.is_err() {
                    return id;
                }
            }
            op.serve(&local, call).await;
            id
        }));
        let slot = Slot {
            task,
            input,
            credits,
            resume: Some(resume),
        };
        self.calls.insert(id, slot);
        Ok(())
    }
}

/// One call's side of the connection: its frames out and its input in.
pub(crate) struct Call {
    id: CallId,
    outbox: Outbox<WorkerFrame>,
    credits: Credits,
    input: mpsc::Receiver<Option<Vec<u8>>>,
}

impl Call {
    /// Perform `operation` within this call's task, so that the call's end is the
    /// operation's, its events sent in frames taking one credit each, then send its
    /// end.
    pub(crate) async fn run<O: Wire>(self, local: &Local, mut operation: O) {
        let Self {
            id,
            outbox,
            credits,
            input,
        } = self;
        if let Some(slot) = operation.input() {
            *slot = Input::new(input_reader(id, input, outbox.clone()));
        }
        let (sender, mut events) = mpsc::channel(crate::local::EVENTS);
        let perform = operation.perform(local, Events::new(sender));
        let forward = async {
            let mut next = None;
            while let Some(first) = match next.take() {
                None => events.recv().await,
                event => event,
            } {
                credits.take().await;
                // Events queued while the credit was awaited share its frame.
                let events = O::event_frame(gather(first, &mut events, &mut next));
                outbox.send_data(WorkerFrame::Events { id, events }).await;
            }
        };
        let (answer, ()) = tokio::join!(perform, forward);
        let outcome = O::outcome(answer);
        outbox.send(WorkerFrame::Done { id, outcome });
    }
}

/// `first` and the events already queued behind it, up to a chunk's bytes; the one
/// that would pass that bound is kept in `next`, to start the frame after.
fn gather<E: serde::Serialize>(
    first: E,
    events: &mut mpsc::Receiver<E>,
    next: &mut Option<E>,
) -> Vec<E> {
    let mut bytes = encoded_bytes(&first);
    let mut batch = vec![first];
    while let Ok(event) = events.try_recv() {
        bytes += encoded_bytes(&event);
        if bytes > CHUNK_BYTES {
            *next = Some(event);
            break;
        }
        batch.push(event);
    }
    batch
}

/// The call's input as it arrives, each chunk acknowledged as it is taken so the
/// client may send another.
fn input_reader(
    id: CallId,
    input: mpsc::Receiver<Option<Vec<u8>>>,
    outbox: Outbox<WorkerFrame>,
) -> impl AsyncRead + Send + 'static {
    let chunks = futures_util::stream::unfold(input, move |mut input| {
        let outbox = outbox.clone();
        async move {
            let data = input.recv().await.flatten()?;
            outbox.acknowledge(WorkerFrame::InputAck { id });
            Some((Ok::<_, std::io::Error>(bytes::Bytes::from(data)), input))
        }
    });
    tokio_util::io::StreamReader::new(Box::pin(chunks))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        op::{Exec, ExecEvent, PathKind, Program, Resolution, Resolve, TargetPath},
        protocol::tests::call_id,
        protocol::{EventFrame, PREAMBLE},
        target::SettingsError,
        tests::bounded,
    };
    use std::assert_matches;
    use tokio::io::AsyncReadExt as _;

    pub(crate) type Served = tokio::task::JoinHandle<Result<Ended, ServeError>>;

    /// A client's end of a connection: the frames it receives, and where it sends,
    /// each closed by its own drop.
    pub(crate) struct Peer {
        pub(crate) frames: FrameReader<tokio::io::DuplexStream>,
        pub(crate) sent: tokio::io::DuplexStream,
    }

    /// A client's end of a connection served under `settings`, past its preamble.
    pub(crate) async fn connect(settings: Settings) -> (Peer, Served) {
        let (sent, input) = tokio::io::duplex(64 * 1024);
        let (output, mut frames) = tokio::io::duplex(64 * 1024);
        let served = tokio::spawn(serve(input, output, settings));
        let preamble = &mut [0; PREAMBLE.len()];
        bounded(frames.read_exact(preamble)).await.unwrap();
        assert_eq!(preamble, PREAMBLE);
        let frames = FrameReader::new(frames);
        (Peer { frames, sent }, served)
    }

    fn settings(root: &std::path::Path) -> Settings {
        Settings {
            authorization_root: root.to_str().unwrap().to_owned(),
        }
    }

    pub(crate) async fn send(client: &mut Peer, frame: ClientFrame) {
        write_frame(&mut client.sent, &frame).await.unwrap();
    }

    /// Start call `id` performing `operation`.
    pub(crate) async fn call(client: &mut Peer, id: CallId, operation: impl Wire) {
        let (op, contained) = (Box::new(operation.into_op()), None);
        send(client, ClientFrame::Call { id, op, contained }).await;
    }

    pub(crate) async fn receive(client: &mut Peer) -> Option<WorkerFrame> {
        bounded(client.frames.next()).await.unwrap()
    }

    async fn ended(served: Served) -> Ended {
        bounded(served).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn handshakes_refuse_unusable_settings_and_fail_on_another_builds_frames() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let usable = settings(&root);
        // A client of another build sends what this one cannot decode.
        let (mut client, served) = connect(usable.clone()).await;
        write_frame(&mut client.sent, &"Unknown").await.unwrap();
        assert_matches!(bounded(served).await.unwrap(), Err(ServeError::Io(_)));
        assert_matches!(receive(&mut client).await, None);

        let missing = root.join("missing");
        let (mut client, served) = connect(settings(&missing)).await;
        send(&mut client, ClientFrame::Hello).await;
        assert_matches!(
            receive(&mut client).await,
            Some(WorkerFrame::Refused(SettingsError::AuthorizationRoot { path, .. }))
                if path == missing.to_str().unwrap()
        );
        assert_eq!(ended(served).await, Ended::Unrequested);

        let (client, served) = connect(usable).await;
        drop(client);
        assert_eq!(ended(served).await, Ended::Unrequested);
    }

    /// A call answers with its own operation's end, and the connection ends when
    /// the client ends it, or when the client departs.
    #[tokio::test]
    async fn calls_answer_their_operation_until_the_connection_ends() {
        let root = tempfile::tempdir().unwrap();
        let root = crate::fs::canonical(root.path()).unwrap();
        std::fs::write(root.join("file"), b"").unwrap();
        let text = root.to_str().unwrap();
        let file = TargetPath::new(root.join("file").to_str().unwrap());
        for requested in [true, false] {
            let (mut client, served) = connect(settings(&root)).await;
            send(&mut client, ClientFrame::Hello).await;
            let Some(WorkerFrame::Ready(facts)) = receive(&mut client).await else {
                panic!("expected the worker to be ready");
            };
            assert_eq!(facts.authorization_root, TargetPath::new(text));
            let resolve = Resolve {
                workspace: TargetPath::new(text),
                paths: vec![("file".into(), PathKind::Existing)],
            };
            let id = call_id(1);
            call(&mut client, id, resolve).await;
            let Some(WorkerFrame::Done { id: done, outcome }) = receive(&mut client).await else {
                panic!("expected the call's end");
            };
            assert_eq!(done, id);
            let Some(Ok(resolved)) = Resolve::answer(outcome) else {
                panic!("expected the resolve's answer");
            };
            assert_matches!(
                &resolved[..],
                [resolved] if matches!(&resolved.resolution,
                    Ok(Resolution::Resolved(path)) if path.path == file)
            );
            let expected = match requested {
                true => {
                    send(&mut client, ClientFrame::End).await;
                    assert_matches!(receive(&mut client).await, Some(WorkerFrame::Ended));
                    Ended::Requested
                }
                false => {
                    drop(client);
                    Ended::Unrequested
                }
            };
            assert_eq!(ended(served).await, expected);
        }
    }

    /// A connection ends its calls' processes before it has ended: a process still
    /// running has been killed by the time the client reads `Ended`, and by the time a
    /// connection whose output failed, its input still open, has stopped serving.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_connection_ends_once_its_calls_processes_are_killed() {
        let field = |status: &str, name: &str| {
            let line = status.lines().find_map(|line| line.strip_prefix(name));
            line.unwrap_or_default().trim().to_owned()
        };
        // Asleep in `sleep` with no kill pending, as no killed process ever is again.
        let running = move |pid: &str| {
            let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
                return false;
            };
            let kill = 1_u64 << (libc::SIGKILL - 1);
            let pending = ["SigPnd:", "ShdPnd:"].into_iter().any(|name| {
                u64::from_str_radix(&field(&status, name), 16).is_ok_and(|mask| mask & kill != 0)
            });
            field(&status, "Name:") == "sleep"
                && field(&status, "State:").starts_with('S')
                && !pending
        };
        let root = tempfile::tempdir().unwrap();
        let root = crate::fs::canonical(root.path()).unwrap();
        let text = root.to_str().unwrap();
        for requested in [true, false] {
            let (mut client, served) = connect(settings(&root)).await;
            send(&mut client, ClientFrame::Hello).await;
            assert_matches!(receive(&mut client).await, Some(WorkerFrame::Ready(_)));
            let exec = Exec {
                program: Program::Shell("echo $$; exec sleep 60".into()),
                cwd: TargetPath::new(text),
                env: Default::default(),
                timeout: None,
                spill: crate::op::StreamSpill::never(),
            };
            call(&mut client, call_id(1), exec).await;
            let mut output = String::new();
            let pid = loop {
                let Some(WorkerFrame::Events {
                    events: EventFrame::Exec(events),
                    ..
                }) = receive(&mut client).await
                else {
                    panic!("expected the process's events");
                };
                for event in events {
                    if let ExecEvent::Output { text, .. } = event {
                        output += &text;
                    }
                }
                if let Some(pid) = output
                    .lines()
                    .last()
                    .filter(|pid| pid.parse::<u32>().is_ok())
                {
                    break pid.to_owned();
                }
            };
            bounded(async {
                while !running(&pid) {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            if !requested {
                let Peer { frames, mut sent } = client;
                drop(frames);
                // Answering another call is what finds the output closed.
                let segments = vec![TargetPath::new(text)];
                let op = Box::new(crate::op::Workspace { segments }.into_op());
                let (id, contained) = (call_id(2), None);
                let answered = ClientFrame::Call { id, op, contained };
                write_frame(&mut sent, &answered).await.unwrap();
                assert_matches!(bounded(served).await.unwrap(), Err(ServeError::Io(_)));
                assert!(!running(&pid), "a process outlived its connection");
                continue;
            }
            // A task of its own reads the end, as a client on this runtime would, so
            // the tasks queued before it are not all run first.
            let ending = tokio::spawn(async move {
                send(&mut client, ClientFrame::End).await;
                assert_matches!(receive(&mut client).await, Some(WorkerFrame::Ended));
                running(&pid)
            });
            assert!(
                !bounded(ending).await.unwrap(),
                "a process outlived its connection's end"
            );
            assert_eq!(ended(served).await, Ended::Requested);
        }
    }

    /// A connection acknowledges its end once the blocking work its calls started has
    /// returned, even a call's whose task the end dropped: a tree it was removing is
    /// gone by the time the client reads `Ended`, never left part removed.
    #[tokio::test]
    async fn an_end_is_acknowledged_once_its_calls_blocking_work_has_returned() {
        let root = tempfile::tempdir().unwrap();
        let root = crate::fs::canonical(root.path()).unwrap();
        let tree = root.join("tree");
        for directory in 0..40 {
            let directory = tree.join(directory.to_string());
            std::fs::create_dir_all(&directory).unwrap();
            for file in 0..100 {
                std::fs::write(directory.join(file.to_string()), b"").unwrap();
            }
        }
        let (mut client, served) = connect(settings(&root)).await;
        send(&mut client, ClientFrame::Hello).await;
        assert_matches!(receive(&mut client).await, Some(WorkerFrame::Ready(_)));
        let remove = crate::op::Remove {
            path: TargetPath::new(tree.to_str().unwrap()),
            recursive: true,
        };
        call(&mut client, call_id(1), remove).await;
        // The removal has begun once one of the tree's directories is gone.
        let listed = || std::fs::read_dir(&tree).map_or(0, Iterator::count);
        bounded(async {
            while listed() == 40 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        send(&mut client, ClientFrame::End).await;
        while !matches!(
            receive(&mut client).await.expect("the end is acknowledged"),
            WorkerFrame::Ended
        ) {}
        assert!(!tree.exists(), "the removal outlived its connection's end");
        assert_eq!(ended(served).await, Ended::Requested);
    }

    /// A contained call performs on its paths' resolutions at once only when every one
    /// resolved inside the root. One leaving it, as spelled, by `..` or through a link,
    /// is held with the resolution, performing nothing until it is resumed, then on
    /// that resolution, or never once cancelled. One whose operation names another path
    /// than it resolves fails the connection, performing nothing.
    #[tokio::test]
    async fn contained_calls_perform_inside_the_root_or_once_resumed() {
        use crate::op::{Content, Resolved, Write, Written};
        let base = tempfile::tempdir().unwrap();
        let base = crate::fs::canonical(base.path()).unwrap();
        let (root, outside) = (base.join("root"), base.join("outside"));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let (mut client, served) = connect(settings(&root)).await;
        send(&mut client, ClientFrame::Hello).await;
        assert_matches!(receive(&mut client).await, Some(WorkerFrame::Ready(_)));
        // Call `id` writing `named`, resolving `requested`.
        let contained = |id, named: &str, requested: &str| {
            let write = Write {
                path: named.into(),
                content: Content::Inline(b"written".to_vec()),
            };
            let paths = Resolve {
                workspace: TargetPath::new(root.to_str().unwrap()),
                paths: vec![(requested.into(), PathKind::Writable)],
            };
            let (op, contained) = (Box::new(write.into_op()), Some(paths));
            ClientFrame::Call { id, op, contained }
        };
        let escaping = outside.join("spelled");
        for (number, requested, resumed) in [
            (1, "missing/new", None),
            (2, escaping.to_str().unwrap(), Some(true)),
            (3, "../outside/parent", Some(false)),
            (4, "link/linked", Some(true)),
        ] {
            let id = call_id(number);
            send(&mut client, contained(id, requested, requested)).await;
            let resolved = match (receive(&mut client).await, resumed) {
                (Some(WorkerFrame::Performing { resolved, .. }), None) => resolved,
                (Some(WorkerFrame::Held { resolved, .. }), Some(_)) => resolved,
                (other, _) => panic!("{requested}: {other:?}"),
            };
            let [
                Resolved {
                    resolution: Ok(Resolution::Resolved(path)),
                    ..
                },
            ] = &resolved[..]
            else {
                panic!("{requested} resolved as {resolved:?}");
            };
            let written = std::path::Path::new(path.path.as_str());
            assert_eq!(path.outside, resumed.is_some(), "{requested}");
            match resumed {
                Some(true) => {
                    assert!(
                        !written.exists(),
                        "{requested} was written before it resumed"
                    );
                    send(&mut client, ClientFrame::Resume { id }).await;
                }
                Some(false) => send(&mut client, ClientFrame::Cancel { id }).await,
                None => {}
            }
            if resumed == Some(false) {
                continue;
            }
            let Some(WorkerFrame::Done { outcome, .. }) = receive(&mut client).await else {
                panic!("expected {requested}'s end");
            };
            assert_matches!(Write::answer(outcome), Some(Ok(Written { bytes: 7 })));
            assert_eq!(std::fs::read(written).unwrap(), b"written");
        }
        let id = call_id(5);
        send(&mut client, contained(id, "named", "resolved")).await;
        assert_matches!(
            bounded(served).await.unwrap(),
            Err(ServeError::Violation(Violation::ContainedPaths(failed))) if failed == id
        );
        for unwritten in [
            outside.join("parent"),
            root.join("named"),
            root.join("resolved"),
        ] {
            assert!(!unwritten.exists(), "{unwritten:?} was written");
        }
    }

    /// A frame takes the events already queued while they fit in a chunk's bytes,
    /// in order; an event that alone passes the bound still has a frame of its own.
    #[test]
    fn frames_gather_queued_events_up_to_a_chunk() {
        let (sender, mut queued) = mpsc::channel(8);
        let large = "x".repeat(CHUNK_BYTES);
        for event in ["a", "b", &large, "c"] {
            sender.try_send(event.to_owned()).unwrap();
        }
        drop(sender);
        let mut frames = Vec::new();
        let mut next = None;
        while let Some(first) = next.take().or_else(|| queued.try_recv().ok()) {
            frames.push(gather(first, &mut queued, &mut next));
        }
        assert_eq!(frames, [vec!["a", "b"], vec![&large[..]], vec!["c"]]);
    }

    /// Cancelling a running call stops only that call: the next call on the
    /// connection is answered, and the connection ends when the client ends it.
    #[tokio::test]
    async fn a_cancelled_call_leaves_the_connection_serving() {
        let root = tempfile::tempdir().unwrap();
        let root = crate::fs::canonical(root.path()).unwrap();
        let text = root.to_str().unwrap();
        let (mut client, served) = connect(settings(&root)).await;
        send(&mut client, ClientFrame::Hello).await;
        assert_matches!(receive(&mut client).await, Some(WorkerFrame::Ready(_)));
        let exec = Exec {
            program: Program::Shell("sleep 60".into()),
            cwd: TargetPath::new(text),
            env: Default::default(),
            timeout: None,
            spill: crate::op::StreamSpill::never(),
        };
        let running = call_id(1);
        call(&mut client, running, exec).await;
        assert_matches!(
            receive(&mut client).await,
            Some(WorkerFrame::Events { id, events: EventFrame::Exec(events) })
                if id == running && matches!(events[..], [ExecEvent::Started])
        );
        send(&mut client, ClientFrame::Cancel { id: running }).await;
        let resolve = Resolve {
            workspace: TargetPath::new(text),
            paths: vec![(".".into(), PathKind::Existing)],
        };
        let next = call_id(2);
        call(&mut client, next, resolve).await;
        let Some(WorkerFrame::Done { id, outcome }) = receive(&mut client).await else {
            panic!("expected the next call's end");
        };
        assert_eq!(id, next);
        assert_matches!(Resolve::answer(outcome), Some(Ok(_)));
        send(&mut client, ClientFrame::End).await;
        assert_matches!(receive(&mut client).await, Some(WorkerFrame::Ended));
        assert_eq!(ended(served).await, Ended::Requested);
    }
}
