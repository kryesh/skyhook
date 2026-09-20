//! One bounded payload transport per connection; only the host persists output.
use std::{
    io::{self, Write},
    num::NonZeroU64,
    sync::Arc,
};

use tokio::{
    io::AsyncWrite,
    sync::{Mutex, mpsc, oneshot},
};

use crate::remote::{
    flow::{CHUNK_BYTES, Credits, WINDOW},
    protocol::{
        ImageId, PayloadEvent, PayloadId, PayloadOpen, RemoteToolResult, RequestId, Response,
        write_frame,
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
    next_image: Arc<std::sync::Mutex<Option<ImageId>>>,
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
            next_image: Arc::new(std::sync::Mutex::new(Some(ImageId(NonZeroU64::MIN)))),
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
                let id = {
                    let mut next = self
                        .next_image
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let id = next.ok_or_else(|| io::Error::other("image ID space exhausted"))?;
                    *next = id.0.checked_add(1).map(ImageId);
                    id
                };
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

    pub(crate) async fn forward<W: AsyncWrite + Unpin + Send + 'static>(
        mut self,
        writer: Arc<Mutex<W>>,
    ) -> io::Result<()> {
        while let Some(event) = self.receiver.recv().await {
            match event {
                Outgoing::Payload { request_id, event } => {
                    self.credits.take().await?;
                    write_frame(
                        &mut *writer.lock().await,
                        &Response::Payload { request_id, event },
                    )
                    .await?;
                }
                Outgoing::Complete {
                    request_id,
                    delivered,
                } => {
                    let result =
                        write_frame(&mut *writer.lock().await, &Response::Tool { request_id })
                            .await;
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
    use crate::remote::protocol::{PromptId, RemoteToolOutput, read_frame};
    use crate::tool::output::{CaptureEvent, CaptureId};

    async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(std::time::Duration::from_secs(5), future)
            .await
            .unwrap()
    }

    #[test]
    fn result_serialization_progresses_with_a_saturated_single_thread_blocking_pool() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let bytes = CHUNK_BYTES + 1;
        runtime.block_on(async {
            let (sender, receiver) = PayloadSender::new();
            let result = sender.request(RequestId::FIRST);
            let competing = sender.request(RequestId::FIRST.next().unwrap());
            let output = competing.context();
            let capture = output
                .pending_stream_capture("/result/stdout", crate::tool::output::CaptureKind::Text)
                .await
                .unwrap();
            let completion = tokio::spawn(result.finish(Ok(RemoteToolOutput {
                value: serde_json::json!({"value": "x".repeat(bytes)}),
                images: Vec::new(),
                captures: Vec::new(),
                streams: Default::default(),
            })));
            bounded(async {
                while sender.0.capacity() == WINDOW {
                    tokio::task::yield_now().await;
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
                    tokio::task::yield_now().await;
                }
            })
            .await;
            let credits = receiver.credits();
            let (input, mut peer) = tokio::io::duplex(4096);
            let forward = tokio::spawn(receiver.forward(Arc::new(Mutex::new(input))));
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
                        Response::Tool {
                            request_id: RequestId::FIRST,
                        } => {
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
        let forward = tokio::spawn(receiver.forward(writer.clone()));
        let mut producers = Vec::new();
        for request_id in [RequestId::FIRST, RequestId::FIRST.next().unwrap()] {
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
                tokio::task::yield_now().await;
            }
        })
        .await;
        // No host ACKs: persistence has made no further progress. Control must
        // still acquire the writer and pass the credit-blocked output forwarder.
        write_frame(
            &mut *writer.lock().await,
            &Response::SensitiveCancelled {
                prompt_id: PromptId(7),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            bounded(read_frame::<_, Response>(&mut output))
                .await
                .unwrap(),
            Some(Response::SensitiveCancelled {
                prompt_id: PromptId(7)
            })
        ));
        forward.abort();
        assert!(bounded(forward).await.unwrap_err().is_cancelled());
        for producer in producers {
            assert!(bounded(producer).await.unwrap().is_err());
        }
        assert!(credits.take().await.is_err());
    }
}
