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
        fs,
        process::{Command, Output},
    };
    const KEY: &str = "SKYHOOK_DOTENV_TEST_KEY";
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
    fn assert_headless_success(output: &Output) {
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        std::str::from_utf8(&output.stdout)
            .unwrap()
            .trim()
            .parse::<skyhook::identity::SessionId>()
            .expect("stdout must contain only the session ID");
    }

    #[test]
    fn invocation_dotenv_not_workspace_or_config_reaches_local_commands() {
        let f = fixture();
        dotenv(
            &f,
            &format!("\u{feff}export {KEY}=\"invocation-value\" # comment\n"),
        );
        for path in [
            ".env",
            "workspace/.env",
            "config/.env",
            "config/skyhook/.env",
        ] {
            fs::write(f.path(path), format!("{KEY}=wrong-directory\n")).unwrap();
        }
        let out = output(&mut shell(&f, &format!("printf '%s' \"${KEY}\" > env.out")));
        assert_headless_success(&out);
        assert_eq!(
            fs::read_to_string(f.path("env.out")).unwrap(),
            "invocation-value"
        );
    }

    #[test]
    fn inherited_values_win_including_empty_and_non_unicode_values() {
        use std::os::unix::ffi::OsStringExt;
        for inherited in [
            std::ffi::OsString::from("inherited-value"),
            std::ffi::OsString::from(""),
            std::ffi::OsString::from_vec(b"non-unicode-\xff".to_vec()),
        ] {
            let f = fixture();
            dotenv(&f, &format!("{KEY}=dotenv-value\n"));
            let mut command = shell(&f, &format!("printf '%s' \"${KEY}\" > env.out"));
            command.env(KEY, &inherited);
            let out = output(&mut command);
            assert_headless_success(&out);
            assert_eq!(fs::read(f.path("env.out")).unwrap(), inherited.into_vec());
        }
    }

    #[test]
    fn missing_dotenv_does_not_search_ancestors_workspace_or_config() {
        let f = fixture();
        for path in [
            ".env",
            "workspace/.env",
            "config/.env",
            "config/skyhook/.env",
        ] {
            // Would abort startup if any of these files were read.
            fs::write(f.path(path), "BROKEN='unterminated-secret\n").unwrap();
        }
        let out = output(&mut shell(
            &f,
            &format!("printf '%s' \"${{{KEY}-absent}}\" > env.out"),
        ));
        assert_headless_success(&out);
        assert_eq!(fs::read_to_string(f.path("env.out")).unwrap(), "absent");
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
            .arg(f.path("workspace"))
            .args(["--non-interactive", "-s", "run.js"]);
        assert_headless_success(&output(&mut command));

        let credentials = f.path("config/skyhook/codex-oauth.json");
        fs::write(&credentials, "fixture credentials removed without parsing").unwrap();
        let mut command = f.bare_command();
        command
            .env_remove("XDG_CONFIG_HOME")
            .args(["auth", "logout"]);
        let out = output(&mut command);
        assert!(out.status.success(), "{out:?}");
        assert!(out.stderr.is_empty());
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
            assert!(!out.status.success());
            assert!(out.stdout.is_empty());
            assert_eq!(
                String::from_utf8(out.stderr).unwrap(),
                "skyhook: invalid invocation directory .env file\n"
            );
            let out = output(f.command().args(["--non-interactive", "-p", "unused"]));
            assert!(!out.status.success());
            assert!(out.stdout.is_empty());
            assert!(out.stderr.is_empty());
            for flag in ["--help", "--version"] {
                let out = output(f.bare_command().args(["--non-interactive", flag]));
                assert!(out.status.success(), "{out:?}");
                assert!(!out.stdout.is_empty());
                assert!(out.stderr.is_empty());
            }
        }
    }

    #[test]
    fn askpass_never_loads_dotenv_or_accepts_its_socket_override() {
        let f = fixture();
        dotenv(
            &f,
            "SKYHOOK_ASKPASS_SOCKET=/dotenv-must-not-control-askpass\n",
        );
        let out = output(f.bare_command().args(["--askpass", "Password:"]));
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
        // Missing inherited socket exits silently; a loaded socket would report an IO error.
        assert!(out.stderr.is_empty());
        dotenv(&f, "PRIVATE_TOKEN='malformed-secret\n");
        let out = output(f.bare_command().args(["--askpass", "Password:"]));
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
        assert!(out.stderr.is_empty());
    }

    #[test]
    fn dotenv_credentials_reach_env_and_local_command_providers() {
        for (credential, inherited, expected) in [
            (format!("api_key_env='{KEY}'"), None, "dotenv-api-key"),
            (
                format!("api_key_env='{KEY}'"),
                Some("inherited-api-key"),
                "inherited-api-key",
            ),
            (
                format!("api_key_command = \"printf '%s' \\\"${KEY}\\\"\""),
                None,
                "dotenv-api-key",
            ),
        ] {
            let f = fixture();
            let (endpoint, server) = mock_provider();
            f.provider_config(&endpoint, "", &credential);
            dotenv(&f, &format!("{KEY}=dotenv-api-key\n"));
            let mut command = f.command();
            command.args(["--non-interactive", "-p", "hello"]);
            if let Some(key) = inherited {
                command.env(KEY, key);
            }
            let out = output(&mut command);
            let (headers, _) = server.join().unwrap();
            assert_headless_success(&out);
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains(&format!("authorization: bearer {expected}\r\n"))
            );
        }
    }
}
