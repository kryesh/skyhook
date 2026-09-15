//! Flow-controlled SSH byte streams relayed over a shim connection.
use super::*;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

pub(super) struct ClientStream {
    pub(super) output: tokio::sync::mpsc::Sender<Result<Vec<u8>, RemoteError>>,
    pub(super) credit: Arc<tokio::sync::Semaphore>,
}
impl Drop for ClientStream {
    fn drop(&mut self) {
        self.credit.close();
    }
}

struct StreamOwner {
    parent: Arc<PooledConnection>,
    channel: RequestId,
    tasks: Vec<tokio::task::AbortHandle>,
}
impl Drop for StreamOwner {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        let parent = self.parent.clone();
        let channel = self.channel;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                parent.state.lock().await.streams.remove(&channel);
                let _ = write_frame(
                    &mut parent.writer.lock().await.input,
                    &Request::StreamClose { channel },
                )
                .await;
            });
        }
    }
}
impl PooledConnection {
    pub(super) async fn open_stream(
        self: &Arc<Self>,
        route: Vec<TargetDefinition>,
        command: String,
    ) -> Result<crate::remote::transport::Transport, RemoteError> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(256);
        let credit = Arc::new(tokio::sync::Semaphore::new(16));
        let (channel, ()) = self
            .submit(|channel, state| {
                state.streams.insert(
                    channel,
                    ClientStream {
                        output: sender,
                        credit: credit.clone(),
                    },
                );
                (
                    Request::OpenSsh {
                        channel,
                        route,
                        command,
                    },
                    (),
                )
            })
            .await?;
        let (client, peer) = tokio::io::duplex(64 * 1024);
        let (mut input, mut output) = tokio::io::split(peer);
        let parent = self.clone();
        let write_task = tokio::spawn(async move {
            let mut bytes = vec![0; 32 * 1024];
            while let Ok(count) = input.read(&mut bytes).await {
                let Ok(permit) = credit.acquire().await else {
                    break;
                };
                permit.forget();
                let request = if count == 0 {
                    Request::StreamEnd { channel }
                } else {
                    Request::StreamData {
                        channel,
                        data: bytes[..count].to_vec(),
                    }
                };
                let writer = parent.writer.clone().lock_owned().await;
                if parent.write(writer, request).await.is_err() || count == 0 {
                    break;
                }
            }
        });
        let parent = self.clone();
        let failure = Arc::new(std::sync::Mutex::new(None));
        let read_failure = failure.clone();
        let read_task = tokio::spawn(async move {
            while let Some(result) = receiver.recv().await {
                let bytes = match result {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        *read_failure
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(error.to_string());
                        break;
                    }
                };
                if output.write_all(&bytes).await.is_err() {
                    break;
                }
                let writer = parent.writer.clone().lock_owned().await;
                if parent
                    .write(writer, Request::StreamAck { channel })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            let _ = output.shutdown().await;
        });
        let (output, input) = tokio::io::split(client);
        Ok(crate::remote::transport::Transport {
            input: Box::new(input),
            output: Box::new(crate::remote::transport::RelayedReader { output, failure }),
            owner: Box::new(StreamOwner {
                parent: self.clone(),
                channel,
                tasks: vec![write_task.abort_handle(), read_task.abort_handle()],
            }),
        })
    }
}
