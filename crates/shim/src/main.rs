#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() {
    skyhook::remote::askpass_main();
    let args: Vec<_> = std::env::args().skip(1).collect();
    if let Err(error) = skyhook::remote::worker::main(&args).await {
        eprintln!("shim: {error}");
        std::process::exit(1);
    }
}
