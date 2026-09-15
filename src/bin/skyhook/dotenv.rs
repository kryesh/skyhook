//! Invocation-local environment defaults, loaded before any worker threads exist.
use std::{fmt, fs::File, io::Read};

#[derive(Debug)]
pub(super) enum LoadError {
    Read,
    Parse,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Dotenv parser errors include the original line (often a credential).
        // Never retain or display parser errors, file contents, or variable values.
        f.write_str(match self {
            Self::Read => "could not read invocation directory .env file",
            Self::Parse => "invalid invocation directory .env file",
        })
    }
}

/// Load exactly `./.env`, without walking ancestors or consulting config/workspace.
/// Existing variables, including empty and non-Unicode values, take precedence.
///
/// # Safety
/// Must only be called during single-threaded process startup, before any thread
/// or runtime is started: mutating the process environment is otherwise unsafe.
pub(super) unsafe fn load_invocation_env() -> Result<(), LoadError> {
    let mut file = match File::open(".env") {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(LoadError::Read),
    };
    let mut source = String::new();
    file.read_to_string(&mut source)
        .map_err(|_| LoadError::Read)?;
    // The iterator does not strip a UTF-8 BOM, unlike dotenvy's load helpers.
    let source = source.strip_prefix('\u{feff}').unwrap_or(&source);
    for entry in dotenvy::from_read_iter(source.as_bytes()) {
        let (key, value) = entry.map_err(|_| LoadError::Parse)?;
        // set_var panics on NUL; reject it without exposing a secret in a panic.
        if key.contains('\0') || value.contains('\0') {
            return Err(LoadError::Parse);
        }
        if std::env::var_os(&key).is_none() {
            // SAFETY: guaranteed by this function's startup-only caller contract.
            unsafe { std::env::set_var(key, value) };
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::tests::{Fixture, mock_provider, output};
    use std::{
        ffi::OsString,
        fs,
        process::{Command, Output},
    };
    const KEY: &str = "SKYHOOK_DOTENV_TEST_KEY";
    const IGNORED: [&str; 4] = [
        ".env",
        "workspace/.env",
        "config/.env",
        "config/skyhook/.env",
    ];

    fn fixture() -> Fixture {
        let mut f = Fixture::new();
        for dir in ["invocation", "workspace", "home"] {
            fs::create_dir_all(f.path(dir)).unwrap();
        }
        f.cwd = f.path("invocation");
        f
    }

    fn dotenv(f: &Fixture, contents: &str) {
        fs::write(f.path("invocation/.env"), contents).unwrap();
    }

    fn shell(f: &Fixture, command: &str) -> Command {
        fs::write(f.path("invocation/run.js"), format!(
        "const result = await tool.exec({{argv: ['sh', '-c', {}]}}); if (result.exit_code !== 0) throw new Error('command failed'); return result;",
        serde_json::to_string(command).unwrap())).unwrap();
        let mut command = f.command();
        command.args(["--non-interactive", "--approve-all", "-s", "run.js"]);
        command
    }

    /// What a local command sees for `expansion`, optionally with an inherited value.
    fn observed(f: &Fixture, expansion: &str, inherited: Option<&OsString>) -> Vec<u8> {
        let mut command = shell(f, &format!("printf '%s' \"{expansion}\" > env.out"));
        if let Some(value) = inherited {
            command.env(KEY, value);
        }
        assert_headless_success(&output(&mut command));
        fs::read(f.path("env.out")).unwrap()
    }

    fn assert_headless_success(output: &Output) {
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        let stdout = std::str::from_utf8(&output.stdout).unwrap().trim();
        let id = stdout.parse::<skyhook::identity::SessionId>();
        id.expect("stdout must contain only the session ID");
    }

    fn assert_silent_failure(output: &Output) {
        assert!(!output.status.success());
        assert!(
            output.stdout.is_empty() && output.stderr.is_empty(),
            "{output:?}"
        );
    }

    #[test]
    fn invocation_dotenv_not_workspace_or_config_reaches_local_commands() {
        let f = fixture();
        dotenv(
            &f,
            &format!("\u{feff}export {KEY}=\"invocation-value\" # comment\n"),
        );
        for path in IGNORED {
            fs::write(f.path(path), format!("{KEY}=wrong-directory\n")).unwrap();
        }
        assert_eq!(observed(&f, &format!("${KEY}"), None), b"invocation-value");
    }

    #[test]
    fn inherited_values_win_including_empty_and_non_unicode_values() {
        use std::os::unix::ffi::OsStringExt;
        for inherited in [
            OsString::from("inherited-value"),
            OsString::from(""),
            OsString::from_vec(b"non-unicode-\xff".to_vec()),
        ] {
            let f = fixture();
            dotenv(&f, &format!("{KEY}=dotenv-value\n"));
            let seen = observed(&f, &format!("${KEY}"), Some(&inherited));
            assert_eq!(seen, inherited.into_vec());
        }
    }

    #[test]
    fn missing_dotenv_does_not_search_ancestors_workspace_or_config() {
        let f = fixture();
        for path in IGNORED {
            // Would abort startup if any of these files were read.
            fs::write(f.path(path), "BROKEN='unterminated-secret\n").unwrap();
        }
        assert_eq!(observed(&f, &format!("${{{KEY}-absent}}"), None), b"absent");
    }

    #[test]
    fn dotenv_is_loaded_before_default_config_and_auth_path_selection() {
        let f = fixture();
        // With inherited XDG_CONFIG_HOME absent, startup must use the dotenv value
        // rather than the empty HOME configuration directory.
        dotenv(
            &f,
            &format!("XDG_CONFIG_HOME='{}'\n", f.path("config").display()),
        );
        fs::write(f.path("invocation/run.js"), "return 'configured';").unwrap();
        let mut command = f.bare_command();
        command
            .env_remove("XDG_CONFIG_HOME")
            .arg("--workspace")
            .arg(f.path("workspace"));
        assert_headless_success(&output(command.args(["--non-interactive", "-s", "run.js"])));

        let credentials = f.path("config/skyhook/codex-oauth.json");
        fs::write(&credentials, "fixture credentials removed without parsing").unwrap();
        let mut command = f.bare_command();
        let out = output(
            command
                .env_remove("XDG_CONFIG_HOME")
                .args(["auth", "logout"]),
        );
        assert!(out.status.success() && out.stderr.is_empty(), "{out:?}");
        assert!(
            !credentials.exists(),
            "auth must use the dotenv config directory"
        );
    }

    #[test]
    fn malformed_dotenv_is_sanitized_silent_in_headless_and_ignored_for_help_version() {
        let f = fixture();
        for source in [
            "PRIVATE_TOKEN='secret-never-print\n",
            "PRIVATE_TOKEN=secret\0never-print\n",
        ] {
            dotenv(&f, source);
            let out = output(&mut f.command());
            assert!(!out.status.success() && out.stdout.is_empty());
            let stderr = String::from_utf8(out.stderr).unwrap();
            assert_eq!(stderr, "skyhook: invalid invocation directory .env file\n");
            assert_silent_failure(&output(f.command().args([
                "--non-interactive",
                "-p",
                "unused",
            ])));
            for flag in ["--help", "--version"] {
                let out = output(f.bare_command().args(["--non-interactive", flag]));
                assert!(out.status.success(), "{out:?}");
                assert!(!out.stdout.is_empty() && out.stderr.is_empty());
            }
        }
    }

    #[test]
    fn askpass_never_loads_dotenv_or_accepts_its_socket_override() {
        let f = fixture();
        // A missing inherited socket exits silently; a loaded socket would report an IO error.
        for source in [
            "SKYHOOK_ASKPASS_SOCKET=/dotenv-must-not-control-askpass\n",
            "PRIVATE_TOKEN='malformed-secret\n",
        ] {
            dotenv(&f, source);
            assert_silent_failure(&output(f.bare_command().args(["--askpass", "Password:"])));
        }
    }

    #[test]
    fn dotenv_credentials_reach_env_and_local_command_providers() {
        let env = format!("api_key_env='{KEY}'");
        let command = format!("api_key_command = \"printf '%s' \\\"${KEY}\\\"\"");
        for (credential, inherited, expected) in [
            (&env, None, "dotenv-api-key"),
            (&env, Some("inherited-api-key"), "inherited-api-key"),
            (&command, None, "dotenv-api-key"),
        ] {
            let f = fixture();
            let (endpoint, server) = mock_provider();
            f.provider_config(&endpoint, "", credential);
            dotenv(&f, &format!("{KEY}=dotenv-api-key\n"));
            let mut command = f.command();
            command.args(["--non-interactive", "-p", "hello"]);
            if let Some(key) = inherited {
                command.env(KEY, key);
            }
            let out = output(&mut command);
            let (headers, _) = server.join().unwrap();
            assert_headless_success(&out);
            let authorization = format!("authorization: bearer {expected}\r\n");
            assert!(headers.to_ascii_lowercase().contains(&authorization));
        }
    }
}
