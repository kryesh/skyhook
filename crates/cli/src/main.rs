#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    skyhook::remote::askpass_main();
    // SAFETY: askpass returns without starting workers; startup is still single-threaded.
    unsafe { cli::main(cli::Skyhook) }
}
