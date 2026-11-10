//! A child's standard stream is a pipe laid out as CreatePipe lays one out: the
//! reading end serves it and the writing end is its client, each with the access
//! CreatePipe gives. This process's end overlaps, so reading or writing it ties up no
//! thread. The child's end is inheritable only while the child is created, and each
//! pipe takes one client, connected before it is returned.
use std::{
    io,
    os::windows::{
        fs::OpenOptionsExt as _,
        io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle},
    },
};

use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::{
    Foundation::{
        ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, FALSE, GENERIC_WRITE, INVALID_HANDLE_VALUE, TRUE,
    },
    Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_READ_ATTRIBUTES,
        PIPE_ACCESS_INBOUND, WriteFile,
    },
    System::{
        IO::{GetOverlappedResult, OVERLAPPED},
        Pipes::{ConnectNamedPipe, CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE},
        Threading::CreateEventW,
    },
};

use super::check;

/// Each direction of a pipe buffers this much.
const PIPE_BYTES: u32 = 64 * 1024;

/// How the child uses its end of a pipe.
#[derive(Clone, Copy)]
pub(crate) enum Theirs {
    /// With completion routines, as OpenSSH uses its standard streams: on a
    /// synchronous pipe its writes never complete.
    Overlapped,
    /// With plain reads and writes, as a pseudo console uses its streams.
    Synchronous,
}

impl Theirs {
    fn flags(self) -> u32 {
        match self {
            Self::Overlapped => FILE_FLAG_OVERLAPPED,
            Self::Synchronous => 0,
        }
    }
}

/// A pipe the child writes and this process reads.
pub(crate) fn from_child(theirs: Theirs) -> io::Result<(NamedPipeServer, OwnedHandle)> {
    let name = pipe_name()?;
    let ours = ServerOptions::new()
        .access_inbound(true)
        .access_outbound(false)
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .max_instances(1)
        .in_buffer_size(PIPE_BYTES)
        .out_buffer_size(PIPE_BYTES)
        .create(&name)?;
    let theirs = std::fs::OpenOptions::new()
        .access_mode(GENERIC_WRITE | FILE_READ_ATTRIBUTES)
        .custom_flags(theirs.flags())
        .open(&name)?;
    // The client has connected, so connecting completes at once.
    let connected = futures_util::FutureExt::now_or_never(ours.connect());
    connected.unwrap_or_else(|| Err(io::ErrorKind::NotConnected.into()))?;
    Ok((ours, theirs.into()))
}

/// A pipe this process writes and the child reads.
pub(crate) fn to_child(theirs: Theirs) -> io::Result<(NamedPipeClient, OwnedHandle)> {
    let name = pipe_name()?;
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: the name is NUL-terminated; the result is a new handle or invalid.
    let server = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE | theirs.flags(),
            PIPE_TYPE_BYTE | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            PIPE_BYTES,
            PIPE_BYTES,
            0,
            std::ptr::null(),
        )
    };
    if server == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the handle was just created and is owned here alone.
    let server = unsafe { OwnedHandle::from_raw_handle(server) };
    let ours = ClientOptions::new().read(false).write(true).open(&name)?;
    // SAFETY: the handle is open; with the client connected this returns at once.
    if unsafe { ConnectNamedPipe(server.as_raw_handle(), std::ptr::null_mut()) } == FALSE {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
            return Err(error);
        }
    }
    Ok((ours, server))
}

/// A new pipe's name, which no one can guess to take first.
fn pipe_name() -> io::Result<String> {
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let random = skyhook_base::hex::Digest::from_bytes(random);
    Ok(format!(r"\\.\pipe\skyhook-{}-{random}", std::process::id()))
}

/// Write `bytes` whole to the overlapped `pipe`, waiting until they are written.
pub(crate) fn write(pipe: &OwnedHandle, bytes: &[u8]) -> io::Result<()> {
    // SAFETY: all arguments may be null; the result is a new event or null.
    let event = unsafe { CreateEventW(std::ptr::null(), TRUE, FALSE, std::ptr::null()) };
    if event.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the event was just created and is owned here alone.
    let event = unsafe { OwnedHandle::from_raw_handle(event) };
    // SAFETY: the structure is plain data, valid when zeroed.
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.hEvent = event.as_raw_handle();
    let length = u32::try_from(bytes.len()).map_err(io::Error::other)?;
    let mut written = 0;
    // SAFETY: the buffer and the structure outlive the write, which is waited for
    // below before either is dropped.
    unsafe {
        if WriteFile(
            pipe.as_raw_handle(),
            bytes.as_ptr(),
            length,
            std::ptr::null_mut(),
            &raw mut overlapped,
        ) == FALSE
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(error);
            }
        }
        check(GetOverlappedResult(
            pipe.as_raw_handle(),
            &raw const overlapped,
            &raw mut written,
            TRUE,
        ))?;
    }
    match written as usize == bytes.len() {
        true => Ok(()),
        false => Err(io::ErrorKind::WriteZero.into()),
    }
}
