//! Execution targets as the worker knows them: the trait every operation runs through,
//! the facts a target reports about itself, the settings one connection runs under, and
//! the file a shim installs as.

mod facts;
mod settings;
mod shim;

use std::future::Future;

pub use facts::{Arch, Facts, Os, Platform, TimeZone};
pub use settings::{Settings, SettingsError};
pub use shim::{ShimName, UPLOAD_PREFIX};

use crate::{
    op::{Answer, Never, Running},
    protocol::Wire,
};

/// A machine tools run on: the session's own ([`Local`](crate::local::Local)) or
/// one reached over the protocol. Every operation runs there whole.
pub trait Target: Send + Sync {
    /// Why an operation could not reach the target, or its answer come back.
    type Unreachable: std::error::Error + Send + Sync + 'static;

    fn run<O: Wire>(
        &self,
        operation: O,
    ) -> impl Future<Output = Result<Running<O, Self::Unreachable>, Self::Unreachable>> + Send;

    /// An operation that streams nothing, to its end.
    fn call<O: Wire<Event = Never>>(
        &self,
        operation: O,
    ) -> impl Future<Output = Answer<O::End, O::Error, Self::Unreachable>> + Send {
        async move { self.run(operation).await?.end().await }
    }
}
