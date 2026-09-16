use crate::tests::Fixture;
use std::{fs, path::Path, process::Output};

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn successful_config(output: &Output) -> toml::Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = std::str::from_utf8(&output.stdout).unwrap();
    let _: skyhook::config::Config = toml::from_str(text).unwrap();
    toml::from_str(text).unwrap()
}

fn no_session(f: &Fixture) {
    assert!(!f.path(".skyhook/sessions").exists());
    assert!(!f.path("project/.skyhook/sessions").exists());
    assert!(!f.path(".skyhook/state.json").exists());
}

#[test]
fn dump_config_merges_selected_workspace_and_applies_cli_policy() {
    let f = Fixture::new();
    f.config(
        "http://127.0.0.1:1/v1",
        "approve_all=true\ncapabilities=['read']",
    );
    let user_path = f.path("config/skyhook/config.toml");
    let mut user = fs::read_to_string(&user_path).unwrap();
    user.push_str("\n[targets.build]\ntype='ssh'\nhost='user-build'\nworkspace='/user'\n[targets.build.ssh]\nuser='alice'\n[targets.monitor]\ntype='ssh'\nhost='monitor'\n");
    fs::write(&user_path, user).unwrap();
    write(&f.path(".skyhook/config.toml"), "invalid config [");
    write(
        &f.path("project/.skyhook/config.toml"),
        "[models.first]\nmax_output=8192\n[targets.build]\ntype='ssh'\nhost='project-build'\n",
    );
    let output = f
        .bare_command()
        .args(["--dump", "--workspace"])
        .arg(f.path("project"))
        .output()
        .unwrap();
    let config = successful_config(&output);
    assert_eq!(config["approve_all"].as_bool(), Some(true));
    assert_eq!(
        config["models"]["first"]["max_context"].as_integer(),
        Some(128000)
    );
    assert_eq!(
        config["models"]["first"]["max_output"].as_integer(),
        Some(8192)
    );
    assert_eq!(
        config["targets"]["build"]["host"].as_str(),
        Some("project-build")
    );
    // Normalized output contains the target default, not the inherited /user path.
    assert_eq!(config["targets"]["build"]["workspace"].as_str(), Some("."));
    assert!(
        config["targets"]["build"]
            .get("ssh")
            .and_then(|ssh| ssh.get("user"))
            .is_none()
    );
    assert_eq!(
        config["targets"]["monitor"]["host"].as_str(),
        Some("monitor")
    );
    let overridden = f
        .bare_command()
        .args(["--dump=config", "--workspace"])
        .arg(f.path("project"))
        .args(["--capabilities", "read,agents", "--approve-all"])
        .output()
        .unwrap();
    let config = successful_config(&overridden);
    assert_eq!(
        config["capabilities"].as_array().unwrap(),
        &[
            toml::Value::String("read".into()),
            toml::Value::String("agents".into())
        ]
    );
    assert_eq!(config["approve_all"].as_bool(), Some(true));
    no_session(&f);
}

#[test]
fn dump_config_reports_failed_xdg_and_uses_only_home_fallback() {
    let f = Fixture::new();
    let valid = fs::read_to_string(f.path("config/skyhook/config.toml")).unwrap();
    write(
        &f.path(".config/skyhook/config.toml"),
        &valid.replace("model='fixture'", "model='home-model'"),
    );
    write(&f.path("config/skyhook/config.toml"), "invalid = [");
    let output = f
        .bare_command()
        .args(["--dump", "config"])
        .output()
        .unwrap();
    let config = successful_config(&output);
    assert_eq!(
        config["models"]["first"]["model"].as_str(),
        Some("home-model")
    );
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(errors.contains(f.path("config/skyhook/config.toml").to_str().unwrap()));
    assert!(errors.contains(f.path(".config/skyhook/config.toml").to_str().unwrap()));
    no_session(&f);
}

#[test]
fn dump_config_rejects_target_cycles_in_candidates_explicit_files_and_merged_layers() {
    let f = Fixture::new();
    let a = "[targets.a]\ntype='ssh'\nhost='a'\nvia='b'\n";
    let b = "[targets.b]\ntype='ssh'\nhost='b'\nvia='a'\n";
    let xdg = f.path("config/skyhook/config.toml");
    let home = f.path(".config/skyhook/config.toml");
    write(&home, &fs::read_to_string(&xdg).unwrap());
    write(&xdg, &format!("{a}{b}"));
    let fallback = f.bare_command().arg("--dump=config").output().unwrap();
    let config = successful_config(&fallback);
    assert!(config["targets"].get("a").is_none());
    let diagnostic = String::from_utf8_lossy(&fallback.stderr);
    assert!(diagnostic.contains("target route contains a cycle"));
    assert!(diagnostic.contains(xdg.to_str().unwrap()));

    let explicit = f.command().arg("--dump=config").output().unwrap();
    assert!(!explicit.status.success());
    assert!(explicit.stdout.is_empty());
    assert!(String::from_utf8_lossy(&explicit.stderr).contains("target route contains a cycle"));

    write(&xdg, a);
    let workspace = f.path(".skyhook/config.toml");
    write(&workspace, b);
    let merged = f.bare_command().arg("--dump=config").output().unwrap();
    assert!(!merged.status.success());
    assert!(merged.stdout.is_empty());
    let diagnostic = String::from_utf8_lossy(&merged.stderr);
    assert!(diagnostic.contains("effective configuration failed"));
    assert!(diagnostic.contains("target route contains a cycle"));
    assert!(diagnostic.contains(workspace.to_str().unwrap()));
    no_session(&f);
}

#[cfg(unix)]
#[test]
fn dump_config_accepts_unresolved_routes_without_importing_or_running_ssh() {
    use std::os::unix::fs::PermissionsExt;

    let f = Fixture::new();
    let ssh = f.path("bin/ssh");
    write(&ssh, "#!/bin/sh\ntouch SSH_WAS_RUN\nexit 1\n");
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    let path = f.path("config/skyhook/config.toml");
    let mut config = fs::read_to_string(&path).unwrap();
    config.push_str("\n[targets.remote]\ntype='ssh'\nhost='remote.test'\nvia='defined-later'\n");
    write(&path, &config);
    let output = f
        .bare_command()
        .env("PATH", format!("{}:/usr/bin:/bin", f.path("bin").display()))
        .arg("--dump=config")
        .output()
        .unwrap();
    let config = successful_config(&output);
    assert_eq!(
        config["targets"]["remote"]["via"].as_str(),
        Some("defined-later")
    );
    assert!(!f.path("SSH_WAS_RUN").exists());
    no_session(&f);
}

#[test]
fn dump_explicit_config_does_not_probe_user_or_workspace_configs() {
    let f = Fixture::new();
    let valid = fs::read_to_string(f.path("config/skyhook/config.toml")).unwrap();
    write(&f.path("only.toml"), &valid);
    write(&f.path("config/skyhook/config.toml"), "invalid = [");
    write(&f.path(".skyhook/config.toml"), "invalid = [");
    let output = f
        .bare_command()
        .args(["--dump", "--config"])
        .arg(f.path("only.toml"))
        .arg("--workspace")
        .arg(f.path("does-not-exist"))
        .output()
        .unwrap();
    successful_config(&output);
    let diagnostics = String::from_utf8_lossy(&output.stderr);
    assert!(!diagnostics.contains("config/skyhook/config.toml"));
    assert!(!diagnostics.contains(".skyhook/config.toml"));
    let missing = f
        .bare_command()
        .args(["--dump", "--config"])
        .arg(f.path("missing.toml"))
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(missing.stdout.is_empty());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("missing.toml"));
    no_session(&f);
}

#[test]
fn dump_config_does_not_resolve_secrets_start_mcp_or_load_terminal_state() {
    let f = Fixture::new();
    f.provider_config("http://127.0.0.1:1/v1", "", "api_key_env='MISSING_API_KEY'");
    let path = f.path("config/skyhook/config.toml");
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str("\n[mcp.trap]\ntransport='stdio'\nstart_command=['/bin/sh','-c','touch SHOULD_NOT_EXIST']\n");
    fs::write(&path, text).unwrap();
    write(&f.path(".skyhook/state.json"), "malformed saved state");
    let output = f
        .bare_command()
        .args(["--dump", "config"])
        .output()
        .unwrap();
    let config = successful_config(&output);
    assert_eq!(
        config["providers"]["test"]["api_key_env"].as_str(),
        Some("MISSING_API_KEY")
    );
    assert!(!f.path("SHOULD_NOT_EXIST").exists());
    assert!(!f.path(".skyhook/sessions").exists());
    assert_eq!(
        fs::read_to_string(f.path(".skyhook/state.json")).unwrap(),
        "malformed saved state"
    );
}

#[test]
fn dump_config_fatal_errors_have_diagnostics_and_no_partial_config() {
    let f = Fixture::new();
    write(
        &f.path(".skyhook/config.toml"),
        "[models.first]\nmax_output=0\n",
    );
    let output = f.bare_command().arg("--dump").output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
    no_session(&f);
}

#[test]
fn dump_skills_shows_frontmatter_assets_and_errors_without_model_config() {
    let f = Fixture::new();
    fs::remove_file(f.path("config/skyhook/config.toml")).unwrap();
    write(
        &f.path(".agents/skills/release/SKILL.md"),
        "---\nname: declared-release\ndescription: Release helper\nmetadata:\n  owner: maintainers\n  checks: [tests, lint]\n---\n# Release\n",
    );
    write(
        &f.path(".agents/skills/release/references/checklist.md"),
        "Private asset body should not be dumped.",
    );
    write(
        &f.path(".agents/skills/release/scripts/run.sh"),
        "touch SHOULD_NOT_EXIST",
    );
    let output = f
        .bare_command()
        .args(["--dump", "skills"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "release",
        "declared-release",
        "Release helper",
        "owner",
        "maintainers",
        "checks",
        "checklist.md",
        "run.sh",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }
    assert!(text.contains('├') || text.contains('└'));
    assert!(!text.contains("Private asset body"));
    assert!(!f.path("SHOULD_NOT_EXIST").exists());
    write(
        &f.path(".agents/skills/broken/SKILL.md"),
        "---\ndescription: [unterminated\n---\n",
    );
    let errors = f
        .bare_command()
        .args(["--dump", "skills"])
        .output()
        .unwrap();
    assert!(!errors.status.success());
    assert!(String::from_utf8_lossy(&errors.stdout).contains("release"));
    assert!(String::from_utf8_lossy(&errors.stderr).contains("broken"));
    no_session(&f);
}

#[test]
fn malformed_dotenv_and_invalid_dump_combinations_report_errors() {
    let f = Fixture::new();
    write(&f.path(".env"), "PRIVATE_KEY='unterminated-secret");
    let output = f.bare_command().arg("--dump").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(".env"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("unterminated-secret"));
    let output = f
        .bare_command()
        .args(["--dump", "--non-interactive"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!output.stderr.is_empty());
    no_session(&f);
}

#[test]
fn dump_workspace_errors_keep_user_candidate_diagnostics() {
    let f = Fixture::new();
    write(&f.path("config/skyhook/config.toml"), "invalid = [");
    let output = f
        .bare_command()
        .args(["--dump", "--workspace"])
        .arg(f.path("missing-workspace"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let errors = String::from_utf8_lossy(&output.stderr);
    for path in [
        "config/skyhook/config.toml",
        ".config/skyhook/config.toml",
        "missing-workspace",
    ] {
        assert!(errors.contains(path), "missing {path}: {errors}");
    }
    no_session(&f);
}

#[cfg(unix)]
#[test]
fn dump_diagnostics_escape_repository_controlled_terminal_sequences() {
    let f = Fixture::new();
    write(
        &f.path(".agents/skills/evil\u{1b}[2J/SKILL.md"),
        "# Skill\nDescription.\n",
    );
    let output = f
        .bare_command()
        .args(["--dump", "skills"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!output.stderr.contains(&0x1b));
    assert!(String::from_utf8_lossy(&output.stderr).contains("evil\\u{1b}[2J"));

    let path = f.path("odd\u{1b}[2J.toml");
    let config = fs::read_to_string(f.path("config/skyhook/config.toml")).unwrap();
    write(&path, &config);
    let output = f
        .bare_command()
        .args(["--dump", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    successful_config(&output);
    assert!(!output.stderr.contains(&0x1b));
    assert!(String::from_utf8_lossy(&output.stderr).contains("odd\\u{1b}[2J.toml"));
    fs::remove_file(&path).unwrap();
    let output = f
        .bare_command()
        .args(["--dump", "--config"])
        .arg(path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!output.stderr.contains(&0x1b));
    no_session(&f);
}
