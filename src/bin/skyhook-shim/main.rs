#[tokio::main]
async fn main() {
    let result = match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [mode] if mode == "--serve" => skyhook::remote::worker::serve().await,
        [mode, expected] if mode == "--self-check" => {
            skyhook::remote::worker::self_check(expected).await
        }
        _ => Err("usage: skyhook-shim --serve | --self-check SHA256".into()),
    };
    if let Err(error) = result {
        eprintln!("skyhook-shim: {error}");
        std::process::exit(1);
    }
}
