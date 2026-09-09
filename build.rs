use std::{env, path::Path};

// Keep this package-relative staging path in sync with the RustEmbed folder.
const SHIM_DIRECTORY: &str = "target/shims";

fn main() {
    // cargo-zigbuild's generated wrappers re-enter this executable, not its CLI.
    // Dispatch before emitting any Cargo directives (or checking Cargo features).
    #[cfg(feature = "embed-shims")]
    if embedded::dispatch() {
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
        embedded::build();
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
mod embedded {
    #[cfg(feature = "embed-shims")]
    use super::SHIM_DIRECTORY;
    use std::{
        collections::HashSet,
        fs,
        io::{self, Write},
        path::Path,
        sync::atomic::{AtomicU64, Ordering},
    };

    #[cfg(feature = "embed-shims")]
    use std::{env, path::PathBuf, process::Command};

    struct ShimTarget {
        platform: &'static str,
        protocol: &'static str,
        arch: &'static str,
        target: &'static str,
    }

    const SHIM_TARGETS: &[ShimTarget] = &[
        ShimTarget {
            platform: "linux",
            protocol: "ssh",
            arch: "x86_64",
            target: "x86_64-unknown-linux-musl",
        },
        ShimTarget {
            platform: "linux",
            protocol: "ssh",
            arch: "aarch64",
            target: "aarch64-unknown-linux-musl",
        },
    ];

    #[cfg(feature = "embed-shims")]
    const WORKER_ARG: &str = "--skyhook-private-build-shims";

    impl ShimTarget {
        fn bin(&self) -> String {
            format!("{}-{}", self.platform, self.protocol)
        }

        fn artifact(&self) -> String {
            format!("{}-{}", self.bin(), self.arch)
        }

        fn machine(&self) -> Result<u16, String> {
            let parts: Vec<_> = self.target.split('-').collect();
            if parts.len() != 4
                || parts[0] != self.arch
                || parts[2] != self.platform
                || self.platform != "linux"
                || parts[3] != "musl"
            {
                return Err(format!(
                    "unsupported or inconsistent shim platform/architecture/target: {}/{}/{}",
                    self.platform, self.arch, self.target
                ));
            }
            match self.arch {
                "x86_64" => Ok(62),
                "aarch64" => Ok(183),
                arch => Err(format!("unsupported shim ELF architecture: {arch}")),
            }
        }
    }

    fn validate_targets(targets: &[ShimTarget]) -> Result<(), String> {
        let mut names = HashSet::new();
        for target in targets {
            for component in [target.platform, target.protocol, target.arch] {
                if component.is_empty()
                    || !component
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                {
                    return Err(format!("invalid shim name component: {component:?}"));
                }
            }
            target.machine()?;
            if !names.insert(target.artifact()) {
                return Err(format!("duplicate shim artifact: {}", target.artifact()));
            }
        }
        Ok(())
    }

    struct ShimProfile {
        cargo_name: &'static str,
        output_directory: &'static str,
    }

    impl ShimProfile {
        fn for_outer_profile(profile: &str) -> Self {
            // Cargo reports dev/test builds as `debug`, but the profile passed
            // to nested Cargo must be named `dev`; its output directory is debug.
            if profile == "debug" {
                Self {
                    cargo_name: "dev",
                    output_directory: "debug",
                }
            } else {
                Self {
                    cargo_name: "shim-release",
                    output_directory: "shim-release",
                }
            }
        }
    }

    #[cfg(feature = "embed-shims")]
    pub(super) fn dispatch() -> bool {
        use cargo_zigbuild::Zig;

        let mut args = env::args();
        let executable = PathBuf::from(args.next().expect("missing executable name"));
        let basename = executable.file_stem().unwrap_or_default().to_string_lossy();
        let first = args.next();
        let (tool, args) = if basename.eq_ignore_ascii_case("ar")
            || basename.eq_ignore_ascii_case("lib")
            || basename.ends_with("dlltool")
        {
            let tool = if basename.ends_with("dlltool") {
                "dlltool".to_owned()
            } else {
                basename.to_ascii_lowercase()
            };
            (tool, first.into_iter().chain(args).collect::<Vec<_>>())
        } else if first.as_deref() == Some("zig") {
            let tool = args.next().expect("missing Zig wrapper subcommand");
            let mut args: Vec<_> = args.collect();
            // Match clap's delimiter handling in cargo-zigbuild's CLI.
            if args.first().is_some_and(|arg| arg == "--") {
                args.remove(0);
            }
            (tool, args)
        } else if first.as_deref() == Some(WORKER_ARG) {
            let manifest_dir =
                PathBuf::from(args.next().expect("missing worker manifest directory"));
            let out_dir = PathBuf::from(args.next().expect("missing worker output directory"));
            let profile = args.next().expect("missing worker build profile");
            assert!(args.next().is_none(), "unexpected worker argument");
            worker(&manifest_dir, &out_dir, &profile);
            return true;
        } else {
            assert!(first.is_none(), "unexpected build script argument");
            return false;
        };
        let zig = match tool.as_str() {
            "cc" => Zig::Cc { args },
            "c++" => Zig::Cxx { args },
            "ar" => Zig::Ar { args },
            "ranlib" => Zig::Ranlib { args },
            "lib" => Zig::Lib { args },
            "dlltool" => Zig::Dlltool { args },
            _ => panic!("unsupported Zig wrapper subcommand: {tool}"),
        };
        zig.execute()
            .unwrap_or_else(|error| panic!("Zig {tool} failed: {error:#}"));
        true
    }

    #[cfg(feature = "embed-shims")]
    fn required_path(name: &str) -> PathBuf {
        env::var_os(name)
            .map(PathBuf::from)
            .unwrap_or_else(|| panic!("Cargo did not set {name}"))
    }

    fn inherited_build_variable(name: &str) -> bool {
        [
            "CARGO_FEATURE_",
            "CARGO_CFG_",
            "CARGO_PKG_",
            "CARGO_BIN_",
            "DEP_",
        ]
        .iter()
        .any(|prefix| name.starts_with(prefix))
            || matches!(
                name,
                "OUT_DIR"
                    | "HOST"
                    | "TARGET"
                    | "PROFILE"
                    | "OPT_LEVEL"
                    | "DEBUG"
                    | "NUM_JOBS"
                    | "CARGO_MANIFEST_DIR"
                    | "CARGO_MANIFEST_PATH"
                    | "CARGO_PRIMARY_PACKAGE"
                    | "CARGO_CRATE_NAME"
                    | "CARGO_RESOLVER_LOCKFILE_PATH"
                    | "CARGO_MAKEFLAGS"
                    | "MAKEFLAGS"
                    | "MFLAGS"
                    | "CARGO_ENCODED_RUSTFLAGS"
                    | "RUSTFLAGS"
                    | "CARGO_BUILD_RUSTFLAGS"
                    | "CLIPPY_ARGS"
                    | "CLIPPY_CONF_DIR"
                    | "CARGO_BUILD_TARGET"
                    | "CARGO_TARGET_DIR"
                    | "CARGO_BUILD_TARGET_DIR"
                    | "RUSTC_LINKER"
                    | "CARGO_ZIGBUILD_ZIG_COMMAND"
                    | "CARGO_ZIGBUILD_ZIG_COMMAND_ARGS"
                    | "CARGO_ZIGBUILD_ZIG_VERSION"
            )
    }

    fn inherited_analysis_wrapper(name: &str, value: &std::ffi::OsStr) -> bool {
        matches!(name, "RUSTC_WRAPPER" | "RUSTC_WORKSPACE_WRAPPER")
            && Path::new(value)
                .file_stem()
                .is_some_and(|name| name == "clippy-driver")
    }

    #[cfg(feature = "embed-shims")]
    pub(super) fn build() {
        validate_targets(SHIM_TARGETS).expect("invalid embedded shim target configuration");
        let manifest_dir = required_path("CARGO_MANIFEST_DIR");
        let out_dir = required_path("OUT_DIR");
        // Capture the outer profile before removing Cargo state from the worker.
        let profile = env::var("PROFILE").expect("Cargo did not set PROFILE");
        fs::create_dir_all(manifest_dir.join(SHIM_DIRECTORY))
            .expect("could not create the target/shims staging directory");

        // cargo-zigbuild reads process-global Cargo configuration and rustflags while
        // constructing Build's command. Sanitize a private subprocess *first*, not
        // just the returned Command, and never mutate this process's environment.
        let mut command = Command::new(env::current_exe().expect("could not locate build script"));
        command
            .arg(WORKER_ARG)
            .arg(&manifest_dir)
            .arg(&out_dir)
            .arg(&profile)
            .current_dir(&manifest_dir)
            .stdout(std::process::Stdio::from(io::stderr()));
        for (name, value) in env::vars_os() {
            let key = name.to_string_lossy();
            // A Clippy invocation still needs real, portable shim executables,
            // not another lint run. Keep ordinary caching/compiler wrappers.
            if inherited_build_variable(&key) || inherited_analysis_wrapper(&key, &value) {
                command.env_remove(name);
            }
        }
        // An explicitly empty value also overrides rustflags in Cargo config
        // files. Merely removing inherited variables would expose host settings
        // such as target-cpu=native to both portable shim targets while the
        // library constructs its compiler wrappers.
        command.env("CARGO_ENCODED_RUSTFLAGS", "");
        let status = command
            .status()
            .expect("could not start the private shim build worker");
        assert!(
            status.success(),
            "embedded shim worker failed with {status}; install Zig and the Rust targets x86_64-unknown-linux-musl and aarch64-unknown-linux-musl"
        );
    }

    #[cfg(feature = "embed-shims")]
    fn worker(manifest_dir: &Path, out_dir: &Path, outer_profile: &str) {
        let profile = ShimProfile::for_outer_profile(outer_profile);
        validate_targets(SHIM_TARGETS).expect("invalid embedded shim target configuration");
        let nested_target = out_dir.join("zig-target");
        for target in SHIM_TARGETS {
            let bin = target.bin();
            eprintln!(
                "building {} for {} with cargo-zigbuild (profile {})",
                bin, target.target, profile.cargo_name
            );
            let mut build = cargo_zigbuild::Build::new(Some(manifest_dir.join("Cargo.toml")));
            build.packages = vec!["skyhook-agent".into()];
            build.locked = true;
            build.profile = Some(profile.cargo_name.into());
            build.bin = vec![bin.clone()];
            build.target = vec![target.target.into()];
            build.target_dir = Some(nested_target.clone());
            build.no_default_features = true;
            build.features = vec!["shim-bin".into()];
            build.enable_zig_ar = true;
            // Keep nested Cargo output off the outer build script's directive stream.
            let status = build
                .build_command()
                .unwrap_or_else(|error| panic!("could not configure cargo-zigbuild: {error:#}"))
                .stdout(std::process::Stdio::inherit())
                .status()
                .expect("could not start nested Cargo");
            assert!(
                status.success(),
                "cargo-zigbuild failed for {} with {status}",
                target.target
            );

            let source = nested_target
                .join(target.target)
                .join(profile.output_directory)
                .join(bin);
            let bytes = fs::read(&source)
                .unwrap_or_else(|error| panic!("could not read {}: {error}", source.display()));
            validate_static_elf(&bytes, target.machine().unwrap())
                .unwrap_or_else(|error| panic!("invalid shim {}: {error}", source.display()));
            let destination = manifest_dir.join(SHIM_DIRECTORY).join(target.artifact());
            write_if_changed(&destination, &bytes).unwrap_or_else(|error| {
                panic!("could not publish {}: {error}", destination.display())
            });
        }
        // Only publish configured targets. Never sweep this directory: other files
        // (including artifacts from older versions) may be owned by the user.
    }

    fn write_if_changed(path: &Path, contents: &[u8]) -> io::Result<()> {
        match fs::read(path) {
            Ok(existing) if existing == contents => {
                // Repair outputs produced by older build scripts without rewriting
                // identical bytes (and therefore without changing their mtime).
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let metadata = fs::symlink_metadata(path)?;
                    if metadata.file_type().is_symlink() {
                        return Err(io::Error::other("refusing to chmod a shim symlink"));
                    }
                    if metadata.permissions().mode() & 0o777 != 0o755 {
                        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
                    }
                }
                return Ok(());
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
        // Same-directory rename publishes a complete file atomically. create_new
        // ensures even stale temporaries can never make us overwrite another file.
        let (temporary, mut file) = loop {
            let temporary = path.with_file_name(format!(
                ".{}.{}.{}.tmp",
                path.file_name().unwrap().to_string_lossy(),
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
            {
                Ok(file) => break (temporary, file),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        };
        let result = (|| {
            file.write_all(contents)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(fs::Permissions::from_mode(0o755))?;
            }
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn validate_static_elf(bytes: &[u8], expected_machine: u16) -> Result<(), String> {
        if bytes.len() < 64 || bytes.get(..4) != Some(b"\x7fELF") {
            return Err("not an ELF executable".to_owned());
        }
        if bytes[4] != 2 || bytes[5] != 1 {
            return Err("expected a 64-bit little-endian ELF executable".to_owned());
        }
        if !matches!(read_u16(bytes, 16)?, 2 | 3) {
            return Err("ELF is not an executable or position-independent executable".to_owned());
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
        if entry_count == 0 || entry_size < 56 {
            return Err("ELF has no usable program headers".to_owned());
        }
        let table_end = entry_count
            .checked_mul(entry_size)
            .and_then(|size| program_offset.checked_add(size))
            .ok_or_else(|| "program header table overflowed".to_owned())?;
        if table_end > bytes.len() {
            return Err("truncated ELF program header table".to_owned());
        }
        for index in 0..entry_count {
            let offset = index
                .checked_mul(entry_size)
                .and_then(|value| program_offset.checked_add(value))
                .ok_or_else(|| "program header offset overflowed".to_owned())?;
            if read_u32(bytes, offset)? == 3 {
                return Err(
                    "ELF requests a program interpreter and is dynamically linked".to_owned(),
                );
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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn shim_profiles_match_outer_build_mode_and_cargo_output_layout() {
            let debug = ShimProfile::for_outer_profile("debug");
            assert_eq!(debug.cargo_name, "dev");
            assert_eq!(debug.output_directory, "debug");
            let release = ShimProfile::for_outer_profile("release");
            assert_eq!(release.cargo_name, "shim-release");
            assert_eq!(release.output_directory, "shim-release");
        }

        #[test]
        fn target_names_and_consistency() {
            validate_targets(SHIM_TARGETS).unwrap();
            assert_eq!(SHIM_TARGETS[0].bin(), "linux-ssh");
            assert_eq!(SHIM_TARGETS[0].artifact(), "linux-ssh-x86_64");
            assert_eq!(SHIM_TARGETS[1].artifact(), "linux-ssh-aarch64");
            assert!(
                validate_targets(&[
                    ShimTarget { ..SHIM_TARGETS[0] },
                    ShimTarget { ..SHIM_TARGETS[0] }
                ])
                .unwrap_err()
                .contains("duplicate")
            );
            for invalid in [
                ShimTarget {
                    arch: "aarch64",
                    ..SHIM_TARGETS[0]
                },
                ShimTarget {
                    platform: "windows",
                    ..SHIM_TARGETS[0]
                },
                ShimTarget {
                    protocol: "../ssh",
                    ..SHIM_TARGETS[0]
                },
                ShimTarget {
                    protocol: "SSH",
                    ..SHIM_TARGETS[0]
                },
                ShimTarget {
                    target: "x86_64-unknown-linux-gnu",
                    ..SHIM_TARGETS[0]
                },
            ] {
                assert!(validate_targets(&[invalid]).is_err());
            }
        }

        #[test]
        fn sanitizes_outer_cargo_state_but_keeps_tool_discovery() {
            for name in [
                "CARGO_FEATURE_EMBED_SHIMS",
                "CARGO_CFG_TARGET_ARCH",
                "CARGO_ENCODED_RUSTFLAGS",
                "RUSTFLAGS",
                "CARGO_BUILD_TARGET",
                "CARGO_RESOLVER_LOCKFILE_PATH",
                "CARGO_MAKEFLAGS",
                "MAKEFLAGS",
                "OUT_DIR",
                "CARGO_BIN_EXE_cargo-zigbuild",
                "CLIPPY_ARGS",
                "CLIPPY_CONF_DIR",
                "DEP_FOO_ROOT",
            ] {
                assert!(inherited_build_variable(name), "{name}");
            }
            for name in [
                "PATH",
                "HOME",
                "CARGO_HOME",
                "RUSTUP_HOME",
                "CARGO",
                "RUSTC",
                "RUSTC_WRAPPER",
                "CARGO_ZIGBUILD_ZIG_PATH",
            ] {
                assert!(!inherited_build_variable(name), "{name}");
            }
            for name in ["RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"] {
                for wrapper in ["/tools/clippy-driver", "/tools/clippy-driver.exe"] {
                    assert!(inherited_analysis_wrapper(name, wrapper.as_ref()));
                }
                assert!(!inherited_analysis_wrapper(name, "/tools/sccache".as_ref()));
            }
            assert!(!inherited_analysis_wrapper("RUSTC", "rustc".as_ref()));
        }

        fn elf(machine: u16) -> Vec<u8> {
            let mut bytes = vec![0; 120];
            bytes[..4].copy_from_slice(b"\x7fELF");
            bytes[4] = 2;
            bytes[5] = 1;
            bytes[16..18].copy_from_slice(&2u16.to_le_bytes());
            bytes[18..20].copy_from_slice(&machine.to_le_bytes());
            bytes[32..40].copy_from_slice(&64u64.to_le_bytes());
            bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
            bytes[56..58].copy_from_slice(&1u16.to_le_bytes());
            bytes[64..68].copy_from_slice(&1u32.to_le_bytes());
            bytes
        }

        #[test]
        fn validates_static_elf_and_rejects_wrong_or_truncated_headers() {
            for machine in [62, 183] {
                let bytes = elf(machine);
                validate_static_elf(&bytes, machine).unwrap();
                for length in 0..bytes.len() {
                    assert!(validate_static_elf(&bytes[..length], machine).is_err());
                }
            }
            assert!(validate_static_elf(&elf(62), 183).is_err());
            let mut dynamic = elf(62);
            dynamic[64..68].copy_from_slice(&3u32.to_le_bytes());
            assert!(
                validate_static_elf(&dynamic, 62)
                    .unwrap_err()
                    .contains("interpreter")
            );
            let mut overflow = elf(62);
            overflow[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
            assert!(validate_static_elf(&overflow, 62).is_err());
            let mut object = elf(62);
            object[16..18].copy_from_slice(&1u16.to_le_bytes());
            assert!(validate_static_elf(&object, 62).is_err());
        }

        #[test]
        fn publishing_preserves_unchanged_mtime_and_unrelated_files() {
            let directory = std::env::temp_dir().join(format!(
                "skyhook-build-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&directory).unwrap();
            let destination = directory.join("linux-ssh-x86_64");
            let unrelated = directory.join("skyhook-shim-legacy");
            fs::write(&unrelated, b"user file").unwrap();
            write_if_changed(&destination, b"first").unwrap();
            let modified = fs::metadata(&destination).unwrap().modified().unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
                    0o755
                );
                fs::set_permissions(&destination, fs::Permissions::from_mode(0o644)).unwrap();
            }
            write_if_changed(&destination, b"first").unwrap();
            assert_eq!(
                fs::metadata(&destination).unwrap().modified().unwrap(),
                modified
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
                    0o755
                );
            }
            write_if_changed(&destination, b"second").unwrap();
            assert_eq!(fs::read(&destination).unwrap(), b"second");
            assert_eq!(fs::read(&unrelated).unwrap(), b"user file");
            assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);
            fs::remove_dir_all(&directory).unwrap();
        }
    }
}
