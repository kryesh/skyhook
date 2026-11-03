//! Shared terminal and batch host for product binaries.
pub mod page;
mod product;
pub use product::{
    Configured, Error, ExtensionView, HookFuture, Identity, InitialInput, NoArgs, NoCommand,
    Opening, PreparedSession, Product, RootMode, SessionContext, SessionExtension, SessionFactory,
    Skyhook,
};

mod args;
mod auth;
mod config;
pub mod form;
pub use config::HostConfig;
mod dotenv;
mod dump;
mod embedded_shims;
mod headless;
mod interaction;
mod launch;
mod state;
mod stats;
mod text;
pub use text::Table;
mod tui;
use args::Invocation;
pub use tui::{Color, Palette, Rgb, SyntaxPalette, Theme};

/// Exit when `result` failed, reporting the error under `label` with its
/// terminal controls escaped.
fn exit_on<T>(label: &str, result: Result<T, impl std::fmt::Display>) -> T {
    result.unwrap_or_else(|error| {
        let error = skyhook::tool::diagnostic::escape_controls(error);
        eprintln!("{label}: {error}");
        std::process::exit(1)
    })
}

/// Run a product's CLI, loading `.env` only after help/version and syntax admission.
/// Call `remote::askpass_main()` before this entry point.
///
/// # Safety
/// The process must still be single-threaded: `.env` loading mutates its environment.
/// No runtime, terminal worker, provider, or tracing subscriber may have been started.
/// Product syntax hooks and Clap value parsers must preserve this until configuration begins.
pub unsafe fn main<P: Product>(product: P) {
    let identity = product.identity();
    let invocation =
        args::parse_product(&product, std::env::args_os()).unwrap_or_else(|error| error.exit());
    // SAFETY: the caller guarantees single-threaded startup; syntax hooks must preserve it.
    exit_on(identity.name(), unsafe { dotenv::load_invocation_env() });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|_| "could not start async runtime");
    exit_on(identity.name(), runtime).block_on(run(product, invocation));
}

async fn run<P: Product>(product: P, invocation: Invocation<P>) {
    let identity = product.identity();
    let name = identity.name();
    match invocation {
        Invocation::Product(command) => exit_on(name, product.command(command).await),
        Invocation::Inspect(request) => exit_on(
            &format!("{name} dump"),
            dump::run(&product, request, &mut std::io::stdout()).await,
        ),
        Invocation::Stats(request) => exit_on(
            &format!("{name} stats"),
            stats::run(request, &identity, &mut std::io::stdout().lock()).await,
        ),
        Invocation::Auth(command) => {
            exit_on(&format!("{name} auth"), auth::run(command, &identity).await)
        }
        // Batch execution never installs a terminal, renderer, or tracing subscriber.
        Invocation::Headless(request, input, args) => {
            let host = exit_on(
                name,
                launch::configure(&product, args, &request.execution, true).await,
            );
            exit_on(
                name,
                headless::run(request, input, host, &mut std::io::stdout()).await,
            )
        }
        Invocation::Interactive(request, input, args) => {
            exit_on(name, tui::check_terminal(&identity));
            let host = exit_on(
                name,
                launch::configure(&product, args, &request.execution, false).await,
            );
            exit_on(name, tui::run(request, input, host).await)
        }
    }
}

#[cfg(test)]
mod tests {
    use skyhook::config::{Config, ConfigDiagnostic};

    /// A product whose configuration edit selects a mode of its own, `inspect`, and reports
    /// a warning carrying a control sequence.
    pub(crate) struct Configuring;
    impl crate::Product for Configuring {
        type Args = crate::NoArgs;
        type Command = crate::NoCommand;
        fn identity(&self) -> crate::Identity {
            crate::Skyhook.identity()
        }
        async fn configure(
            &self,
            config: &mut Config,
            _: crate::NoArgs,
            context: crate::SessionContext<'_>,
        ) -> Result<crate::Configured, crate::Error> {
            assert!(context.workspace.is_absolute());
            config.default_mode = "inspect".parse().unwrap();
            config
                .modes
                .insert(config.default_mode.clone(), config.modes["general"].clone());
            let mut configured = crate::Configured::default();
            configured.report.diagnostics.push(ConfigDiagnostic {
                path: context.workspace.join("product.yaml"),
                message: "fixture warning\u{1b}[31m".into(),
            });
            Ok(configured)
        }
        async fn command(&self, command: crate::NoCommand) -> Result<(), crate::Error> {
            match command {}
        }
    }

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
}
