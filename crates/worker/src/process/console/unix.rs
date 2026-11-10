//! A pseudo-terminal as a process's controlling terminal.
use std::{
    io::{self, Read as _, Write as _},
    os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd},
};

use tokio::io::unix::AsyncFd;

/// A pseudo-terminal whose secondary side becomes a process's controlling terminal. Its
/// own descriptor of that side stays open, so reading the primary side never reports a
/// hangup while the process has not opened the terminal.
pub(crate) struct Console {
    primary: AsyncFd<std::fs::File>,
    secondary: OwnedFd,
}

impl Console {
    /// What ends a typed line.
    pub(crate) const ENTER: u8 = b'\n';

    pub(crate) fn open() -> io::Result<Self> {
        let (primary, secondary) = pseudo_terminal()?;
        // SAFETY: the file owns its descriptor, which stays open and unchanged until
        // the `AsyncFd` drops the file.
        let primary = unsafe { AsyncFd::register(std::fs::File::from(primary)) }?;
        Ok(Self { primary, secondary })
    }

    /// Start `command` in a session of its own with this as its controlling terminal.
    pub(crate) fn control(&self, command: &mut tokio::process::Command) {
        let secondary = self.secondary.as_raw_fd();
        // SAFETY: setsid and ioctl are async-signal-safe, and the hook neither
        // allocates nor touches shared state between fork and exec. The descriptor
        // is open in the child, which inherits it until exec closes it.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 || libc::ioctl(secondary, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    /// Everything the terminal shows once it shows something, or none once it closed.
    pub(crate) async fn shown(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut buffer = [0; 1024];
        let mut shown = Vec::new();
        loop {
            let result = self.primary.readable().await.and_then(|mut ready| {
                match ready.try_io(|file| file.get_ref().read(&mut buffer)) {
                    Ok(read) => read,
                    Err(_) => Err(io::ErrorKind::WouldBlock.into()),
                }
            });
            match result {
                Ok(0) => return Ok(None),
                Ok(read) => shown.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if !shown.is_empty() {
                        return Ok(Some(shown));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }

    pub(crate) async fn type_in(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let mut ready = self.primary.writable().await?;
            match ready.try_io(|file| file.get_ref().write(bytes)) {
                Ok(written) => bytes = &bytes[written?..],
                Err(_would_block) => {}
            }
        }
        Ok(())
    }
}

/// A non-blocking primary side and its secondary side, both closed on exec.
fn pseudo_terminal() -> io::Result<(OwnedFd, OwnedFd)> {
    let check = |result: libc::c_int| {
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result)
        }
    };
    let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
    // SAFETY: posix_openpt returns a new descriptor or -1.
    let primary = check(unsafe { libc::posix_openpt(flags | libc::O_NONBLOCK) })?;
    // SAFETY: the descriptor was just opened and is owned here alone.
    let primary = unsafe { OwnedFd::from_raw_fd(primary) };
    let raw = primary.as_raw_fd();
    // SAFETY: these take the primary descriptor, which is open.
    check(unsafe { libc::grantpt(raw) })?;
    check(unsafe { libc::unlockpt(raw) })?;
    let secondary = check(secondary(raw, flags))?;
    // SAFETY: the descriptor was just opened and is owned here alone.
    Ok((primary, unsafe { OwnedFd::from_raw_fd(secondary) }))
}

/// The secondary side of the primary `raw`, opened with `flags` (close-on-exec)
/// in one step, or -1.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn secondary(raw: libc::c_int, flags: libc::c_int) -> libc::c_int {
    // SAFETY: TIOCGPTPEER takes the open primary and returns a new descriptor or -1.
    unsafe { libc::ioctl(raw, libc::TIOCGPTPEER as _, flags) }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn secondary(raw: libc::c_int, flags: libc::c_int) -> libc::c_int {
    // SAFETY: ptsname returns a NUL-terminated path or null; open takes that path.
    unsafe {
        let name = libc::ptsname(raw);
        if name.is_null() {
            return -1;
        }
        libc::open(name, flags)
    }
}
