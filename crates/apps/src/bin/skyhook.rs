//! The plain coding-agent harness in the terminal.
use skyhook::product::{Product, Skyhook};
use skyhook_apps::args::{self, Invocation, NoArgs, NoCommand};
use skyhook_apps::{auth, batch, dump, exit_on, stats, tui};
use std::io;

#[cfg(not(windows))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const ABOUT: &str = "Programmable coding-agent harness";
const STATE: &str = "state.json";

fn main() {
    let identity = Skyhook.identity();
    let invocation = args::parse::<NoArgs, NoCommand, _>(&Skyhook, ABOUT);
    // SAFETY: no threads or runtime have been started.
    let runtime = unsafe { skyhook_apps::start(&identity) };
    let name = identity.name();
    runtime.block_on(async {
        match invocation {
            Invocation::Product(command) => match command {},
            Invocation::Auth(command) => {
                exit_on(&format!("{name} auth"), auth::run(command, &identity).await)
            }
            Invocation::Dump(request) => exit_on(
                &format!("{name} dump"),
                dump::run(&Skyhook, request, &mut io::stdout()).await,
            ),
            Invocation::Stats(request) => exit_on(
                &format!("{name} stats"),
                stats::run(request, &identity, &mut io::stdout().lock()).await,
            ),
            Invocation::Batch(job) => exit_on(
                name,
                batch::run(&Skyhook, job, STATE, &mut io::stdout(), &mut io::stderr()).await,
            ),
            Invocation::Interactive(session) => exit_on(
                name,
                tui::run(&Skyhook, session, STATE, tui::Look::SKYHOOK).await,
            ),
        }
    });
}
