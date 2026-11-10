//! Processes as Windows creates them: suspended, with only the handles they are given
//! and an optional pseudo console, and the overlapped pipes that are their standard
//! streams.

pub(crate) mod create;
pub(crate) mod pipe;

use windows_sys::Win32::Foundation::FALSE;

pub(crate) fn check(result: windows_sys::core::BOOL) -> std::io::Result<()> {
    (result != FALSE)
        .then_some(())
        .ok_or_else(std::io::Error::last_os_error)
}
