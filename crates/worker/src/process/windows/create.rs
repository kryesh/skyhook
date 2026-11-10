//! The process itself: its program, command line, environment and attributes.
use std::{
    ffi::{OsStr, OsString},
    io,
    os::windows::{
        ffi::OsStrExt as _,
        io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle},
    },
    path::{Path, PathBuf},
};

use windows_sys::Win32::{
    Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, TRUE},
    System::{
        Console::HPCON,
        Threading::{
            CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
            DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
            InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
            PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess,
            UpdateProcThreadAttribute,
        },
    },
};

use super::check;
use crate::{
    origin::ProcessEnvironment,
    process::{exclusively, windows_command_line},
};

/// A process created suspended, and its one thread.
pub(crate) struct Created {
    pub process: OwnedHandle,
    pub thread: OwnedHandle,
}

impl Created {
    /// Create `program`, found on the search path as `<program>.exe`, with
    /// `arguments`, this process's environment and `environment`'s variables over it,
    /// `stdio` as its standard streams and, with a `console`, attached to it.
    pub(crate) fn suspended(
        program: &str,
        arguments: &[OsString],
        environment: &ProcessEnvironment,
        stdio: [&OwnedHandle; 3],
        console: Option<HPCON>,
    ) -> io::Result<Self> {
        let program = find(&format!("{program}.exe"), environment)?;
        let line = [program.as_os_str()]
            .into_iter()
            .chain(arguments.iter().map(OsString::as_os_str));
        let mut line = windows_command_line(line.map(OsStr::encode_wide));
        line.push(0);
        let environment = environment_block(environment);
        exclusively(|| {
            let created = inherit(stdio, HANDLE_FLAG_INHERIT)
                .and_then(|()| create(&program, &mut line, &environment, stdio, console));
            // Still inheritable, they would reach whatever process is created next.
            let cleared = inherit(stdio, 0);
            let created = created?;
            if let Err(error) = cleared {
                // SAFETY: the process handle is open; the call takes no pointers.
                unsafe { TerminateProcess(created.process.as_raw_handle(), 1) };
                return Err(error);
            }
            Ok(created)
        })
    }
}

/// Set whether `handles` are inheritable.
fn inherit(handles: [&OwnedHandle; 3], flags: u32) -> io::Result<()> {
    handles.iter().try_for_each(|handle| {
        // SAFETY: the handle is open; its flags change no memory.
        check(unsafe { SetHandleInformation(handle.as_raw_handle(), HANDLE_FLAG_INHERIT, flags) })
    })
}

/// The first `name` on the search path `environment` sets, or this process's.
fn find(name: &str, environment: &ProcessEnvironment) -> io::Result<PathBuf> {
    let search = match environment.get("PATH") {
        Some(path) => OsString::from(path),
        None => std::env::var_os("PATH").unwrap_or_default(),
    };
    (std::env::split_paths(&search))
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{name} is not on PATH")))
}

/// This process's environment with `overrides` over it, as CreateProcessW takes it.
/// Names compare without case, as Windows compares them.
fn environment_block(overrides: &ProcessEnvironment) -> Vec<u16> {
    let overrides = (overrides.iter()).map(|(name, value)| (name.into(), value.into()));
    let mut variables = std::collections::BTreeMap::<OsString, (OsString, OsString)>::new();
    for (name, value) in std::env::vars_os().chain(overrides) {
        variables.insert(name.to_ascii_uppercase(), (name, value));
    }
    let mut block = Vec::new();
    for (name, value) in variables.into_values() {
        block.extend(name.encode_wide());
        block.push(u16::from(b'='));
        block.extend(value.encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

/// Create the process suspended, with `stdio`, inheritable, as its standard streams
/// and the only handles it inherits.
fn create(
    program: &Path,
    line: &mut [u16],
    environment: &[u16],
    stdio: [&OwnedHandle; 3],
    console: Option<HPCON>,
) -> io::Result<Created> {
    let handles: [HANDLE; 3] = stdio.map(|handle| handle.as_raw_handle());
    let mut attributes = Attributes::new(1 + u32::from(console.is_some()))?;
    // SAFETY: `handles` outlives the list, which is used only below.
    unsafe {
        attributes.set(
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            handles.as_ptr().cast(),
            size_of_val(&handles),
        )?;
        if let Some(console) = console {
            // The attribute's value is the console handle itself.
            attributes.set(
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                console as *const _,
                size_of::<HPCON>(),
            )?;
        }
    }
    // SAFETY: the structure is plain data, valid when zeroed.
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    [
        startup.StartupInfo.hStdInput,
        startup.StartupInfo.hStdOutput,
        startup.StartupInfo.hStdError,
    ] = handles;
    startup.lpAttributeList = attributes.list();
    let program: Vec<u16> = program.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: the structure is plain data, valid when zeroed.
    let mut created: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the strings are NUL-terminated, the command line is writable, the
    // environment block ends with two NULs, and every pointer outlives the call.
    check(unsafe {
        CreateProcessW(
            program.as_ptr(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            TRUE,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
            environment.as_ptr().cast(),
            std::ptr::null(),
            &raw const startup.StartupInfo,
            &raw mut created,
        )
    })?;
    // SAFETY: the handles were just created and are owned here alone.
    Ok(unsafe {
        Created {
            process: OwnedHandle::from_raw_handle(created.hProcess),
            thread: OwnedHandle::from_raw_handle(created.hThread),
        }
    })
}

/// A process creation attribute list, freed on drop.
struct Attributes(Vec<u64>);

impl Attributes {
    fn new(count: u32) -> io::Result<Self> {
        let mut size = 0;
        // SAFETY: a null list only asks for its size, failing as it reports it.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &raw mut size) };
        let mut list = Self(vec![0; size.div_ceil(8)]);
        // SAFETY: the buffer holds `size` bytes, aligned for the list.
        check(unsafe { InitializeProcThreadAttributeList(list.list(), count, 0, &raw mut size) })?;
        Ok(list)
    }

    fn list(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }

    /// # Safety
    /// `value` points to `size` bytes, or is the value itself where the attribute
    /// says so, and stays valid while the list is used.
    unsafe fn set(
        &mut self,
        attribute: u32,
        value: *const std::ffi::c_void,
        size: usize,
    ) -> io::Result<()> {
        // SAFETY: the list is initialized; the caller vouches for the value.
        check(unsafe {
            UpdateProcThreadAttribute(
                self.list(),
                0,
                attribute as usize,
                value,
                size,
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        })
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: the list was initialized in `new`.
        unsafe { DeleteProcThreadAttributeList(self.list()) };
    }
}
