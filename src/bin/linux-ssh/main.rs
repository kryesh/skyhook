#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() {
    if std::env::args().nth(1).as_deref() == Some("--askpass") {
        let result = std::env::var_os("SKYHOOK_ASKPASS_SOCKET")
            .ok_or("askpass socket missing".into())
            .and_then(|socket| {
                skyhook::remote::run_askpass_helper(
                    std::path::Path::new(&socket),
                    std::env::args().nth(2).unwrap_or_default(),
                )
            });
        if result.is_err() {
            std::process::exit(1);
        }
        return;
    }
    let result = match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [mode] if mode == "--serve" => skyhook::remote::worker::serve().await,
        [mode, root] if mode == "--serve" => {
            skyhook::remote::worker::serve_with_authorization_root(std::path::Path::new(root)).await
        }
        [mode, expected] if mode == "--self-check" => {
            skyhook::remote::worker::self_check(expected).await
        }
        _ => Err("usage: linux-ssh --serve [AUTHORIZATION_ROOT] | --self-check SHA256".into()),
    };
    if let Err(error) = result {
        eprintln!("linux-ssh: {error}");
        std::process::exit(1);
    }
}
