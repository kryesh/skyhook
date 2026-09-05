#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() {
    let result = match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [mode] if mode == "--serve" => skyhook::remote::worker::serve().await,
        [mode, root] if mode == "--serve" => {
            skyhook::remote::worker::serve_with_authorization_root(std::path::Path::new(root)).await
        }
        [mode, expected] if mode == "--self-check" => {
            skyhook::remote::worker::self_check(expected).await
        }
        _ => Err("usage: skyhook-shim --serve [AUTHORIZATION_ROOT] | --self-check SHA256".into()),
    };
    if let Err(error) = result {
        eprintln!("skyhook-shim: {error}");
        std::process::exit(1);
    }
}
