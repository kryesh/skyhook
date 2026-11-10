//! The commands that deploy and start a shim, in the language of the login shell a
//! target's OS has: POSIX sh, or Windows PowerShell 5.1 started from cmd.exe, Windows
//! OpenSSH's default shell.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use skyhook_worker::{
    process::windows_command_line,
    protocol::{SELF_CHECK, SERVE},
    target::{Os, Platform, ShimName},
};

use crate::remote::error::DeploymentError;

/// What a start prints before its probe when no installed shim could start.
const PROBED: &str = "skyhook-probe";
/// What an install prints once the uploaded shim is in place.
pub(super) const INSTALLED: &str = "skyhook-installed";

/// Where shims are installed on unix: under the remote user's cache directory, as
/// its shell expands it.
const SHIM_DIR: &str = "${XDG_CACHE_HOME:-$HOME/.cache}/skyhook/shims";
/// Where shims are installed on Windows, relative to `%LOCALAPPDATA%`: the user's own,
/// non-roaming application data, whose inherited access is the user's alone.
const WINDOWS_SHIM_DIR: &str = r"skyhook\shims";

/// A command that runs the first of `installed` that is there and intact, serving
/// `root` on the command's own standard streams; when none is, it prints what
/// [`platform`] reads.
pub(super) fn start(os: Os, installed: &[ShimName], root: &str) -> String {
    match os {
        Os::Linux => {
            let started: String = (installed.iter())
                .map(|shim| {
                    let (hash, path) = (shim.digest(), format!("{SHIM_DIR}/{shim}"));
                    let serve = serve(os, shim, root);
                    format!("test -x \"{path}\" && \"{path}\" {SELF_CHECK} {hash} && {serve}; ")
                })
                .collect();
            format!("{started}echo {PROBED}; uname -s; uname -m")
        }
        Os::Windows => {
            let started: String = (installed.iter())
                .map(|shim| {
                    format!(
                        "$shim = Join-Path $env:LOCALAPPDATA '{WINDOWS_SHIM_DIR}\\{shim}'\n\
                         if (Test-Path -LiteralPath $shim -PathType Leaf) {{\n\
                         & $shim {SELF_CHECK} {}\n\
                         if ($LASTEXITCODE -eq 0) {{\n{}\n}}\n\
                         }}\n",
                        shim.digest(),
                        powershell_serve(shim, root)
                    )
                })
                .collect();
            powershell(&format!("{started}'{PROBED}'\n$env:PROCESSOR_ARCHITECTURE"))
        }
    }
}

/// The platform a start that started no shim printed, after whatever the shell
/// printed before it: the `os` declared, or the probe names another system and the
/// declaration is refused.
pub(super) fn platform(os: Os, printed: &str) -> Result<Platform, DeploymentError> {
    let lines = printed.lines().map(str::trim);
    let mut lines = lines.skip_while(|line| *line != PROBED).skip(1);
    let (system, arch) = match os {
        Os::Linux => (lines.next(), lines.next()),
        Os::Windows => (Some(Os::Windows.as_str()), lines.next()),
    };
    let (Some(system), Some(arch)) = (system, arch) else {
        return Err(DeploymentError::Probe);
    };
    let unsupported = || DeploymentError::UnsupportedPlatform {
        os: system.to_owned(),
        arch: arch.to_owned(),
    };
    let reported = probed_os(system).ok_or_else(unsupported)?;
    if reported != os {
        let declared = os;
        return Err(DeploymentError::OsMismatch { declared, reported });
    }
    let arch = arch.to_ascii_lowercase().parse();
    Ok(Platform {
        os,
        arch: arch.map_err(|_| unsupported())?,
    })
}

/// The system a probe names. A POSIX shell on Windows answers `uname -s` with its
/// own layer and the kernel under it, as `MINGW64_NT-10.0-22631`, `CYGWIN_NT-10.0-19045`
/// and `Windows_NT` do.
fn probed_os(system: &str) -> Option<Os> {
    let system = system.to_ascii_lowercase();
    match system.contains("_nt") {
        true => Some(Os::Windows),
        false => system.parse().ok(),
    }
}

/// A command that writes its standard input to `upload` beside the installed shims,
/// installs it as `shim` once it hashes as that name says, printing [`INSTALLED`], and
/// otherwise removes it.
pub(super) fn install(os: Os, shim: &ShimName, upload: &str) -> String {
    let hash = shim.digest();
    match os {
        Os::Linux => format!(
            "umask 077; d=\"{SHIM_DIR}\"; t=\"$d/{upload}\"; mkdir -p \"$d\" && trap 'rm -f \"$t\"' EXIT HUP INT TERM && cat > \"$t\" && chmod 700 \"$t\" && \"$t\" {SELF_CHECK} {hash} && mv -f \"$t\" \"$d/{shim}\" && echo {INSTALLED}"
        ),
        // [Console]::OpenStandardInput() stops reading part way through the pipe
        // Windows OpenSSH gives a command, so the upload reads the handle as a file.
        Os::Windows => powershell(&format!(
            "$ErrorActionPreference = 'Stop'\n\
             $directory = Join-Path $env:LOCALAPPDATA '{WINDOWS_SHIM_DIR}'\n\
             $upload = Join-Path $directory '{upload}'\n\
             try {{\n\
             [void](New-Item -ItemType Directory -Force -Path $directory)\n\
             $std = Add-Type -Name Std -Namespace Skyhook -PassThru -MemberDefinition '[DllImport(\"kernel32.dll\")] public static extern IntPtr GetStdHandle(int handle);'\n\
             $handle = [Microsoft.Win32.SafeHandles.SafeFileHandle]::new($std::GetStdHandle(-10), $false)\n\
             $stdin = [IO.FileStream]::new($handle, [IO.FileAccess]::Read)\n\
             $file = [IO.File]::Create($upload)\n\
             try {{ $stdin.CopyTo($file) }} finally {{ $file.Close() }}\n\
             & $upload {SELF_CHECK} {hash}\n\
             if ($LASTEXITCODE -eq 0) {{\n\
             Move-Item -LiteralPath $upload -Destination (Join-Path $directory '{shim}') -Force\n\
             '{INSTALLED}'\n\
             }}\n\
             }} finally {{\n\
             if (Test-Path -LiteralPath $upload) {{ Remove-Item -LiteralPath $upload -Force }}\n\
             }}"
        )),
    }
}

/// A command that runs the installed `shim`, serving `root` on the command's own
/// standard streams.
pub(super) fn serve(os: Os, shim: &ShimName, root: &str) -> String {
    match os {
        Os::Linux => {
            let root = shell_quote(root);
            format!("exec \"{SHIM_DIR}/{shim}\" {SERVE} {root}")
        }
        Os::Windows => powershell(&powershell_serve(shim, root)),
    }
}

/// A PowerShell script running the installed `shim` as [`serve`] does.
/// `&` would quote the arguments itself, which Windows PowerShell 5.1 gets wrong for
/// an empty one or one ending in a backslash. The shim inherits the streams either
/// way, so its protocol passes through byte for byte.
fn powershell_serve(shim: &ShimName, root: &str) -> String {
    let arguments = [SERVE, root].into_iter().map(str::chars);
    let arguments: String = windows_command_line(arguments).into_iter().collect();
    format!(
        "$process = [Diagnostics.Process]::new()\n\
         $process.StartInfo.FileName = Join-Path $env:LOCALAPPDATA '{WINDOWS_SHIM_DIR}\\{shim}'\n\
         $process.StartInfo.Arguments = {}\n\
         $process.StartInfo.UseShellExecute = $false\n\
         [void]$process.Start()\n\
         $process.WaitForExit()\n\
         exit $process.ExitCode",
        powershell_literal(&arguments)
    )
}

/// `script` as a cmd.exe command running it in Windows PowerShell 5.1. cmd.exe ends a
/// command at its first newline, so the script travels encoded and runs as a script
/// block; `-EncodedCommand` would instead report errors as CLIXML and collapse exit codes.
fn powershell(script: &str) -> String {
    let script = STANDARD.encode(skyhook_worker::process::powershell_script(script));
    format!(
        "powershell.exe -NoProfile -NonInteractive -Command \"& ([scriptblock]::Create([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{script}'))))\""
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// `value` as a single-quoted PowerShell string: PowerShell reads each of `'` and
/// U+2018 to U+201B as a single quotation mark, and a doubled one as itself.
fn powershell_literal(value: &str) -> String {
    let mut literal = String::from('\'');
    for character in value.chars() {
        if matches!(character, '\'' | '\u{2018}'..='\u{201B}') {
            literal.push(character);
        }
        literal.push(character);
    }
    literal.push('\'');
    literal
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The script a PowerShell command runs.
    fn script(command: &str) -> String {
        let encoded = command.split('\'').nth(1).unwrap();
        String::from_utf8(STANDARD.decode(encoded).unwrap()).unwrap()
    }

    /// A start tries each installed shim for the declared OS, then probes: a unix
    /// shell reports both halves of the platform, PowerShell only the architecture of a
    /// Windows machine, each read after whatever a login script printed before it. A
    /// probe that names another system than the one declared refuses the connection.
    #[test]
    fn a_start_tries_the_installed_shims_then_probes_the_platform() {
        let installed = [b"one", b"two"].map(|bytes| ShimName::of(bytes, Os::Linux));
        let started = start(Os::Linux, &installed, "/root");
        let tried: Vec<_> = started.match_indices(SELF_CHECK).collect();
        assert_eq!(tried.len(), 2);
        assert!(
            started.ends_with("echo skyhook-probe; uname -s; uname -m"),
            "{started}"
        );
        let printed = "Last login: today\nskyhook-probe\nLinux\nx86_64\n";
        let linux = platform(Os::Linux, printed).unwrap();
        assert_eq!(linux.to_string(), "linux-x86_64");
        assert!(matches!(
            platform(Os::Linux, "Linux\nx86_64\n"),
            Err(DeploymentError::Probe)
        ));
        // A POSIX shell on Windows runs the probe and names the system under it,
        // which is not the one declared; any other system is unsupported as itself.
        for system in [
            "MINGW64_NT-10.0-22631",
            "CYGWIN_NT-10.0-19045",
            "Windows_NT",
        ] {
            let printed = format!("skyhook-probe\n{system}\nx86_64\n");
            assert!(matches!(
                platform(Os::Linux, &printed),
                Err(DeploymentError::OsMismatch {
                    declared: Os::Linux,
                    reported: Os::Windows
                })
            ));
        }
        assert!(matches!(
            platform(Os::Linux, "skyhook-probe\nDarwin\narm64\n"),
            Err(DeploymentError::UnsupportedPlatform { os, .. }) if os == "Darwin"
        ));
        let installed = [ShimName::of(b"one", Os::Windows)];
        let started = script(&start(Os::Windows, &installed, r"C:\root"));
        assert!(
            started.contains("\n'skyhook-probe'\n$env:PROCESSOR_ARCHITECTURE\n"),
            "{started}"
        );
        let windows = |printed| platform(Os::Windows, printed);
        for (printed, expected) in [
            ("skyhook-probe\r\nAMD64\r\n", "windows-x86_64"),
            ("skyhook-probe\nARM64", "windows-aarch64"),
        ] {
            assert_eq!(windows(printed).unwrap().to_string(), expected);
        }
        assert!(matches!(
            windows("skyhook-probe\r\nx86\r\n"),
            Err(DeploymentError::UnsupportedPlatform { os, arch }) if os == "windows" && arch == "x86"
        ));
        assert!(matches!(windows(""), Err(DeploymentError::Probe)));
    }

    /// The shim receives its root as written, through PowerShell's quoting and the
    /// command line's: a trailing backslash, quotation marks.
    #[test]
    fn windows_serve_arguments_reach_the_shim_as_written() {
        let shim = ShimName::of(b"shim", Os::Windows);
        let command = serve(Os::Windows, &shim, r#"C:\o'brien’s "x"\"#);
        assert!(command.starts_with("powershell.exe -NoProfile -NonInteractive -Command \"& "));
        assert!(!command.contains('\n'));
        let arguments = r#"'--serve "C:\o''brien’’s \"x\"\\"'"#;
        let script = script(&command);
        assert!(
            script.contains(&format!("$process.StartInfo.Arguments = {arguments}\n")),
            "{script}"
        );
        assert!(script.contains(&format!(r"'skyhook\shims\{shim}'")));
    }
}
