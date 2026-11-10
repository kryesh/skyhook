//! The remote shim: it serves one connection over its standard streams, or checks its own
//! hash before the host trusts it.
use std::{
    ffi::OsStr,
    path::Path,
    process::ExitCode,
    time::{Duration, SystemTime},
};

use skyhook_worker::{
    protocol::{SELF_CHECK, SERVE, serve::serve},
    target::{Os, Platform, Settings, ShimName, UPLOAD_PREFIX},
};

#[cfg(not(windows))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// How long an exiting shim waits for its runtime's threads: its connection has
/// already waited for its calls' blocking work, but a read of its standard input
/// cannot be cancelled.
const EXIT_GRACE: Duration = Duration::from_secs(1);

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let ran = tokio::runtime::Runtime::new()
        .map_err(Into::into)
        .and_then(|runtime| {
            let ran = runtime.block_on(run(&args));
            runtime.shutdown_timeout(EXIT_GRACE);
            ran
        });
    match ran {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("shim: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Do what `args`, the arguments after the program name, ask.
async fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    match args {
        [mode, root] if mode == SERVE => {
            let settings = Settings {
                authorization_root: root.clone(),
            };
            // Beside the connection, never holding it up.
            std::thread::spawn(prune_installed);
            let served = serve(tokio::io::stdin(), tokio::io::stdout(), settings).await;
            Ok(served.map(drop)?)
        }
        [mode, expected] if mode == SELF_CHECK => self_check(expected).await,
        _ => Err(format!("usage: {SERVE} AUTHORIZATION_ROOT | {SELF_CHECK} SHA256").into()),
    }
}

/// Remove the other shims installed beside this one for its platform that no client
/// has checked for a while, which newer installs left behind, and the uploads that
/// installs cut short left.
fn prune_installed() {
    let Ok(own) = std::env::current_exe().and_then(|exe| exe.canonicalize()) else {
        return;
    };
    let (Some(directory), Some(own)) = (own.parent(), own.file_name()) else {
        return;
    };
    prune(directory, own, Platform::current().os);
}

/// How long a shim goes unchecked before another prunes it. A client checks a shim,
/// which marks it used, just before it starts it, so one checked since may be about to
/// run; one that runs longer is unaffected on unix, and cannot be removed on Windows.
const UNUSED: Duration = Duration::from_secs(24 * 60 * 60);

/// Remove every file in `directory` named as a shim built for `os`, except `own`, or
/// as an upload, that was last marked used, or written, longer ago than [`UNUSED`].
/// What cannot be removed, such as a directory or a file in use, stays.
fn prune(directory: &Path, own: &OsStr, os: Os) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let installed = (name.to_str()).is_some_and(|name| {
            ShimName::parse(name, os).is_some() || name.starts_with(UPLOAD_PREFIX)
        });
        let modified = entry.metadata().and_then(|metadata| metadata.modified());
        let unused = modified.is_ok_and(|used| used.elapsed().is_ok_and(|age| age > UNUSED));
        if installed && name != own && unused {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Mark the shim at `path` used now, so that no other shim prunes it before it starts.
fn mark_used(path: &Path) -> std::io::Result<()> {
    let mut options = std::fs::File::options();
    // Changing a file's times needs no write access on unix, which a running
    // executable refuses; on Windows, only the right to write its attributes.
    options.read(true);
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::access_mode(
        &mut options,
        windows_sys::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES,
    );
    options.open(path)?.set_modified(SystemTime::now())
}

async fn self_check(expected: &str) -> Result<(), Box<dyn std::error::Error>> {
    let exe = std::env::current_exe()?;
    // Unmarked, it is pruned once it would have been anyway.
    let _ = mark_used(&exe);
    let bytes = tokio::fs::read(&exe).await?;
    let actual = ShimName::of(&bytes, Platform::current().os)
        .digest()
        .to_string();
    if actual != expected {
        return Err(format!("shim hash mismatch: expected {expected}, got {actual}").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pruning removes only other shims for its own platform and uploads, and only those
    /// unused for a while: a shim another client checked lately may be about to start,
    /// and an upload written lately may be about to be installed. Whatever else is in
    /// the directory stays.
    #[test]
    fn pruning_removes_only_other_long_unused_shims_for_its_platform() {
        let directory = tempfile::tempdir().unwrap();
        let hash = |digit: char| digit.to_string().repeat(64);
        let file = |name: &str| directory.path().join(name);
        let unused = |name: &str| {
            let file = std::fs::File::options()
                .write(true)
                .open(file(name))
                .unwrap();
            let long_ago = std::time::SystemTime::now() - UNUSED - UNUSED;
            file.set_modified(long_ago).unwrap();
        };
        let own = hash('a');
        let older = hash('b');
        let windows = format!("{}.exe", hash('c'));
        let checked = hash('9');
        let uploading = format!("{UPLOAD_PREFIX}fedcba9876543210");
        let abandoned = format!("{UPLOAD_PREFIX}0123456789abcdef.exe");
        let kept = [
            own.clone(),
            windows.clone(),
            checked.clone(),
            uploading.clone(),
            "notes.txt".to_owned(),
            hash('D'),
            hash('e')[1..].to_owned(),
        ];
        for name in kept.iter().chain([&older, &abandoned]) {
            std::fs::write(file(name), b"").unwrap();
            unused(name);
        }
        // Another client checked this one since, which marked it used, and wrote
        // this upload.
        for recent in [&checked, &uploading] {
            mark_used(&file(recent)).unwrap();
        }
        std::fs::create_dir(file(&hash('f'))).unwrap();
        prune(directory.path(), own.as_ref(), Os::Linux);
        let mut left: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        let mut expected: Vec<_> = kept.into_iter().chain([hash('f')]).collect();
        expected.sort();
        assert_eq!(left, expected);
        // A Windows shim keeps a bare digest and removes its own platform's form.
        prune(directory.path(), own.as_ref(), Os::Windows);
        assert!(!file(&windows).exists());
        assert!(file(&own).exists());
    }
}
