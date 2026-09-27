//! Built-in coding, helper, and job-control tools.

mod fetch;
pub(crate) mod filesystem;
pub(crate) mod jobs;
mod process;
mod script;
pub(crate) mod search;
mod skills;
mod targets;
pub(crate) mod workspace;

use crate::{
    job::JobManager,
    session::SessionStore,
    tool::{RegistryError, ToolRegistryBuilder},
};

/// Builtin tool names, shared by registration and host presentation.
pub mod names {
    pub const AGENT: &str = "agent";
    pub const EXEC: &str = "exec";
    pub const READ: &str = "read";
    pub const REPLACE: &str = "replace";
    pub const SCRIPT: &str = "script";
    pub const WRITE: &str = "write";
}
pub(crate) use script::install_script_tool;
pub use skills::HostSkills;

/// Register the standard workspace, process, and job-control tool set.
pub(crate) fn register_coding_tools(
    builder: &mut ToolRegistryBuilder,
    store: SessionStore,
    jobs: JobManager,
    skills: HostSkills,
    router: crate::target::TargetRouter,
) -> Result<(), RegistryError> {
    builder.register_local(register_local_tools)?;
    jobs::register(builder, jobs)?;
    skills::register(builder, skills, store.clone())?;
    targets::register(builder, store, router)?;
    Ok(())
}

/// An attached image as a tool result describes it; the image itself is attached.
#[derive(serde::Serialize, schemars::JsonSchema)]
struct ImageSummary {
    format: crate::media::ImageFormat,
    bytes: u64,
}

impl From<&crate::media::ImageRef> for ImageSummary {
    fn from(image: &crate::media::ImageRef) -> Self {
        Self {
            format: image.format,
            bytes: image.blob.bytes,
        }
    }
}

fn default_dot() -> String {
    crate::tool::registry::DEFAULT_PATH.to_owned()
}

mod bounded {
    /// An integer argument from `MIN` through `MAX`, a range its schema carries.
    #[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize)]
    #[serde(try_from = "u64", into = "u64")]
    pub(super) struct Bounded<const MIN: u64, const MAX: u64>(u64);

    impl<const MIN: u64, const MAX: u64> Bounded<MIN, MAX> {
        /// A constant within the range, checked at compile time.
        pub(super) const fn new<const VALUE: u64>() -> Self {
            const { assert!(MIN <= VALUE && VALUE <= MAX) };
            Self(VALUE)
        }
    }

    impl<const MIN: u64, const MAX: u64> TryFrom<u64> for Bounded<MIN, MAX> {
        type Error = String;

        fn try_from(value: u64) -> Result<Self, String> {
            (MIN..=MAX)
                .contains(&value)
                .then_some(Self(value))
                .ok_or_else(|| format!("must be {MIN} through {MAX}"))
        }
    }

    impl<const MIN: u64, const MAX: u64> From<Bounded<MIN, MAX>> for u64 {
        fn from(value: Bounded<MIN, MAX>) -> Self {
            value.0
        }
    }

    impl<const MIN: u64, const MAX: u64> schemars::JsonSchema for Bounded<MIN, MAX> {
        fn inline_schema() -> bool {
            true
        }
        fn schema_name() -> std::borrow::Cow<'static, str> {
            "Bounded".into()
        }
        fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
            schemars::json_schema!({
                "type": "integer",
                "format": "uint64",
                "minimum": MIN,
                "maximum": MAX,
            })
        }
    }
}
use bounded::Bounded;

/// Seconds a call may run.
type TimeoutSecs = Bounded<1, 3600>;

impl TimeoutSecs {
    fn duration(self) -> std::time::Duration {
        std::time::Duration::from_secs(self.into())
    }
}

pub(crate) fn register_local_tools(
    builder: &mut crate::tool::invocation::LocalCatalogBuilder,
) -> Result<(), RegistryError> {
    filesystem::register(builder)?;
    search::register(builder)?;
    process::register(builder, tokio::time::sleep)?;
    fetch::register(builder)?;
    Ok(())
}
