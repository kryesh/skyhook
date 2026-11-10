//! Relaying a stream this machine starts as a route's origin.
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::op::{CHUNK_BYTES, Events, Input, Open, Operation, StreamOutput};
use crate::origin::{Leg, error::TransportError, transport::Transport};

impl Operation for Open {
    /// The command's output.
    type Event = StreamOutput;
    type End = ();
    type Error = TransportError;

    fn input(&mut self) -> Option<&mut Input> {
        Some(&mut self.input)
    }

    /// Start SSH along the leg's route, or connect from here to its WinRM listener,
    /// and relay the command's stream.
    async fn perform(
        self,
        local: &super::Local,
        events: Events<StreamOutput>,
    ) -> Result<(), TransportError> {
        let stream = match &self.leg {
            Leg::Ssh(route) => {
                let origin = local.origin();
                origin.open(route, &self.command, local.work()).await?
            }
            Leg::Winrm(route) => {
                let client = route.connect(local.work()).await?;
                crate::origin::winrm::open(route, &client, &self.command).await?
            }
        };
        relay(stream, self.input, events).await
    }
}

/// Relay `stream` until its output ends; input that ends first closes its standard
/// input, and output keeps arriving.
async fn relay(
    stream: Transport,
    input: Input,
    events: Events<StreamOutput>,
) -> Result<(), TransportError> {
    let Transport {
        input: mut stdin,
        output: mut stdout,
        owner: _owner,
    } = stream;
    let mut input = input.into_reader();
    let write = async move {
        tokio::io::copy(&mut input, &mut stdin).await?;
        stdin.shutdown().await
    };
    let read = async {
        let mut buffer = vec![0; CHUNK_BYTES];
        loop {
            let read = stdout.read(&mut buffer).await?;
            if read == 0 {
                return Ok::<_, std::io::Error>(());
            }
            if events
                .send(StreamOutput(buffer[..read].to_vec()))
                .await
                .is_err()
            {
                return Ok(());
            }
        }
    };
    tokio::pin!(read);
    let written = tokio::select! {
        written = write => written,
        read = &mut read => return read.map_err(TransportError::from),
    };
    // Input has stopped, but the output still carries the command's remaining
    // result and the connection's exit diagnostic.
    read.await.and(written).map_err(TransportError::from)
}
