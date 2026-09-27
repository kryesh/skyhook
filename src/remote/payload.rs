//! One bounded payload transport per connection; only the host persists output.
use std::{
    io::{self, Write},
    sync::Arc,
};

use tokio::{
    io::AsyncWrite,
    sync::{Mutex, mpsc, oneshot},
};

use crate::remote::{
    flow::{self, CHUNK_BYTES, Credits, WINDOW},
    protocol::{
        ImageId, PayloadEvent, PayloadId, PayloadOpen, RemoteToolResult, RequestId, Response,
        Sequence, write_frame_within,
    },
};
use crate::tool::output::{OutputContext, OutputEvent, OutputSink};

enum Outgoing {
    Payload {
        request_id: RequestId,
        event: PayloadEvent,
    },
    Complete {
        request_id: RequestId,
        delivered: oneshot::Sender<io::Result<()>>,
    },
}

#[derive(Clone)]
pub(crate) struct PayloadSender(mpsc::Sender<Outgoing>);

pub(crate) struct PayloadReceiver {
    receiver: mpsc::Receiver<Outgoing>,
    credits: Credits,
}

#[derive(Clone)]
pub(crate) struct RequestOutput {
    sender: PayloadSender,
    request_id: RequestId,
    images: Arc<Sequence<ImageId>>,
}

impl PayloadSender {
    pub(crate) fn new() -> (Self, PayloadReceiver) {
        let (sender, receiver) = mpsc::channel(WINDOW);
        (
            Self(sender),
            PayloadReceiver {
                receiver,
                credits: Credits::default(),
            },
        )
    }

    pub(crate) fn request(&self, request_id: RequestId) -> RequestOutput {
        RequestOutput {
            sender: self.clone(),
            request_id,
            images: Arc::default(),
        }
    }
}

impl RequestOutput {
    pub(crate) fn context(&self) -> OutputContext {
        OutputContext::new(Arc::new(self.clone()))
    }

    pub(crate) async fn finish(self, result: RemoteToolResult) -> io::Result<()> {
        let producer = self.clone();
        tokio::task::spawn_blocking(move || -> io::Result<()> {
            producer.payload(PayloadEvent::Open(PayloadOpen::Result))?;
            let mut writer = std::io::BufWriter::with_capacity(CHUNK_BYTES, ChunkWriter(&producer));
            serde_json::to_writer(&mut writer, &result)?;
            writer.flush()?;
            producer.payload(PayloadEvent::Finish {
                id: PayloadId::Result,
            })?;
            Ok(())
        })
        .await
        .map_err(io::Error::other)??;
        let (delivered, receipt) = oneshot::channel();
        self.sender
            .0
            .send(Outgoing::Complete {
                request_id: self.request_id,
                delivered,
            })
            .await
            .map_err(io::Error::other)?;
        receipt.await.map_err(io::Error::other)?
    }

    /// Stream a source file's contents to the host in flow-controlled chunks.
    /// Every send happens on the calling task, so dropping it stops the stream
    /// before the request's terminal result is sent.
    pub(crate) async fn send_source(&self, file: std::fs::File) -> io::Result<()> {
        self.send_payload(PayloadEvent::Open(PayloadOpen::Source))
            .await?;
        let id = PayloadId::Source;
        flow::pump(tokio::fs::File::from_std(file), None, |data| async move {
            let event = match data {
                Some(data) => PayloadEvent::Data { id, data },
                None => PayloadEvent::Finish { id },
            };
            self.send_payload(event).await
        })
        .await
    }

    async fn send_payload(&self, event: PayloadEvent) -> io::Result<()> {
        self.sender
            .0
            .send(Outgoing::Payload {
                request_id: self.request_id,
                event,
            })
            .await
            .map_err(io::Error::other)
    }

    fn payload(&self, event: PayloadEvent) -> io::Result<()> {
        self.sender
            .0
            .blocking_send(Outgoing::Payload {
                request_id: self.request_id,
                event,
            })
            .map_err(io::Error::other)
    }
}

impl OutputSink for RequestOutput {
    fn send(&self, event: OutputEvent) -> io::Result<()> {
        let event = match event {
            OutputEvent::Capture(event) => PayloadEvent::Capture(event),
            OutputEvent::Image { file, image } => {
                let id = (self.images.next())
                    .ok_or_else(|| io::Error::other("image ID space exhausted"))?;
                self.payload(PayloadEvent::Open(PayloadOpen::Image { id, file }))?;
                for data in image.bytes().chunks(CHUNK_BYTES) {
                    self.payload(PayloadEvent::Data {
                        id: PayloadId::Image(id),
                        data: data.to_vec(),
                    })?;
                }
                return self.payload(PayloadEvent::Finish {
                    id: PayloadId::Image(id),
                });
            }
        };
        self.payload(event)
    }
}

impl PayloadReceiver {
    pub(crate) fn credits(&self) -> Credits {
        self.credits.clone()
    }

    /// Write the queued payloads as frames of at most `frame_limit` bytes.
    pub(crate) async fn forward<W: AsyncWrite + Unpin + Send + 'static>(
        mut self,
        writer: Arc<Mutex<W>>,
        frame_limit: usize,
    ) -> io::Result<()> {
        while let Some(event) = self.receiver.recv().await {
            match event {
                Outgoing::Payload { request_id, event } => {
                    self.credits.take().await?;
                    write_frame_within(
                        &mut *writer.lock().await,
                        &Response::Payload { request_id, event },
                        frame_limit,
                    )
                    .await?;
                }
                Outgoing::Complete {
                    request_id,
                    delivered,
                } => {
                    let terminal = Response::Tool { request_id };
                    let result =
                        write_frame_within(&mut *writer.lock().await, &terminal, frame_limit).await;
                    let receipt = result
                        .as_ref()
                        .copied()
                        .map_err(|error| io::Error::new(error.kind(), error.to_string()));
                    let _ = delivered.send(receipt);
                    result?;
                }
            }
        }
        Ok(())
    }
}

impl Drop for PayloadReceiver {
    fn drop(&mut self) {
        self.credits.close();
    }
}

struct ChunkWriter<'a>(&'a RequestOutput);
impl Write for ChunkWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for chunk in bytes.chunks(CHUNK_BYTES) {
            self.0.payload(PayloadEvent::Data {
                id: PayloadId::Result,
                data: chunk.to_vec(),
            })?;
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::protocol::{
        MAX_FRAME_BYTES, PromptId, RemoteToolOutput, read_frame, write_frame,
    };
    use crate::tests::bounded;
    use crate::tool::output::{CaptureEvent, CaptureId};

    /// Channel capacity has no change notification, so waits on it poll.
    const POLL: std::time::Duration = std::time::Duration::from_millis(10);

    #[test]
    fn result_serialization_progresses_with_a_saturated_single_thread_blocking_pool() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        // The result is larger than a frame, so only chunking can carry it.
        let frame_limit = 2 * CHUNK_BYTES;
        let bytes = frame_limit + 1;
        runtime.block_on(async {
            let (sender, receiver) = PayloadSender::new();
            let result = sender.request(RequestId::new(1));
            let competing = sender.request(RequestId::new(2));
            let output = competing.context();
            let capture = output
                .pending_stream_capture(
                    crate::tool::output::FieldPointer::result().property("stdout"),
                    crate::tool::output::CaptureKind::Text,
                )
                .await
                .unwrap();
            let completion = tokio::spawn(result.finish(Ok(RemoteToolOutput {
                diagnostic: None,
                value: serde_json::json!({"value": "x".repeat(bytes)}),
                images: Vec::new(),
                captures: Vec::new(),
                streams: Default::default(),
            })));
            bounded(async {
                while sender.0.capacity() == WINDOW {
                    tokio::time::sleep(POLL).await;
                }
            })
            .await;
            let producer = tokio::task::spawn_blocking(move || {
                let mut capture = capture.open();
                for _ in 0..WINDOW * 3 {
                    capture.write_all(&vec![b'x'; CHUNK_BYTES])?;
                }
                capture.finish()
            });
            // Either serializer or capture producer owns the only blocking
            // thread. Forwarding must drain it without another blocking task.
            bounded(async {
                while sender.0.capacity() != 0 {
                    tokio::time::sleep(POLL).await;
                }
            })
            .await;
            let credits = receiver.credits();
            let (input, mut peer) = tokio::io::duplex(4096);
            let forward = receiver.forward(Arc::new(Mutex::new(input)), frame_limit);
            let forward = tokio::spawn(forward);
            let received = tokio::spawn(async move {
                let mut streamed_bytes = 0;
                let mut terminal = false;
                while let Some(frame) = read_frame::<_, Response>(&mut peer).await.unwrap() {
                    match frame {
                        Response::Payload { event, .. } => {
                            if let PayloadEvent::Data {
                                id: PayloadId::Result,
                                data,
                            } = event
                            {
                                streamed_bytes += data.len();
                            }
                            credits.acknowledge().unwrap();
                        }
                        Response::Tool { request_id } if request_id == RequestId::new(1) => {
                            assert!(streamed_bytes > bytes);
                            terminal = true;
                        }
                        other => panic!("unexpected payload frame: {other:?}"),
                    }
                }
                terminal
            });
            bounded(completion).await.unwrap().unwrap();
            bounded(producer).await.unwrap().unwrap();
            output.settle().await.unwrap();
            drop((output, competing, sender));
            bounded(forward).await.unwrap().unwrap();
            assert!(bounded(received).await.unwrap());
        });
    }

    #[tokio::test]
    async fn connection_payload_window_bounds_multiple_producers_without_blocking_control() {
        let (sender, receiver) = PayloadSender::new();
        let credits = receiver.credits();
        let (input, mut output) = tokio::io::duplex(4096);
        let writer = Arc::new(Mutex::new(input));
        let forward = receiver.forward(writer.clone(), MAX_FRAME_BYTES);
        let forward = tokio::spawn(forward);
        let mut producers = Vec::new();
        for request_id in [RequestId::new(1), RequestId::new(2)] {
            let producer = sender.request(request_id);
            producers.push(tokio::task::spawn_blocking(move || {
                for _ in 0..WINDOW * 3 {
                    producer.send(OutputEvent::Capture(CaptureEvent::Write {
                        id: CaptureId::FIRST,
                        data: vec![b'x'; CHUNK_BYTES],
                    }))?;
                }
                Ok::<(), io::Error>(())
            }));
        }
        for _ in 0..WINDOW {
            assert!(matches!(
                bounded(read_frame::<_, Response>(&mut output))
                    .await
                    .unwrap(),
                Some(Response::Payload { .. })
            ));
        }
        bounded(async {
            while sender.0.capacity() != 0 {
                tokio::time::sleep(POLL).await;
            }
        })
        .await;
        // No host ACKs: persistence has made no further progress. Control must
        // still acquire the writer and pass the credit-blocked output forwarder.
        write_frame(
            &mut *writer.lock().await,
            &Response::SensitiveCancelled {
                prompt_id: PromptId::new(7),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            bounded(read_frame::<_, Response>(&mut output))
                .await
                .unwrap(),
            Some(Response::SensitiveCancelled { prompt_id }) if prompt_id == PromptId::new(7)
        ));
        forward.abort();
        assert!(bounded(forward).await.unwrap_err().is_cancelled());
        for producer in producers {
            assert!(bounded(producer).await.unwrap().is_err());
        }
        assert!(credits.take().await.is_err());
    }
}
