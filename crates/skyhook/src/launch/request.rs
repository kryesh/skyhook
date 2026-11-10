//! What a front end asks a launch for, once its own syntax admitted it. Paths and inputs
//! remain unread until their session starts.
use crate::config::mode::ModeName;
use crate::identity::SessionId;
use crate::provider::model::ModelRef;
use crate::tool::policy::Capability;
use std::path::PathBuf;

pub struct ExecutionRequest {
    pub resume: Option<SessionId>,
    /// The root model to select and remember; a resumed session switches its root to it
    /// before anything runs.
    pub model: Option<ModelRef>,
}

/// A configured mode (the default when unnamed), or a batch job's exact capabilities.
pub enum PermissionRequest {
    Mode(Option<ModeName>),
    Exact(Vec<Capability>),
}

/// Unread input for a session's first operation. Paths are read only after the session
/// has opened (and a batch session's ID has been announced).
pub enum InitialInput {
    Prompt { text: String, images: Vec<PathBuf> },
    Script(PathBuf),
}
