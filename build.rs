use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
};

struct ShimTarget {
    triple: &'static str,
    artifact: &'static str,
    machine: u16,
}

const SHIM_TARGETS: &[ShimTarget] = &[
    ShimTarget {
        triple: "x86_64-unknown-linux-musl",
        artifact: "skyhook-shim-x86_64-linux",
        machine: 62,
    },
    ShimTarget {
        triple: "aarch64-unknown-linux-musl",
        artifact: "skyhook-shim-aarch64-linux",
        machine: 183,
    },
];

fn main() {
    println!("cargo:rerun-if-env-changed=SKYHOOK_CROSS");
    println!("cargo:rerun-if-env-changed=RA_RUSTC_WRAPPER");
    println!("cargo:rerun-if-env-changed=RUSTC_WRAPPER");
    println!("cargo:rerun-if-env-changed=CARGO_RESOLVER_LOCKFILE_PATH");
    println!("cargo:rerun-if-env-changed=CROSS_CONTAINER_ENGINE");
    println!("cargo:rerun-if-env-changed=CROSS_CONFIG");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=src/bin/skyhook-shim");
    println!("cargo:rerun-if-changed=crates/skyhook-core/Cargo.toml");
    println!("cargo:rerun-if-changed=crates/skyhook-core/src");

    let out_dir = required_path("OUT_DIR");
    let generated = out_dir.join("embedded_shims.rs");
    let rust_analyzer = running_under_rust_analyzer();
    if env::var_os("CARGO_FEATURE_EMBED_SHIMS").is_none() || rust_analyzer {
        if rust_analyzer {
            println!("cargo:warning=skipping remote shim builds during rust-analyzer analysis");
        }
        write_catalog(&generated, &[]);
        return;
    }

    let manifest_dir = required_path("CARGO_MANIFEST_DIR");
    let nested_target = out_dir.join("cross-target");
    let shim_dir = out_dir.join("shims");
    fs::create_dir_all(&shim_dir).expect("could not create the embedded shim output directory");
    let cross = env::var_os("SKYHOOK_CROSS").unwrap_or_else(|| "cross".into());
    let mut artifacts = Vec::with_capacity(SHIM_TARGETS.len());

    for target in SHIM_TARGETS {
        println!("cargo:warning=building {} with cross", target.triple);
        let mut command = Command::new(&cross);
        command
            .current_dir(&manifest_dir)
            .arg("build")
            .arg("--manifest-path")
            .arg(manifest_dir.join("Cargo.toml"))
            .arg("--package")
            .arg("skyhook-agent")
            .arg("--locked")
            .arg("--profile")
            .arg("shim-release")
            .arg("--bin")
            .arg("skyhook-shim")
            .arg("--target")
            .arg(target.triple)
            .arg("--target-dir")
            .arg(&nested_target)
            .arg("--no-default-features")
            .arg("--features")
            .arg("shim-bin");
        for (name, _) in env::vars_os() {
            if name.to_string_lossy().starts_with("CARGO_FEATURE_") {
                command.env_remove(name);
            }
        }
        command
            .env_remove("CARGO_RESOLVER_LOCKFILE_PATH")
            .env_remove("CARGO_MAKEFLAGS")
            .env_remove("MAKEFLAGS");
        let status = command.status().unwrap_or_else(|error| {
            panic!(
                "could not start `cross` while building embedded shims: {error}; \
                     install it with `cargo install cross --git https://github.com/cross-rs/cross` \
                     and ensure Docker or Podman is running"
            )
        });
        assert!(
            status.success(),
            "cross failed while building {} with status {status}",
            target.triple
        );

        let source = nested_target
            .join(target.triple)
            .join("shim-release")
            .join("skyhook-shim");
        let bytes = fs::read(&source).unwrap_or_else(|error| {
            panic!("could not read cross output {}: {error}", source.display())
        });
        validate_static_elf(&bytes, target.machine)
            .unwrap_or_else(|error| panic!("invalid cross output {}: {error}", source.display()));
        let destination = shim_dir.join(target.artifact);
        write_if_changed(&destination, &bytes).unwrap_or_else(|error| {
            panic!(
                "could not write embedded shim {}: {error}",
                destination.display()
            )
        });
        artifacts.push((target.artifact, destination));
    }

    write_catalog(&generated, &artifacts);
}

fn running_under_rust_analyzer() -> bool {
    if env::var_os("RA_RUSTC_WRAPPER").is_some() {
        return true;
    }
    if env::var_os("RUSTC_WRAPPER")
        .as_deref()
        .and_then(|wrapper| Path::new(wrapper).file_name())
        .is_some_and(|name| name.to_string_lossy().starts_with("rust-analyzer"))
    {
        return true;
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

fn required_path(name: &str) -> PathBuf {
    env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("Cargo did not set {name}"))
}

fn write_catalog(path: &Path, artifacts: &[(&str, PathBuf)]) {
    let mut source = String::from("pub(crate) static EMBEDDED_SHIMS: &[(&str, &[u8])] = &[\n");
    for (name, artifact) in artifacts {
        let artifact = artifact.to_string_lossy();
        source.push_str(&format!(
            "    ({name:?}, include_bytes!({artifact:?}) as &[u8]),\n"
        ));
    }
    source.push_str("];\n");
    write_if_changed(path, source.as_bytes())
        .unwrap_or_else(|error| panic!("could not generate {}: {error}", path.display()));
}

fn write_if_changed(path: &Path, contents: &[u8]) -> io::Result<()> {
    match fs::read(path) {
        Ok(existing) if existing == contents => Ok(()),
        Ok(_) => fs::write(path, contents),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::write(path, contents),
        Err(error) => Err(error),
    }
}

fn validate_static_elf(bytes: &[u8], expected_machine: u16) -> Result<(), String> {
    if bytes.len() < 64 || bytes.get(..4) != Some(b"\x7fELF") {
        return Err("not an ELF executable".to_owned());
    }
    if bytes[4] != 2 || bytes[5] != 1 {
        return Err("expected a 64-bit little-endian ELF executable".to_owned());
    }
    let machine = read_u16(bytes, 18)?;
    if machine != expected_machine {
        return Err(format!(
            "ELF machine {machine} does not match expected machine {expected_machine}"
        ));
    }

    let program_offset = usize::try_from(read_u64(bytes, 32)?)
        .map_err(|_| "program header offset does not fit usize".to_owned())?;
    let entry_size = usize::from(read_u16(bytes, 54)?);
    let entry_count = usize::from(read_u16(bytes, 56)?);
    if entry_count == 0 || entry_size < 4 {
        return Err("ELF has no usable program headers".to_owned());
    }
    for index in 0..entry_count {
        let offset = index
            .checked_mul(entry_size)
            .and_then(|value| program_offset.checked_add(value))
            .ok_or_else(|| "program header offset overflowed".to_owned())?;
        if read_u32(bytes, offset)? == 3 {
            return Err("ELF requests a program interpreter and is dynamically linked".to_owned());
        }
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| "truncated ELF header".to_owned())?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| "truncated ELF program header".to_owned())?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| "truncated ELF header".to_owned())?;
    Ok(u64::from_le_bytes([
        value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
    ]))
}
