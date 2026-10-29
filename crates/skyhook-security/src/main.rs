mod capabilities;
mod config;
pub mod cvss;
mod presets;
mod product;
mod reporting;
mod research_view;
mod session;
pub mod store;
mod tools;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    skyhook::remote::askpass_main();
    // SAFETY: askpass returns without starting workers; startup is still single-threaded.
    unsafe { skyhook_cli::main(product::Security) }
}
