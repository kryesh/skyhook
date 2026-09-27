//! Flow-controlled SSH byte streams relayed over a shim connection.
use super::*;
use crate::{
    remote::{flow::WINDOW, protocol::ControlRequest, transport::ReportingReader},
    target::Route,
};
use tokio::io::AsyncWriteExt as _;

/// The in-process pipe between a stream's caller and its relay to the shim.
const PIPE_BYTES: usize = 64 * 1024;

pub(super) struct ClientStream {
    pub(super) output: tokio::sync::mpsc::Sender<Vec<u8>>,
    pub(super) credit: OwnedCredits,
    /// Reported to the stream's reader after its output.
    pub(super) closed: oneshot::Sender<Result<(), RemoteError>>,
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
                let close = Request::Control(ControlRequest::StreamClose { channel });
                let _ = write_frame(&mut *parent.writer.lock().await, &close).await;
            });
        }
    }
}
impl PooledConnection {
    /// Start SSH along `route` on this connection's machine, relaying its byte stream.
    pub(in crate::remote) async fn open_ssh(
        self: &Arc<Self>,
        route: Route,
        command: String,
    ) -> Result<Transport, RemoteError> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(WINDOW);
        let (closed, result) = oneshot::channel();
        let credit = Credits::default();
        // Owned from registration, so a stream dropped while its open request is
        // still being written is closed too.
        let (channel, mut owner) = self
            .submit(|channel, state| {
                let stream = ClientStream {
                    output: sender,
                    credit: OwnedCredits(credit.clone()),
                    closed,
                };
                state.streams.insert(channel, stream);
                let open = ControlRequest::OpenSsh {
                    channel,
                    route: Box::new(route),
                    command,
                };
                let owner = StreamOwner {
                    parent: self.clone(),
                    channel,
                    tasks: Vec::new(),
                };
                (Request::Control(open), owner)
            })
            .await?;
        let (client, peer) = tokio::io::duplex(PIPE_BYTES);
        let (input, mut output) = tokio::io::split(peer);
        let parent = self.clone();
        let write_task = tokio::spawn(async move {
            let parent = &parent;
            let _ = flow::pump(input, Some(&credit), |data| async move {
                let request = match data {
                    Some(data) => ControlRequest::StreamData { channel, data },
                    None => ControlRequest::StreamEnd { channel },
                };
                let writer = parent.writer.clone().lock_owned().await;
                write_request(writer, &parent.state, Request::Control(request)).await
            })
            .await;
        });
        let parent = self.clone();
        let read_task = tokio::spawn(async move {
            while let Some(bytes) = receiver.recv().await {
                if output.write_all(&bytes).await.is_err() {
                    break;
                }
                let writer = parent.writer.clone().lock_owned().await;
                let acknowledgement = Request::Control(ControlRequest::StreamAck { channel });
                if write_request(writer, &parent.state, acknowledgement)
                    .await
                    .is_err()
                {
                    break;
                }
            }
            let _ = output.shutdown().await;
        });
        owner.tasks = vec![write_task.abort_handle(), read_task.abort_handle()];
        let (output, input) = tokio::io::split(client);
        Ok(Transport {
            input: Box::new(input),
            output: Box::new(ReportingReader::new(output, result)),
            owner: Box::new(owner),
        })
    }
}
