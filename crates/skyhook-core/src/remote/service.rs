//! Private shim control services. These messages never enter tool output or session events.
use super::{
    SensitivePrompt, SensitivePromptError, SensitivePromptFuture, SensitivePromptHandler,
    backend::{ProcessEnvironment, WorkerBackends},
    prompt::PromptAnswer,
    protocol::{PromptId, Request, RequestId, Response, spawn_owned_write, write_frame},
};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
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
    next: AtomicU64,
}
impl<W: AsyncWrite + Unpin + Send + 'static> SensitivePromptHandler for ForwardPrompts<W> {
    fn prompt(&self, prompt: SensitivePrompt) -> SensitivePromptFuture {
        let prompt_id = PromptId(self.next.fetch_add(1, Ordering::Relaxed));
        let output = self.output.clone();
        let answers = self.answers.clone();
        Box::pin(async move {
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
            let result = match receiver.await {
                Ok(PromptAnswer::Accepted(value)) => Ok(value),
                _ => Err(SensitivePromptError::Cancelled),
            };
            registration.complete();
            result
        })
    }
}

struct WorkerStream {
    input: mpsc::Sender<Option<Vec<u8>>>,
    credit: Arc<tokio::sync::Semaphore>,
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
    _backends: WorkerBackends,
}
impl<W: AsyncWrite + Unpin + Send + 'static> WorkerServices<W> {
    pub fn new(output: Arc<Mutex<W>>) -> Result<Self, std::io::Error> {
        let answers = Answers::default();
        let prompts: Arc<dyn SensitivePromptHandler> = Arc::new(ForwardPrompts {
            output: output.clone(),
            answers: answers.clone(),
            next: AtomicU64::new(1),
        });
        let backends = WorkerBackends::new(prompts.clone())?;
        let environment = backends.environment().clone();
        Ok(Self {
            output,
            answers,
            streams: HashMap::new(),
            tasks: tokio::task::JoinSet::new(),
            prompts,
            environment,
            _backends: backends,
        })
    }
    pub async fn handle(&mut self, request: Request) -> Result<(), std::io::Error> {
        match request {
            Request::SensitiveAnswer { prompt_id, answer } => {
                if let Some(sender) = self
                    .answers
                    .lock()
                    .expect("prompt answers poisoned")
                    .remove(&prompt_id)
                {
                    let _ = sender.send(answer);
                }
            }
            Request::ResolveSsh { request_id, target } => {
                let output = self.output.clone();
                self.tasks.spawn(async move {
                    let result = super::backend::resolve_local(&target)
                        .await
                        .map_err(|e| e.to_string());
                    write_frame(
                        &mut *output.lock().await,
                        &Response::ResolvedSsh { request_id, result },
                    )
                    .await
                });
            }
            Request::OpenSsh {
                channel,
                route,
                command,
            } => {
                if self.streams.contains_key(&channel) {
                    return Err(std::io::Error::other("duplicate SSH stream"));
                }
                let (sender, mut input) = mpsc::channel(128);
                let credit = Arc::new(tokio::sync::Semaphore::new(16));
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
                let environment = self.environment.clone();
                let prompts = self.prompts.clone();
                self.tasks.spawn(async move {
                    let result = async {
                        let stream = tokio::select! {
                            stream = super::backend::open_ssh_request(&route, &command, &environment, prompts) => stream?,
                            () = cancellation.cancelled() => return Ok(()),
                        };
                        let super::transport::Transport { input: stdin, output: mut stdout, owner: _owner } = stream;
                        let output_writer = output.clone();
                        let write = async move {
                            let mut ended = false;
                            let mut stdin = Some(stdin);
                            while let Some(data) = input.recv().await {
                                match data {
                                    Some(data) if !ended => {
                                        stdin.as_mut().expect("open stream").write_all(&data).await?;
                                        write_frame(&mut *output_writer.lock().await, &Response::StreamAck {channel}).await?;
                                    }
                                    None if !ended => { if let Some(mut stdin) = stdin.take() { stdin.shutdown().await?; } ended = true; }
                                    _ => return Err(std::io::Error::other("data after stream EOF")),
                                }
                            }
                            Ok::<(), std::io::Error>(())
                        };
                        let read = async {
                            let mut bytes = vec![0;32*1024];
                            loop {
                                let permit = credit.acquire().await.map_err(std::io::Error::other)?;
                                let count = stdout.read(&mut bytes).await?;
                                if count == 0 { break; }
                                permit.forget();
                                write_frame(&mut *output.lock().await, &Response::StreamData {channel,data:bytes[..count].to_vec()}).await?;
                            }
                            Ok::<(), std::io::Error>(())
                        };
                        tokio::select! {
                            result = write => { result?; }
                            result = read => { result?; }
                            () = cancellation.cancelled() => {}
                        }
                        Ok::<(), super::RemoteError>(())
                    }.await;
                    write_frame(&mut *output.lock().await, &Response::StreamClosed { channel, error: result.err().map(|e| e.to_string()) }).await
                });
            }
            Request::StreamData { channel, data } => {
                if data.len() > 32 * 1024 {
                    return Err(std::io::Error::other("oversized stream chunk"));
                }
                if let Some(sender) = self.streams.get(&channel) {
                    sender.input.try_send(Some(data)).map_err(|_| {
                        std::io::Error::other("SSH stream input overflow or closed")
                    })?;
                }
            }
            Request::StreamEnd { channel } => {
                if let Some(sender) = self.streams.get(&channel) {
                    let _ = sender.input.try_send(None);
                }
            }
            Request::StreamAck { channel } => {
                if let Some(stream) = self.streams.get(&channel) {
                    if stream.credit.available_permits() >= 16 {
                        return Err(std::io::Error::other("invalid stream credit"));
                    }
                    stream.credit.add_permits(1);
                }
            }
            Request::StreamClose { channel } => {
                self.streams.remove(&channel);
            }
            _ => return Err(std::io::Error::other("unexpected control request")),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::protocol::read_frame;

    fn prompts<W>(output: W, next: u64) -> ForwardPrompts<W> {
        ForwardPrompts {
            output: Arc::new(Mutex::new(output)),
            answers: Answers::default(),
            next: AtomicU64::new(next),
        }
    }

    fn password() -> SensitivePrompt {
        SensitivePrompt {
            kind: crate::remote::SensitivePromptKind::Password,
            message: "password".into(),
        }
    }

    #[tokio::test]
    async fn abandoned_partial_prompt_frame_is_finished_before_cancellation() {
        let (client, mut peer) = tokio::io::duplex(1);
        let prompts = prompts(client, 1);
        let mut future = prompts.prompt(password());
        assert!(futures_util::poll!(&mut future).is_pending());
        tokio::task::yield_now().await;
        drop(future);
        assert!(prompts.answers.lock().unwrap().is_empty());
        assert!(matches!(
            read_frame::<_, Response>(&mut peer).await.unwrap(),
            Some(Response::SensitivePrompt {
                prompt_id: PromptId(1),
                ..
            })
        ));
        assert!(matches!(
            read_frame::<_, Response>(&mut peer).await.unwrap(),
            Some(Response::SensitiveCancelled {
                prompt_id: PromptId(1)
            })
        ));
    }

    #[tokio::test]
    async fn successful_forwarded_answer_does_not_emit_late_cancellation() {
        let (client, mut peer) = tokio::io::duplex(4096);
        let prompts = prompts(client, 1);
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
            .send(PromptAnswer::Accepted(crate::remote::SecretValue::new(
                "secret".into(),
            )))
            .unwrap();
        assert_eq!(future.await.unwrap().expose(), "secret");
        assert!(prompts.answers.lock().unwrap().is_empty());
        // A pending cancellation task would hold the writer open and emit a frame.
        drop(prompts);
        assert!(matches!(
            read_frame::<_, Response>(&mut peer).await,
            Ok(None)
        ));
    }
}
