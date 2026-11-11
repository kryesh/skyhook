//! The terminal applications' library: the shared command line, batch jobs, the auth, dump
//! and stats commands and the terminal, which the `skyhook` and `skyhook-security` binaries
//! dispatch.
pub mod args;
pub mod auth;
pub mod batch;
pub mod config;
mod dotenv;
pub mod dump;
mod shims;
mod signals;
mod state;
pub mod stats;
pub mod text;
pub mod tui;

use skyhook::product::Identity;

/// Why a submitted prompt or script failed.
#[derive(Debug, thiserror::Error)]
pub enum OperationError {
    #[error(transparent)]
    Product(#[from] skyhook::product::Error),
    #[error(transparent)]
    Harness(#[from] skyhook::agent::HarnessError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Attachment(#[from] skyhook::attachment::AttachmentError),
}

impl OperationError {
    /// Whether the session's journal failed, which stopped the session.
    pub fn journal_failed(&self) -> bool {
        matches!(self, Self::Harness(error) if error.journal_failed())
    }
}

/// Exit when `result` failed, reporting the error under `label` with its
/// terminal controls escaped.
pub fn exit_on<T>(label: &str, result: Result<T, impl std::fmt::Display>) -> T {
    result.unwrap_or_else(|error| {
        eprintln!("{label}: {}", escape_lines(error));
        std::process::exit(1)
    })
}

/// `text` with every terminal control escaped but its line breaks, which multi-line
/// reports such as a configuration's diagnostics use as structure.
fn escape_lines(text: impl std::fmt::Display) -> String {
    let text = text.to_string();
    let lines = text
        .split('\n')
        .map(skyhook_tools::diagnostic::escape_controls);
    lines.collect::<Vec<_>>().join("\n")
}

/// Load the invocation's `.env`, then start the async runtime; either failing exits under
/// `identity`'s name.
///
/// # Safety
/// The process must still be single-threaded: `.env` loading mutates its environment.
/// Call it once the command line is parsed, before any runtime, terminal worker, provider,
/// or tracing subscriber has started.
pub unsafe fn start(identity: &Identity) -> tokio::runtime::Runtime {
    // SAFETY: the caller guarantees single-threaded startup.
    exit_on(identity.name(), unsafe { dotenv::load_invocation_env() });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|_| "could not start async runtime");
    exit_on(identity.name(), runtime)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    /// Await `future`, failing the test at the caller if it stalls.
    #[track_caller]
    pub(crate) fn bounded<T>(future: impl Future<Output = T>) -> impl Future<Output = T> {
        let caller = std::panic::Location::caller();
        async move {
            tokio::time::timeout(std::time::Duration::from_secs(20), future)
                .await
                .unwrap_or_else(|_| panic!("test synchronization timed out at {caller}"))
        }
    }

    #[test]
    fn exit_reports_keep_line_breaks_but_escape_other_controls() {
        assert_eq!(
            super::escape_lines("headline\n  file: \u{1b}[2Jtext\r"),
            "headline\n  file: \\u{1b}[2Jtext\\r"
        );
    }

    /// A configuration with one model, `test/first`, on an unreachable server. Its models
    /// come last, so a test can list more after them.
    pub(crate) const CONFIG: &str = "providers:\n  test:\n    dialect: compatible\n    codec: chat_completions\n    base_url: http://127.0.0.1:1\n    models:\n      first:\n        model: fixture\n        max_context: 128000\n        max_output: 4096\n";

    /// [`CONFIG`] with models `test/<name>` for each of `names` listed after its own.
    pub(crate) fn models(names: &[&str]) -> String {
        let entry = |name| {
            format!(
                "      {name}:\n        model: fixture\n        max_context: 128000\n        max_output: 4096\n"
            )
        };
        CONFIG.to_owned() + &names.iter().map(entry).collect::<String>()
    }

    /// A product of `identity`, whose sessions `factory` prepares and whose configuration
    /// edit warns of `warnings`.
    pub(crate) struct Fixture {
        pub(crate) identity: skyhook::product::Identity,
        pub(crate) factory: Arc<dyn skyhook::product::SessionFactory>,
        pub(crate) warnings: Vec<String>,
    }
    impl Default for Fixture {
        fn default() -> Self {
            Self {
                identity: skyhook::product::Product::identity(&skyhook::product::Skyhook),
                factory: Arc::new(()),
                warnings: Vec::new(),
            }
        }
    }
    impl skyhook::product::Product for Fixture {
        type Options = ();
        fn identity(&self) -> skyhook::product::Identity {
            self.identity.clone()
        }
        async fn configure(
            &self,
            _: &mut skyhook::config::Config,
            (): (),
            _: skyhook::product::SessionContext<'_>,
        ) -> Result<skyhook::product::Configured, skyhook::product::Error> {
            Ok(skyhook::product::Configured {
                factory: self.factory.clone(),
                warnings: self.warnings.clone(),
            })
        }
    }
}
