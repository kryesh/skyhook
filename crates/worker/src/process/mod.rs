//! Processes on this machine: what a command runs, the tree a spawned child leads, and,
//! behind each OS's own module, how a process is created with its pipes and the terminal
//! or console it may be given.

mod console;
mod group;
mod shell;
#[cfg(windows)]
pub(crate) mod windows;

use std::io;

pub(crate) use console::Console;
pub use group::ProcessGroup;
pub(crate) use shell::Shell;
pub use shell::{CommandLine, InvalidCommand, powershell_script};

/// Spawn `command`. On Windows no other spawn in this process runs meanwhile, so the
/// handles one makes inheritable reach only its own child.
pub(crate) fn spawn(command: &mut tokio::process::Command) -> io::Result<tokio::process::Child> {
    #[cfg(windows)]
    return exclusively(|| command.spawn());
    #[cfg(unix)]
    command.spawn()
}

/// Run `create`, which creates processes, while no other spawn in this process runs.
#[cfg(windows)]
pub(crate) fn exclusively<T>(create: impl FnOnce() -> T) -> T {
    static SPAWNING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _alone = SPAWNING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    create()
}

/// `arguments` as one Windows command line, each quoted only where the C runtime needs
/// quotes to split it back out, in units such as `char`s or UTF-16.
pub fn windows_command_line<T: Copy + PartialEq + From<u8>>(
    arguments: impl IntoIterator<Item = impl IntoIterator<Item = T>>,
) -> Vec<T> {
    let [space, tab, newline, vertical_tab, quote, backslash] =
        [b' ', b'\t', b'\n', 0x0b, b'"', b'\\'].map(T::from);
    let mut line = Vec::new();
    for (index, argument) in arguments.into_iter().enumerate() {
        if index > 0 {
            line.push(space);
        }
        let units: Vec<T> = argument.into_iter().collect();
        let quoted = units.is_empty()
            || (units.iter()).any(|unit| [space, tab, newline, vertical_tab, quote].contains(unit));
        if !quoted {
            line.extend(units);
            continue;
        }
        line.push(quote);
        let mut backslashes = 0;
        for unit in units {
            if unit == backslash {
                backslashes += 1;
                continue;
            }
            // Backslashes escape only a quote after them, as each escapes the other.
            let escaped = if unit == quote {
                backslashes * 2 + 1
            } else {
                backslashes
            };
            line.extend(std::iter::repeat_n(backslash, escaped));
            line.push(unit);
            backslashes = 0;
        }
        line.extend(std::iter::repeat_n(backslash, backslashes * 2));
        line.push(quote);
    }
    line
}

#[cfg(test)]
mod tests {
    /// Only what needs quotes is quoted, and backslashes double only before a quote.
    #[test]
    fn windows_arguments_are_quoted_as_the_c_runtime_splits_them() {
        let arguments = [
            "plain",
            "",
            "two words",
            r#"say "hi""#,
            r"C:\dir\",
            r"C:\My Work\",
            r#"a\\"b"#,
        ];
        let line: String = super::windows_command_line(arguments.map(str::chars))
            .into_iter()
            .collect();
        assert_eq!(
            line,
            r#"plain "" "two words" "say \"hi\"" C:\dir\ "C:\My Work\\" "a\\\\\"b""#
        );
    }
}
