//! Names the directory shims are staged in to the code that embeds them, and rebuilds
//! the applications when the set of staged shims changes. A release build embeds the
//! files rust-embed finds there as it expands, and cargo follows each file it embedded
//! but not the directory it listed.

/// Where shims are staged, from this package's directory.
const SHIMS: &str = "../../target/shims";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-env=SKYHOOK_SHIMS={SHIMS}");
    // A debug build reads the directory as it runs.
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        println!("cargo:rerun-if-changed={SHIMS}");
    }
}
