//! Run the tool executor in a separate session with a real controlling PTY.
//! Never change the test runner's own session or terminal state.

use std::{
    io::Read as _,
    os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd},
};

use super::*;
use crate::{test_support::TestRuntime, tool::policy::CapabilitySet};

const PTY_HELPER: &str = "SKYHOOK_PROCESS_PTY_TEST";

#[tokio::test]
async fn capability_controls_terminal_access_under_a_pty() {
    let mut master = -1;
    let mut slave = -1;
    // SAFETY: openpty initializes the two descriptor slots; optional arguments
    // are null. OwnedFd below takes sole ownership on success.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0
    );
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    for fd in [&master, &slave] {
        // SAFETY: these descriptors remain owned and valid throughout setup.
        assert_ne!(
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
            -1
        );
    }
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "tool::builtins::process::terminal_tests::pty_executor_helper",
            "--ignored",
            "--nocapture",
        ])
        .env(PTY_HELPER, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let slave_fd = slave.as_raw_fd();
    // SAFETY: only async-signal-safe syscalls are used in the child. The slave
    // stays open until spawn finishes and is then closed on exec via CLOEXEC.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 || libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().unwrap();
    let _group = ProcessGroup(i32::try_from(child.id().unwrap()).unwrap());
    let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .expect("PTY executor helper hung")
        .unwrap();
    assert!(
        output.status.success(),
        "PTY helper failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "PTY helper filter did not execute the regression"
    );

    // Keep the slave open so an empty master returns EAGAIN rather than EIO.
    // No tool should have emitted its attempted credential prompt to the PTY.
    assert_ne!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
        -1
    );
    let mut master = std::fs::File::from(master);
    let mut terminal_output = Vec::new();
    match master.read_to_end(&mut terminal_output) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(error) => panic!("reading PTY output: {error}"),
    }
    assert!(
        terminal_output.is_empty(),
        "unexpected terminal prompt: {:?}",
        String::from_utf8_lossy(&terminal_output)
    );
}

#[test]
#[ignore = "invoked in a dedicated PTY/session by capability_controls_terminal_access_under_a_pty"]
fn pty_executor_helper() {
    if std::env::var_os(PTY_HELPER).is_none() {
        return;
    }
    // Establish that this is a real controlling terminal, not merely a tty FD.
    let _tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .expect("helper must have a controlling terminal");
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        for interactive in [false, true] {
            let runtime = TestRuntime::new().await;
            let mut builder = ToolRegistryBuilder::default();
            register(&mut builder).unwrap();
            let mut capabilities = CapabilitySet::default();
            if !interactive {
                capabilities.remove(Capability::Interactive);
            }
            // The gate must not depend on Targets being enabled.
            capabilities.remove(Capability::Targets);
            let executor = runtime.executor(builder).with_capabilities(capabilities);
            let (command, expected) = if interactive {
                ("if (: <> /dev/tty) 2>/dev/null; then printf attached; else printf missing; fi", "attached")
            } else {
                // With only setpgid and piped stdio this emits a terminal prompt
                // then stops on SIGTTIN. The tool timeout makes that regression
                // fail promptly instead of hanging the test suite.
                ("if (: <> /dev/tty) 2>/dev/null; then exec 3<> /dev/tty; printf 'Password: ' >&3; read answer <&3; else printf isolated; fi", "isolated")
            };
            for tool in ["exec", "shell"] {
                let args = if tool == "exec" {
                    serde_json::json!({"argv":["/bin/sh", "-c", command], "timeout":2})
                } else {
                    serde_json::json!({"command":command, "timeout":2})
                };
                let output = executor.execute(runtime.agent.clone(), tool, args, None).await
                    .unwrap_or_else(|error| panic!("{tool} interactive={interactive}: {error}"));
                assert_eq!(output.output.value["exit_code"], 0);
                assert_eq!(output.output.value["stdout"], expected);
            }
        }
    });
}

#[tokio::test]
async fn timeout_kills_noninteractive_descendants_after_shell_exit() {
    let runtime = TestRuntime::new().await;
    let mut builder = ToolRegistryBuilder::default();
    register(&mut builder).unwrap();
    let mut capabilities = CapabilitySet::default();
    capabilities.remove(Capability::Interactive);
    let executor = runtime.executor(builder).with_capabilities(capabilities);
    let error = executor.execute(runtime.agent.clone(), "shell", serde_json::json!({
        "command":"(sleep 1.5; printf escaped > escaped) & printf ready; exit 0", "timeout":1
    }), None).await.expect_err("descendant-held pipes must time out");
    assert!(error.to_string().contains("timed out"), "{error}");
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(!runtime.root.path().join("escaped").exists());
}

#[tokio::test]
async fn noninteractive_askpass_overrides_provider_without_targets() {
    for interactive in [false, true] {
        let runtime = TestRuntime::new().await;
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder).unwrap();
        let mut capabilities = CapabilitySet::default();
        capabilities.remove(Capability::Targets);
        if !interactive {
            capabilities.remove(Capability::Interactive);
        }
        let environment = std::collections::BTreeMap::from([
            ("SSH_ASKPASS".into(), "/inherited/prompt-helper".into()),
            ("SSH_ASKPASS_REQUIRE".into(), "prefer".into()),
            ("DISPLAY".into(), "inherited-display".into()),
            (
                "SKYHOOK_ASKPASS_SOCKET".into(),
                "/inherited/prompt-socket".into(),
            ),
            ("SSH_AUTH_SOCK".into(), "/inherited/agent-socket".into()),
        ]);
        let executor = runtime
            .executor(builder)
            .with_capabilities(capabilities)
            .with_process_environment(environment);
        for tool in ["exec", "shell"] {
            let command = r#"printf '%s\n' "$SSH_ASKPASS" "$SSH_ASKPASS_REQUIRE" "$DISPLAY" "$SKYHOOK_ASKPASS_SOCKET" "$SSH_AUTH_SOCK"; if test -x "$SSH_ASKPASS" && test -S "$SKYHOOK_ASKPASS_SOCKET"; then printf live; fi"#;
            let args = if tool == "exec" {
                serde_json::json!({"argv":["/bin/sh", "-c", command]})
            } else {
                serde_json::json!({"command":command})
            };
            let output = executor
                .execute(runtime.agent.clone(), tool, args, None)
                .await
                .unwrap();
            assert_eq!(output.output.value["exit_code"], 0);
            let lines: Vec<_> = output.output.value["stdout"]
                .as_str()
                .unwrap()
                .lines()
                .collect();
            assert_eq!(
                lines[4], "/inherited/agent-socket",
                "nonprompting credentials remain usable"
            );
            if interactive {
                assert_eq!(
                    lines,
                    [
                        "/inherited/prompt-helper",
                        "prefer",
                        "inherited-display",
                        "/inherited/prompt-socket",
                        "/inherited/agent-socket"
                    ]
                );
            } else {
                assert_ne!(lines[0], "/inherited/prompt-helper");
                assert_eq!(lines[1], "force");
                assert_eq!(lines[2], "skyhook");
                assert_ne!(lines[3], "/inherited/prompt-socket");
                assert_eq!(
                    lines[5], "live",
                    "rejecting broker must live throughout command"
                );
                assert!(
                    !std::path::Path::new(lines[0]).exists(),
                    "helper must be cleaned up"
                );
                assert!(
                    !std::path::Path::new(lines[3]).exists(),
                    "socket must be cleaned up"
                );
            }
        }
    }
}
