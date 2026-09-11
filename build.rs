use std::{env, path::Path};

// Keep this package-relative staging path in sync with the RustEmbed folder.
const SHIM_DIRECTORY: &str = "target/shims";

fn main() {
    // cargo-zigbuild's generated wrappers re-enter this executable, not its CLI.
    // Dispatch before emitting any Cargo directives (or checking Cargo features).
    #[cfg(feature = "embed-shims")]
    if build_support::dispatch() {
        return;
    }

    println!("cargo:rustc-check-cfg=cfg(skyhook_embed_shims)");

    for name in [
        "RA_RUSTC_WRAPPER",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_RESOLVER_LOCKFILE_PATH",
        "PATH",
        "CARGO_ZIGBUILD_ZIG_PATH",
        "CARGO_ZIGBUILD_PYTHON_PATH",
        "CARGO_ZIGBUILD_CACHE_DIR",
        "ZIG_GLOBAL_CACHE_DIR",
        "ZIG_LOCAL_CACHE_DIR",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    for path in [
        "build.rs",
        "build_support",
        "Cargo.toml",
        "Cargo.lock",
        SHIM_DIRECTORY,
        "src/bin",
        "crates/skyhook-core/Cargo.toml",
        "crates/skyhook-core/src",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }

    for config in [".cargo/config", ".cargo/config.toml"] {
        if Path::new(config).exists() {
            println!("cargo:rerun-if-changed={config}");
        }
    }

    let rust_analyzer = running_under_rust_analyzer();
    if rust_analyzer {
        eprintln!("skipping remote shim builds during rust-analyzer analysis");
    }
    #[cfg(feature = "embed-shims")]
    if !rust_analyzer {
        build_support::build();
        // Only advertise embedding after all artifacts were built and validated.
        println!("cargo:rustc-cfg=skyhook_embed_shims");
    }
}

fn running_under_rust_analyzer() -> bool {
    if env::var_os("RA_RUSTC_WRAPPER").is_some() {
        return true;
    }
    for variable in ["RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"] {
        if env::var_os(variable)
            .as_deref()
            .and_then(|wrapper| Path::new(wrapper).file_name())
            .is_some_and(|name| name.to_string_lossy().starts_with("rust-analyzer"))
        {
            return true;
        }
    }
    env::var_os("CARGO_RESOLVER_LOCKFILE_PATH").is_some_and(|lockfile| {
        Path::new(&lockfile).components().any(|component| {
            component
                .as_os_str()
                .to_string_lossy()
                .starts_with("rust-analyzer")
        })
    })
}

#[cfg(any(feature = "embed-shims", test))]
mod build_support;
