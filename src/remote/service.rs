//! Private shim control services. These messages never enter tool output or session events.
use super::{
    PromptAnswer, SensitivePrompt, SensitivePromptError, SensitivePromptFuture,
    SensitivePromptHandler,
    backend::ProcessEnvironment,
    flow::{self, CHUNK_BYTES, Credits, QUEUE},
    protocol::{
        ControlRequest, PromptId, RequestId, Response, Sequence, spawn_owned_write, write_frame,
    },
};
use std::{collections::HashMap, sync::Arc};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt as _},
    sync::{Mutex, mpsc, oneshot},
};

// Prompt registration cleanup must work even when dropped outside a runtime.
type Answers = Arc<std::sync::Mutex<HashMap<PromptId, oneshot::Sender<PromptAnswer>>>>;
struct PromptRegistration<W: AsyncWrite + Unpin + Send + 'static> {
    output: Arc<Mutex<W>>,
    answers: Answers,
    id: Option<PromptId>,
}
impl<W: AsyncWrite + Unpin + Send + 'static> PromptRegistration<W> {
    fn unregister(&mut self) -> Option<PromptId> {
        let id = self.id.take()?;
        let mut answers = self.answers.lock().expect("prompt answers poisoned");
        answers.remove(&id);
        Some(id)
    }
    fn complete(mut self) {
        self.unregister();
    }
}
impl<W: AsyncWrite + Unpin + Send + 'static> Drop for PromptRegistration<W> {
    fn drop(&mut self) {
        let Some(prompt_id) = self.unregister() else {
            return;
        };
        let output = self.output.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = write_frame(
                    &mut *output.lock().await,
                    &Response::SensitiveCancelled { prompt_id },
                )
                .await;
            });
        }
    }
}
struct ForwardPrompts<W> {
    output: Arc<Mutex<W>>,
    answers: Answers,
    ids: Sequence<PromptId>,
}
impl<W: AsyncWrite + Unpin + Send + 'static> SensitivePromptHandler for ForwardPrompts<W> {
    fn prompt(&self, prompt: SensitivePrompt) -> SensitivePromptFuture {
        let prompt_id = self.ids.next();
        let output = self.output.clone();
        let answers = self.answers.clone();
        Box::pin(async move {
            let prompt_id = prompt_id.ok_or(SensitivePromptError::Unavailable)?;
            let (sender, receiver) = oneshot::channel();
            answers
                .lock()
                .expect("prompt answers poisoned")
                .insert(prompt_id, sender);
            let registration = PromptRegistration {
                output: output.clone(),
                answers,
                id: Some(prompt_id),
            };
            // Dropping this caller cannot interrupt a partially written frame.
            let frame = Response::SensitivePrompt { prompt_id, prompt };
            if spawn_owned_write(output.lock_owned().await, frame)
                .await
                .is_err()
            {
                registration.complete();
                return Err(SensitivePromptError::Unavailable);
            }
            let result = receiver.await.map_err(|_| SensitivePromptError::Cancelled);
            registration.complete();
            result
        })
    }
}

struct WorkerStream {
    input: mpsc::Sender<Option<Vec<u8>>>,
    credit: Credits,
    cancellation: tokio_util::sync::CancellationToken,
}
impl Drop for WorkerStream {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

pub(super) struct WorkerServices<W> {
    output: Arc<Mutex<W>>,
    answers: Answers,
    streams: HashMap<RequestId, WorkerStream>,
    pub tasks: tokio::task::JoinSet<std::io::Result<()>>,
    prompts: Arc<dyn SensitivePromptHandler>,
    pub environment: ProcessEnvironment,
    authentication: super::ssh::WorkerAuthentication,
}
impl<W: AsyncWrite + Unpin + Send + 'static> WorkerServices<W> {
    pub fn new(output: Arc<Mutex<W>>) -> Result<Self, std::io::Error> {
        let answers = Answers::default();
        let prompts: Arc<dyn SensitivePromptHandler> = Arc::new(ForwardPrompts {
            output: output.clone(),
            answers: answers.clone(),
            ids: Sequence::default(),
        });
        let authentication = super::ssh::WorkerAuthentication::new(prompts.clone())?;
        let environment = authentication.environment().clone();
        Ok(Self {
            output,
            answers,
            streams: HashMap::new(),
            tasks: tokio::task::JoinSet::new(),
            prompts,
            environment,
            authentication,
        })
    }
    pub async fn handle(&mut self, request: ControlRequest) -> Result<(), std::io::Error> {
        match request {
            ControlRequest::SensitiveAnswer { prompt_id, answer } => {
                if let Some(sender) = self
                    .answers
                    .lock()
                    .expect("prompt answers poisoned")
                    .remove(&prompt_id)
                {
                    let _ = sender.send(answer);
                }
            }
            ControlRequest::OpenSsh {
                channel,
                route,
                command,
            } => {
                if self.streams.contains_key(&channel) {
                    return Err(std::io::Error::other("duplicate SSH stream"));
                }
                let (sender, input) = mpsc::channel(QUEUE);
                let credit = Credits::default();
                let cancellation = tokio_util::sync::CancellationToken::new();
                self.streams.insert(
                    channel,
                    WorkerStream {
                        input: sender,
                        credit: credit.clone(),
                        cancellation: cancellation.clone(),
                    },
                );
                let output = self.output.clone();
                let agent = self.authentication.agent();
                let prompts = self.prompts.clone();
                self.tasks.spawn(async move {
                    let result = async {
                        let open = async {
                            let environment = agent.route_environment(&route).await?;
                            super::ssh::open(&route, &command, &environment, prompts).await
                        };
                        let stream = tokio::select! {
                            stream = open => stream?,
                            () = cancellation.cancelled() => return Ok(()),
                        };
                        relay(stream, channel, input, &credit, &cancellation, &output)
                            .await
                            .map_err(super::error::RemoteError::from)
                    }
                    .await;
                    let error = result
                        .err()
                        .map(|error| error.into_tool_error().diagnostic());
                    write_frame(
                        &mut *output.lock().await,
                        &Response::StreamClosed { channel, error },
                    )
                    .await
                });
            }
            ControlRequest::StreamData { channel, data } => {
                if data.len() > CHUNK_BYTES {
                    return Err(std::io::Error::other("oversized stream chunk"));
                }
                // A closed input means the relay no longer takes input; it reports its end
                // with `StreamClosed`.
                if let Some(sender) = self.streams.get(&channel)
                    && let Err(mpsc::error::TrySendError::Full(_)) =
                        sender.input.try_send(Some(data))
                {
                    return Err(std::io::Error::other("SSH stream input overflow"));
                }
            }
            ControlRequest::StreamEnd { channel } => {
                if let Some(sender) = self.streams.get(&channel) {
                    let _ = sender.input.try_send(None);
                }
            }
            ControlRequest::StreamAck { channel } => {
                if let Some(stream) = self.streams.get(&channel) {
                    stream.credit.acknowledge()?;
                }
            }
            ControlRequest::StreamClose { channel } => {
                self.streams.remove(&channel);
            }
        }
        Ok(())
    }
}

/// Relay an open SSH stream until its output ends or the host closes it.
/// Each frame is written on its own task, so stopping the relay never leaves
/// a partial frame on the wire.
async fn relay<W: AsyncWrite + Unpin + Send + 'static>(
    stream: super::transport::Transport,
    channel: RequestId,
    mut input: mpsc::Receiver<Option<Vec<u8>>>,
    credit: &Credits,
    cancellation: &tokio_util::sync::CancellationToken,
    output: &Arc<Mutex<W>>,
) -> std::io::Result<()> {
    let super::transport::Transport {
        input: stdin,
        output: stdout,
        owner: _owner,
    } = stream;
    let write = async move {
        let mut stdin = Some(stdin);
        while let Some(data) = input.recv().await {
            match (data, &mut stdin) {
                (Some(data), Some(open)) => {
                    open.write_all(&data).await?;
                    let frame = Response::StreamAck { channel };
                    spawn_owned_write(output.clone().lock_owned().await, frame).await?;
                }
                (None, Some(open)) => {
                    open.shutdown().await?;
                    stdin = None;
                }
                (_, None) => return Err(std::io::Error::other("data after stream EOF")),
            }
        }
        Ok(())
    };
    let read = flow::pump(stdout, Some(credit), |data| async move {
        let Some(data) = data else { return Ok(()) };
        let frame = Response::StreamData { channel, data };
        spawn_owned_write(output.clone().lock_owned().await, frame).await
    });
    let mut read = std::pin::pin!(read);
    let written = tokio::select! {
        result = write => result,
        result = &mut read => return result,
        () = cancellation.cancelled() => return Ok(()),
    };
    // Input has stopped and later data for it is discarded, but the output still
    // carries the command's remaining result and SSH's exit diagnostic.
    tokio::select! {
        result = read => result.and(written),
        () = cancellation.cancelled() => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::protocol::read_frame;

    fn prompts<W>(output: W) -> ForwardPrompts<W> {
        ForwardPrompts {
            output: Arc::new(Mutex::new(output)),
            answers: Answers::default(),
            ids: Sequence::default(),
        }
    }

    fn password() -> SensitivePrompt {
        SensitivePrompt::test(crate::remote::SensitivePromptKind::Password)
    }

    #[tokio::test]
    async fn abandoned_partial_prompt_frame_is_finished_before_cancellation() {
        let (client, peer) = tokio::io::duplex(1);
        let mut peer = tokio::io::BufReader::new(peer);
        let prompts = prompts(client);
        let mut future = prompts.prompt(password());
        assert!(futures_util::poll!(&mut future).is_pending());
        // The frame's first byte arrives while the rest cannot fit the pipe.
        crate::tests::bounded(tokio::io::AsyncBufReadExt::fill_buf(&mut peer))
            .await
            .unwrap();
        drop(future);
        assert!(prompts.answers.lock().unwrap().is_empty());
        assert!(matches!(
            read_frame::<_, Response>(&mut peer).await.unwrap(),
            Some(Response::SensitivePrompt { prompt_id, .. }) if prompt_id == PromptId::new(1)
        ));
        assert!(matches!(
            read_frame::<_, Response>(&mut peer).await.unwrap(),
            Some(Response::SensitiveCancelled { prompt_id }) if prompt_id == PromptId::new(1)
        ));
    }

    #[tokio::test]
    async fn closing_a_stream_mid_frame_finishes_the_frame_first() {
        let (client, peer) = tokio::io::duplex(1);
        let mut peer = tokio::io::BufReader::new(peer);
        let output = Arc::new(Mutex::new(client));
        let (_input, received) = mpsc::channel(QUEUE);
        let stream = super::super::transport::Transport {
            input: Box::new(tokio::io::sink()),
            output: Box::new(&b"data"[..]),
            owner: Box::new(()),
        };
        let channel = RequestId::new(1);
        let cancellation = tokio_util::sync::CancellationToken::new();
        let credit = Credits::default();
        {
            let mut relay = std::pin::pin!(relay(
                stream,
                channel,
                received,
                &credit,
                &cancellation,
                &output
            ));
            assert!(futures_util::poll!(&mut relay).is_pending());
            // The frame's first byte arrives while the rest cannot fit the pipe.
            crate::tests::bounded(tokio::io::AsyncBufReadExt::fill_buf(&mut peer))
                .await
                .unwrap();
            cancellation.cancel();
            relay.await.unwrap();
        }
        let closing = async move {
            let closed = Response::StreamClosed {
                channel,
                error: None,
            };
            write_frame(&mut *output.lock().await, &closed).await
        };
        let reading = async {
            let mut frames = Vec::new();
            while let Some(frame) = read_frame::<_, Response>(&mut peer).await.unwrap() {
                frames.push(frame);
            }
            frames
        };
        let (closed, frames) =
            crate::tests::bounded(async { tokio::join!(closing, reading) }).await;
        closed.unwrap();
        assert!(matches!(
            frames.as_slice(),
            [
                Response::StreamData { data, .. },
                Response::StreamClosed { error: None, .. },
            ] if data == b"data"
        ));
    }

    #[tokio::test]
    async fn data_for_a_stream_that_stopped_taking_input_is_discarded() {
        let mut services = WorkerServices::new(Arc::new(Mutex::new(tokio::io::sink()))).unwrap();
        let channel = RequestId::new(1);
        let (input, received) = mpsc::channel(1);
        services.streams.insert(
            channel,
            WorkerStream {
                input,
                credit: Credits::default(),
                cancellation: tokio_util::sync::CancellationToken::new(),
            },
        );
        let data = || ControlRequest::StreamData {
            channel,
            data: b"data".to_vec(),
        };
        services.handle(data()).await.unwrap();
        assert!(services.handle(data()).await.is_err());
        drop(received);
        services.handle(data()).await.unwrap();
    }

    #[tokio::test]
    async fn output_is_drained_after_input_fails() {
        let (client, mut peer) = tokio::io::duplex(4096);
        let output = Arc::new(Mutex::new(client));
        let (stdin, _) = tokio::io::duplex(1);
        let (stdout, mut remote) = tokio::io::duplex(64);
        let (input, received) = mpsc::channel(QUEUE);
        input.try_send(Some(b"input".to_vec())).unwrap();
        let stream = super::super::transport::Transport {
            input: Box::new(stdin),
            output: Box::new(stdout),
            owner: Box::new(()),
        };
        let channel = RequestId::new(1);
        let cancellation = tokio_util::sync::CancellationToken::new();
        let credit = Credits::default();
        let mut relay = std::pin::pin!(relay(
            stream,
            channel,
            received,
            &credit,
            &cancellation,
            &output
        ));
        assert!(futures_util::poll!(&mut relay).is_pending());
        assert!(input.is_closed());
        remote.write_all(b"tail").await.unwrap();
        drop(remote);
        let error = crate::tests::bounded(relay).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        assert!(matches!(
            read_frame::<_, Response>(&mut peer).await.unwrap(),
            Some(Response::StreamData { data, .. }) if data == b"tail"
        ));
    }

    #[tokio::test]
    async fn successful_forwarded_answer_does_not_emit_late_cancellation() {
        let (client, mut peer) = tokio::io::duplex(4096);
        let prompts = prompts(client);
        let mut future = prompts.prompt(password());
        assert!(futures_util::poll!(&mut future).is_pending());
        let Some(Response::SensitivePrompt { prompt_id, .. }) =
            read_frame::<_, Response>(&mut peer).await.unwrap()
        else {
            panic!("expected submission")
        };
        prompts
            .answers
            .lock()
            .unwrap()
            .remove(&prompt_id)
            .unwrap()
            .send(PromptAnswer::Secret(crate::remote::SecretValue::new(
                "secret".into(),
            )))
            .unwrap();
        let PromptAnswer::Secret(secret) = future.await.unwrap() else {
            panic!("expected the forwarded secret")
        };
        assert_eq!(secret.expose(), "secret");
        assert!(prompts.answers.lock().unwrap().is_empty());
        // A pending cancellation task would hold the writer open and emit a frame.
        drop(prompts);
        assert!(matches!(
            read_frame::<_, Response>(&mut peer).await,
            Ok(None)
        ));
    }
}
