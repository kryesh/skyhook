//! A pseudo console as a process's console. What it shows arrives as a
//! rendering, text among control sequences that draw it, so only the text counts as
//! output: drawing the empty console when it opens shows nothing.
use std::{io, os::windows::io::AsRawHandle as _};

use tokio::{
    io::AsyncWriteExt as _,
    net::windows::named_pipe::{NamedPipeClient, NamedPipeServer},
};
use windows_sys::Win32::System::Console::{COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON};

use crate::process::{
    exclusively,
    windows::pipe::{Theirs, from_child, to_child},
};

/// Wide enough that no prompt wraps, which would draw a line end inside it.
const SIZE: COORD = COORD { X: 4096, Y: 25 };

pub(crate) struct Console {
    // The pipes close before the console: closing it waits, on older Windows, until
    // its last rendering is read or cannot be written.
    input: NamedPipeClient,
    output: NamedPipeServer,
    rendering: Rendering,
    console: PseudoConsole,
}

struct PseudoConsole(HPCON);

impl Drop for PseudoConsole {
    fn drop(&mut self) {
        // SAFETY: the console was created and is closed once.
        unsafe { ClosePseudoConsole(self.0) };
    }
}

impl Console {
    /// What ends a typed line: the Enter key.
    pub(crate) const ENTER: u8 = b'\r';

    pub(crate) fn open() -> io::Result<Self> {
        let (input, keys) = to_child(Theirs::Synchronous)?;
        let (output, screen) = from_child(Theirs::Synchronous)?;
        let mut console = 0;
        // SAFETY: both handles are open and the out pointer is valid. The console
        // keeps its own duplicates of the handles.
        let result = exclusively(|| unsafe {
            CreatePseudoConsole(
                SIZE,
                keys.as_raw_handle(),
                screen.as_raw_handle(),
                0,
                &raw mut console,
            )
        });
        if result < 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        Ok(Self {
            input,
            output,
            rendering: Rendering::default(),
            console: PseudoConsole(console),
        })
    }

    pub(crate) fn handle(&self) -> HPCON {
        self.console.0
    }

    /// Everything the console shows once it shows some text, or none once it closed.
    pub(crate) async fn shown(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut buffer = [0; 1024];
        let mut shown = Vec::new();
        loop {
            self.output.readable().await?;
            match self.output.try_read(&mut buffer) {
                Ok(0) => return Ok(None),
                Ok(read) => self.rendering.text(&buffer[..read], &mut shown),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if !shown.is_empty() {
                        return Ok(Some(shown));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return Ok(None),
                Err(error) => return Err(error),
            }
        }
    }

    pub(crate) async fn type_in(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.input.write_all(bytes).await
    }
}

/// Where a rendering stands between reads: in text, or inside a control sequence,
/// which a read may split.
#[derive(Clone, Copy, Default)]
enum Rendering {
    #[default]
    Text,
    /// After ESC, and any intermediate bytes.
    Escape,
    /// A control sequence (ESC `[`), until its final byte.
    Control,
    /// A string such as a window title (ESC `]`), until BEL or ESC `\`.
    String,
    StringEscape,
}

impl Rendering {
    /// Append the text `rendered` shows to `text`: everything but control sequences
    /// and control characters, line feeds kept.
    fn text(&mut self, rendered: &[u8], text: &mut Vec<u8>) {
        for &byte in rendered {
            *self = match (*self, byte) {
                (Self::Text, 0x1b) => Self::Escape,
                (Self::Text, b'\n' | 0x20..=0x7e | 0x80..) => {
                    text.push(byte);
                    Self::Text
                }
                (Self::Text, _) => Self::Text,
                (Self::Escape, b'[') => Self::Control,
                (Self::Escape, b']' | b'P' | b'X' | b'^' | b'_') => Self::String,
                (Self::Escape, 0x20..0x30) => Self::Escape,
                (Self::Escape, _) => Self::Text,
                (Self::Control, 0x40..0x7f) => Self::Text,
                (Self::Control, _) => Self::Control,
                (Self::String | Self::StringEscape, 0x07) | (Self::StringEscape, b'\\') => {
                    Self::Text
                }
                (Self::String | Self::StringEscape, 0x1b) => Self::StringEscape,
                (Self::String | Self::StringEscape, _) => Self::String,
            };
        }
    }
}
