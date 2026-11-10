//! The frames a client and a worker exchange: typed operations, their events and
//! ends, flow control, and the handshake and end that bound a connection, and the
//! serve loop that answers them on this machine.

pub mod flow;
mod frame;
pub mod serve;
use std::{
    num::NonZeroU64,
    sync::{Mutex, PoisonError},
};

use serde::{Deserialize, Serialize};

use crate::op::{self, Operation};

pub(crate) use frame::encoded_bytes;
pub use frame::{FrameReader, FrameWriter, Outbox, write_frame};

/// What a worker writes before its first frame, so that its client can skip whatever
/// a shell printed before the worker started, and tell a started worker from a shell
/// that printed only a probe.
pub const PREAMBLE: &[u8] = b"SKYHOOK\0";

/// The shim's flag to serve one connection over its standard streams.
pub const SERVE: &str = "--serve";
/// The shim's flag to check its own hash before it is trusted.
pub const SELF_CHECK: &str = "--self-check";

/// How long a client waits for a worker to acknowledge the end of a connection: long
/// enough for it to stop its calls and kill their processes, and longer by what any
/// leg it relayed adds, [`Leg::ending`](crate::origin::Leg::ending).
pub const END_GRACE: std::time::Duration = std::time::Duration::from_millis(250);

/// One call on one connection; calls are numbered in order from 1.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct CallId(NonZeroU64);

impl std::fmt::Display for CallId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Allocates a connection's call IDs in order from 1.
pub struct CallIds(Mutex<NonZeroU64>);

impl Default for CallIds {
    fn default() -> Self {
        Self(Mutex::new(NonZeroU64::MIN))
    }
}

impl CallIds {
    pub fn next(&self) -> CallId {
        let mut next = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let id = *next;
        *next = id.saturating_add(1);
        CallId(id)
    }
}

/// What a client sends.
#[derive(Debug, Deserialize, Serialize)]
pub enum ClientFrame {
    /// The first frame of every connection.
    Hello,
    /// Perform `op`. A contained call first resolves its paths, then performs `op` on
    /// their resolutions: the client approved it in advance for every one resolving
    /// inside the authorization root, and for nothing else. Should any not, the call
    /// is held until the client resumes or cancels it. `op` names its paths as
    /// requested, in [`Operation::paths`]'s order.
    Call {
        id: CallId,
        op: Box<Op>,
        contained: Option<op::Resolve>,
    },
    /// Perform the held contained call on its paths' resolutions.
    Resume {
        id: CallId,
    },
    /// Bytes the call reads as its input; at most one flow-control window of
    /// them is unacknowledged by `InputAck`.
    Input {
        id: CallId,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    InputEnd {
        id: CallId,
    },
    /// One frame of the call's events was taken.
    EventAck {
        id: CallId,
    },
    /// Stop the call, sending nothing more of it.
    Cancel {
        id: CallId,
    },
    /// The client is done with the connection.
    End,
}

/// What a worker sends.
#[derive(Debug, Deserialize, Serialize)]
pub enum WorkerFrame {
    /// The handshake's answer: what the worker's machine is.
    Ready(crate::target::Facts),
    /// The worker cannot serve this connection: a setting of it names nothing the
    /// worker can use.
    Refused(crate::target::SettingsError),
    /// A call's next events, as many as were waiting, up to a chunk's bytes unless
    /// one event alone is more; at most one window of these frames is unacknowledged.
    Events { id: CallId, events: EventFrame },
    /// One chunk of a call's input was taken.
    InputAck { id: CallId },
    /// A contained call's paths all resolved inside the root: its operation performs
    /// on these resolutions, its frames following as a call's.
    Performing {
        id: CallId,
        resolved: Vec<op::Resolved>,
    },
    /// A contained call's paths did not all resolve inside the root: it performs
    /// nothing until it is resumed.
    Held {
        id: CallId,
        resolved: Vec<op::Resolved>,
    },
    /// How a call ended; its last frame.
    Done { id: CallId, outcome: Outcome },
    /// The worker ended the connection the client ended.
    Ended,
}

/// What one end sends: frames of a call's stream are sent in parts when too large.
pub trait Frame: Serialize + serde::de::DeserializeOwned {
    /// The call whose stream carries the frame; none for the connection's own, which
    /// make a stream of their own.
    fn call(&self) -> Option<CallId>;
}

impl Frame for ClientFrame {
    fn call(&self) -> Option<CallId> {
        match self {
            Self::Call { id, .. }
            | Self::Resume { id }
            | Self::Input { id, .. }
            | Self::InputEnd { id }
            | Self::EventAck { id }
            | Self::Cancel { id } => Some(*id),
            Self::Hello | Self::End => None,
        }
    }
}

impl Frame for WorkerFrame {
    fn call(&self) -> Option<CallId> {
        match self {
            Self::Events { id, .. }
            | Self::InputAck { id }
            | Self::Performing { id, .. }
            | Self::Held { id, .. }
            | Self::Done { id, .. } => Some(*id),
            Self::Ready(_) | Self::Refused(_) | Self::Ended => None,
        }
    }
}

/// How a peer broke the frame contract, as the end that received the frames saw it;
/// the connection is unusable.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Deserialize, Serialize)]
pub enum Violation {
    /// The worker's view: what the client sent, rendered, instead of its hello.
    #[error("expected hello, received {received}")]
    ExpectedHello { received: String },
    #[error("received a second hello")]
    SecondHello,
    #[error("duplicate call {0}")]
    DuplicateCall(CallId),
    /// A contained call whose operation names other paths than those it resolves.
    #[error("contained call {0} names other paths")]
    ContainedPaths(CallId),
    #[error("oversized input chunk")]
    OversizedInput,
    #[error("input flow-control overflow")]
    InputOverflow,
    /// An event acknowledged that was never sent.
    #[error("invalid stream credit")]
    EventCredit,
    #[error("invalid shim handshake")]
    InvalidHandshake,
    #[error("received a second handshake")]
    SecondHandshake,
    #[error("event flow-control overflow")]
    EventOverflow,
    /// Input acknowledged that was never sent.
    #[error("invalid input credit")]
    InputCredit,
    #[error("event of another operation")]
    ForeignEvent,
    #[error("answer of another operation")]
    ForeignAnswer,
    #[error("shim closed before replying")]
    ClosedBeforeReply,
    #[error("remote response dispatcher stopped unexpectedly")]
    DispatcherStopped,
}

/// An operation's wire form, and its events and end.
pub trait Wire: Operation {
    fn into_op(self) -> Op;
    fn events(frame: EventFrame) -> Option<Vec<Self::Event>>;
    fn answer(outcome: Outcome) -> Option<Result<Self::End, Self::Error>>;
    fn event_frame(events: Vec<Self::Event>) -> EventFrame;
    fn outcome(answer: Result<Self::End, Self::Error>) -> Outcome;
}

/// Declare the operations a connection carries, each once: its request, event and
/// end variants and their typed conversions, so no call is paired with another
/// operation's answer.
macro_rules! operations {
    ($($name:ident),+ $(,)?) => {
        /// An operation a client asks a worker to perform.
        #[derive(Debug, Deserialize, Serialize)]
        pub enum Op { $($name(op::$name)),+ }

        #[derive(Debug, Deserialize, Serialize)]
        pub enum EventFrame { $($name(Vec<<op::$name as Operation>::Event>)),+ }

        #[derive(Debug, Deserialize, Serialize)]
        pub enum Outcome {
            $($name(Result<<op::$name as Operation>::End, <op::$name as Operation>::Error>)),+
        }

        $(impl Wire for op::$name {
            fn into_op(self) -> Op {
                Op::$name(self)
            }

            fn events(frame: EventFrame) -> Option<Vec<Self::Event>> {
                match frame {
                    EventFrame::$name(events) => Some(events),
                    _ => None,
                }
            }

            fn answer(outcome: Outcome) -> Option<Result<Self::End, Self::Error>> {
                match outcome {
                    Outcome::$name(answer) => Some(answer),
                    _ => None,
                }
            }

            fn event_frame(events: Vec<Self::Event>) -> EventFrame {
                EventFrame::$name(events)
            }

            fn outcome(answer: Result<Self::End, Self::Error>) -> Outcome {
                Outcome::$name(answer)
            }
        })+

        impl Op {
            /// Perform this operation on `local`, sending its frames through `call`.
            pub(crate) async fn serve(self, local: &crate::local::Local, call: serve::Call) {
                match self {
                    $(Op::$name(operation) => call.run(local, operation).await,)+
                }
            }

            /// The paths the operation acts on, as [`Operation::paths`] names them.
            pub(crate) fn paths(&mut self) -> Vec<&mut op::TargetPath> {
                match self {
                    $(Op::$name(operation) => operation.paths(),)+
                }
            }
        }
    };
}

operations![
    Resolve, Read, Write, Replace, Remove, CreateDirs, Search, Glob, Exec, Fetch, Open, Workspace,
];

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Call `id`, which must not be zero.
    pub(crate) const fn call_id(id: u64) -> CallId {
        CallId(NonZeroU64::new(id).unwrap())
    }
}
