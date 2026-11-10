//! The terminal a process can be given while its standard streams stay pipes: a
//! pseudo-terminal on unix, a pseudo console on Windows.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(crate) use unix::Console;
#[cfg(windows)]
pub(crate) use windows::Console;
