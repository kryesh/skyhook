//! Write a connection's frames, and route a worker's to the calls they answer,
//! without blocking frame reads.
use super::*;
use futures_util::FutureExt as _;
use tokio::sync::mpsc::error::TrySendError;

/// Run the connection: `writing` writes what `outbox` sends while the worker's
/// frames are routed, until the transport fails or `shutdown` ends the connection.
pub(super) async fn route_responses<R>(
    mut output: FrameReader<R>,
    writing: impl Future<Output = std::io::Result<()>>,
    outbox: Outbox<ClientFrame>,
    state: &Mutex<ConnectionState>,
    shutdown: CancellationToken,
    owner: Box<dyn Send>,
) where
    R: AsyncRead + Unpin,
{
    // Never done while `outbox` is held, but polled again after the end is sent.
    let writing = writing.fuse();
    tokio::pin!(writing);
    let error = tokio::select! {
        Err(error) = &mut writing => transport_error(error, Operation::Send),
        Err(error) = route(&mut output, state) => error,
        () = shutdown.cancelled() => {
            // Ending the connection on purpose: the worker stops its calls, then
            // acknowledges the end. A transport that stalls any of it, the frames
            // queued before the end included, is released once the grace is over.
            outbox.send(ClientFrame::End);
            let grace = skyhook_worker::protocol::END_GRACE + lock(state).ending;
            let end = async {
                tokio::select! {
                    _ = &mut writing => {}
                    _ = route(&mut output, state) => {}
                }
            };
            let _ = tokio::time::timeout(grace, end).await;
            RemoteFault::Cancelled.into()
        }
    };
    drop(owner);
    fail_connection(state, transport_error(error, Operation::Receive));
}

async fn route<R>(
    output: &mut FrameReader<R>,
    state: &Mutex<ConnectionState>,
) -> Result<(), RemoteError>
where
    R: AsyncRead + Unpin,
{
    loop {
        let frame = (output.next::<WorkerFrame>().await)
            .map_err(|error| transport_error(error, Operation::Receive))?
            .ok_or(Violation::ClosedBeforeReply)?;
        match frame {
            WorkerFrame::Events { id, events } => {
                let state = lock(state);
                // A call dropped since discards what was still in flight for it.
                let overflow = (state.calls.get(&id)).is_some_and(|call| {
                    matches!(call.events.try_send(events), Err(TrySendError::Full(_)))
                });
                if overflow {
                    return Err(Violation::EventOverflow.into());
                }
            }
            WorkerFrame::InputAck { id } => {
                let state = lock(state);
                let invalid = (state.calls.get(&id)).is_some_and(|call| {
                    (call.input.as_ref()).is_none_or(|credits| !credits.acknowledge())
                });
                if invalid {
                    return Err(Violation::InputCredit.into());
                }
            }
            WorkerFrame::Performing { id, resolved } => {
                land(state, id, Landed::Performing(resolved))?;
            }
            WorkerFrame::Held { id, resolved } => land(state, id, Landed::Held(resolved))?,
            WorkerFrame::Done { id, outcome } => {
                let call = lock(state).calls.remove(&id);
                if let Some(call) = call {
                    let _ = call.done.send(Ok(outcome));
                }
            }
            WorkerFrame::Ended => {
                return Err(RemoteFault::Cancelled.into());
            }
            WorkerFrame::Ready(_) | WorkerFrame::Refused(_) => {
                return Err(Violation::SecondHandshake.into());
            }
        }
    }
}

/// Tell contained call `id` where its paths resolved; a call not contained, or told
/// already, has no such answer.
fn land(state: &Mutex<ConnectionState>, id: CallId, landed: Landed) -> Result<(), RemoteError> {
    let mut state = lock(state);
    // A call dropped since discards its answer.
    let Some(call) = state.calls.get_mut(&id) else {
        return Ok(());
    };
    let landing = call.landing.take().ok_or(Violation::ForeignAnswer)?;
    let _ = landing.send(landed);
    Ok(())
}
