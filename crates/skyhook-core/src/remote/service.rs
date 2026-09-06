//! Private shim control services. These messages never enter tool output or session events.
use super::{
    SensitivePrompt, SensitivePromptError, SensitivePromptFuture, SensitivePromptHandler,
    askpass::{AskpassServer, PromptAnswer},
    authentication::{AgentRelay, ProcessEnvironment},
    protocol::{Request, Response, write_frame},
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    sync::{Mutex, mpsc, oneshot},
};

type Answers = Arc<Mutex<HashMap<u64, oneshot::Sender<PromptAnswer>>>>;
struct PromptRegistration<W: AsyncWrite + Unpin + Send + 'static> {
    output: Arc<Mutex<W>>,
    answers: Answers,
    id: u64,
}
impl<W: AsyncWrite + Unpin + Send + 'static> Drop for PromptRegistration<W> {
    fn drop(&mut self) {
        let output = self.output.clone();
        let answers = self.answers.clone();
        let prompt_id = self.id;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                answers.lock().await.remove(&prompt_id);
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
        let prompt_id = self.next.fetch_add(1, Ordering::Relaxed);
        let output = self.output.clone();
        let answers = self.answers.clone();
        Box::pin(async move {
            let (sender, receiver) = oneshot::channel();
            answers.lock().await.insert(prompt_id, sender);
            let _registration = PromptRegistration {
                output: output.clone(),
                answers: answers.clone(),
                id: prompt_id,
            };
            if write_frame(
                &mut *output.lock().await,
                &Response::SensitivePrompt { prompt_id, prompt },
            )
            .await
            .is_err()
            {
                answers.lock().await.remove(&prompt_id);
                return Err(SensitivePromptError::Unavailable);
            }
            match receiver.await {
                Ok(PromptAnswer::Accepted(value)) => Ok(value),
                _ => Err(SensitivePromptError::Cancelled),
            }
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
    streams: HashMap<u64, WorkerStream>,
    pub tasks: tokio::task::JoinSet<()>,
    prompts: Arc<dyn SensitivePromptHandler>,
    pub environment: ProcessEnvironment,
    _askpass: AskpassServer,
    _agent: Option<AgentRelay>,
}
impl<W: AsyncWrite + Unpin + Send + 'static> WorkerServices<W> {
    pub fn new(output: Arc<Mutex<W>>) -> Result<Self, std::io::Error> {
        let answers = Answers::default();
        let prompts: Arc<dyn SensitivePromptHandler> = Arc::new(ForwardPrompts {
            output: output.clone(),
            answers: answers.clone(),
            next: AtomicU64::new(1),
        });
        let askpass = AskpassServer::start(prompts.clone())?;
        let mut environment = askpass.environment();
        let agent = std::env::var_os("SSH_AUTH_SOCK")
            .map(|path| AgentRelay::start(PathBuf::from(path)))
            .transpose()?;
        if let Some(agent) = &agent {
            environment.insert(
                "SSH_AUTH_SOCK".into(),
                agent.socket.to_string_lossy().into_owned(),
            );
        }
        Ok(Self {
            output,
            answers,
            streams: HashMap::new(),
            tasks: tokio::task::JoinSet::new(),
            prompts,
            environment,
            _askpass: askpass,
            _agent: agent,
        })
    }
    pub async fn handle(&mut self, request: Request) -> Result<(), std::io::Error> {
        match request {
            Request::SensitiveAnswer { prompt_id, answer } => {
                if let Some(sender) = self.answers.lock().await.remove(&prompt_id) {
                    let _ = sender.send(answer);
                }
            }
            Request::ResolveSsh { request_id, target } => {
                let output = self.output.clone();
                self.tasks.spawn(async move {
                    let result = super::ssh::resolve_local(&target)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = write_frame(
                        &mut *output.lock().await,
                        &Response::ResolvedSsh { request_id, result },
                    )
                    .await;
                });
            }
            Request::OpenSsh {
                channel,
                route,
                command,
            } => {
                if channel == 0 || self.streams.contains_key(&channel) {
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
                            stream = super::ssh::open(&route, &command, &environment, prompts) => stream?,
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
                    let _ = write_frame(&mut *output.lock().await, &Response::StreamClosed { channel, error: result.err().map(|e| e.to_string()) }).await;
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
