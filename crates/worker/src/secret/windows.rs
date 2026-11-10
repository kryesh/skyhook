//! A private file on Windows, by the rule Win32-OpenSSH applies to private keys
//! (`check_secure_file_permission` in `contrib/win32/win32compat/w32-sshfileperm.c`):
//! its owner and every principal an allow entry of its access control list names is
//! you, SYSTEM, Administrators or TrustedInstaller. Deny entries only take access
//! away, so they are not read; inherited entries count like explicit ones, and
//! conditional ones as if their condition held. Unlike OpenSSH, which skips them, an
//! entry of any other kind, such as an object entry that may grant access too,
//! fails the check.

use std::{
    io,
    os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle},
};

use windows_sys::Win32::{
    Foundation::{FALSE, LocalFree},
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
        Authorization::{ConvertSidToStringSidW, GetSecurityInfo, SE_FILE_OBJECT},
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetTokenInformation, IsWellKnownSid,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SID_IDENTIFIER_AUTHORITY,
        TOKEN_QUERY, TOKEN_USER, TokenUser, WinBuiltinAdministratorsSid, WinLocalSystemSid,
    },
    System::{
        SystemServices::{
            ACCESS_ALLOWED_ACE_TYPE, ACCESS_ALLOWED_CALLBACK_ACE_TYPE, ACCESS_DENIED_ACE_TYPE,
            ACCESS_DENIED_CALLBACK_ACE_TYPE, ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE,
            ACCESS_DENIED_OBJECT_ACE_TYPE,
        },
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

use super::{FileProblem, Sid};

/// `NT SERVICE\TrustedInstaller`, S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464.
#[repr(C)]
struct ServiceSid {
    revision: u8,
    count: u8,
    authority: SID_IDENTIFIER_AUTHORITY,
    sub_authorities: [u32; 6],
}

static TRUSTED_INSTALLER: ServiceSid = ServiceSid {
    revision: 1,
    count: 6,
    authority: SID_IDENTIFIER_AUTHORITY {
        Value: [0, 0, 0, 0, 0, 5],
    },
    sub_authorities: [
        80, 956008885, 3418522649, 1831038044, 1853292631, 2271478464,
    ],
};

/// Everyone, the principal a missing access control list grants everything to.
const EVERYONE: &str = "S-1-1-0";

pub(super) fn check_private(file: &std::fs::File) -> Result<(), FileProblem> {
    let unreadable = |error: io::Error| FileProblem::File(error.into());
    let user = TokenUserSid::current().map_err(unreadable)?;
    let descriptor = Descriptor::of(file).map_err(unreadable)?;
    let trusted = |sid: PSID| {
        // SAFETY: both SIDs are valid for the duration of the calls: `sid` lives in the
        // descriptor or its list, and the others are owned here or static.
        unsafe {
            IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != FALSE
                || IsWellKnownSid(sid, WinLocalSystemSid) != FALSE
                || EqualSid(sid, user.sid()) != FALSE
                || EqualSid(sid, (&raw const TRUSTED_INSTALLER).cast_mut().cast()) != FALSE
        }
    };
    if !trusted(descriptor.owner) {
        return Err(FileProblem::ForeignOwner(
            sid_text(descriptor.owner).map_err(unreadable)?,
        ));
    }
    if descriptor.dacl.is_null() {
        return Err(FileProblem::Granted(Sid(EVERYONE.into())));
    }
    // SAFETY: a non-null list GetSecurityInfo returned lives in the descriptor.
    let count = unsafe { (*descriptor.dacl).AceCount };
    for index in 0..u32::from(count) {
        let mut ace = std::ptr::null_mut();
        // SAFETY: the list is valid and `index` is below its entry count.
        if unsafe { GetAce(descriptor.dacl, index, &raw mut ace) } == FALSE {
            return Err(unreadable(io::Error::last_os_error()));
        }
        // SAFETY: every entry starts with its header.
        let kind = unsafe { (*ace.cast::<ACE_HEADER>()).AceType };
        let sid = match u32::from(kind) {
            ACCESS_DENIED_ACE_TYPE
            | ACCESS_DENIED_CALLBACK_ACE_TYPE
            | ACCESS_DENIED_OBJECT_ACE_TYPE
            | ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE => continue,
            // SAFETY: a conditional allow entry is laid out as a plain one, its condition
            // after the SID that starts at `SidStart`.
            ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE => unsafe {
                (&raw mut (*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart).cast()
            },
            _ => return Err(FileProblem::Unchecked(kind)),
        };
        if !trusted(sid) {
            return Err(FileProblem::Granted(sid_text(sid).map_err(unreadable)?));
        }
    }
    Ok(())
}

/// A file's owner and access control list, freed with the descriptor holding them.
struct Descriptor {
    owner: PSID,
    dacl: *mut ACL,
    descriptor: PSECURITY_DESCRIPTOR,
}

impl Descriptor {
    fn of(file: &std::fs::File) -> io::Result<Self> {
        let mut owner = std::ptr::null_mut();
        let mut dacl = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: the handle is open with read access, which includes reading its
        // security; every out pointer is valid.
        let error = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &raw mut owner,
                std::ptr::null_mut(),
                &raw mut dacl,
                std::ptr::null_mut(),
                &raw mut descriptor,
            )
        };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        Ok(Self {
            owner,
            dacl,
            descriptor,
        })
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: GetSecurityInfo allocated the descriptor with LocalAlloc.
        unsafe { LocalFree(self.descriptor) };
    }
}

/// The user this process runs as.
struct TokenUserSid(Vec<u64>);

impl TokenUserSid {
    fn current() -> io::Result<Self> {
        let mut token = std::ptr::null_mut();
        // SAFETY: the pseudo-handle needs no closing; the out pointer is valid.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == FALSE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the token was just opened and is owned here alone.
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut length = 0;
        // SAFETY: a null buffer only asks for the length.
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                std::ptr::null_mut(),
                0,
                &raw mut length,
            )
        };
        // Aligned for TOKEN_USER, which the buffer starts with.
        let mut buffer = vec![0_u64; (length as usize).div_ceil(8)];
        // SAFETY: the buffer holds `length` bytes.
        let read = unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buffer.as_mut_ptr().cast(),
                length,
                &raw mut length,
            )
        };
        if read == FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(buffer))
    }

    fn sid(&self) -> PSID {
        // SAFETY: the buffer holds a TOKEN_USER, whose SID points into the buffer.
        unsafe { (*self.0.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }
}

/// A SID's text form, such as `S-1-5-32-545`.
fn sid_text(sid: PSID) -> io::Result<Sid> {
    let mut text = std::ptr::null_mut();
    // SAFETY: the SID is valid; the result is a new string or none.
    if unsafe { ConvertSidToStringSidW(sid, &raw mut text) } == FALSE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the string is NUL-terminated UTF-16 allocated with LocalAlloc.
    unsafe {
        let length = (0..).take_while(|&index| *text.add(index) != 0).count();
        let spelled = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
        LocalFree(text.cast());
        Ok(Sid(spelled))
    }
}
