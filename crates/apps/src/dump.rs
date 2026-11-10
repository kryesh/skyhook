//! Read-only inspection, deliberately independent of harness/session startup.

use crate::args::{ConfigArgs, ProductArgs};
use skyhook::product::{Attendance, Error, Opening, Product, SessionContext};
use skyhook_tools::diagnostic::escape_controls;
use std::{io::Write, path::PathBuf};

pub enum DumpRequest<A> {
    /// The configuration a new session started with these options would run with. The
    /// selected workspace is used as given, so an explicit file can be inspected even when
    /// the workspace does not exist.
    Config(ConfigArgs, A),
    /// The workspace's skills; their discovery only needs the workspace.
    Skills(PathBuf),
}

pub async fn run<P: Product>(
    product: &P,
    request: DumpRequest<impl ProductArgs<P::Options>>,
    output: &mut impl Write,
) -> Result<(), Error> {
    let identity = product.identity();
    let name = identity.name();
    match request {
        DumpRequest::Config(request, args) => {
            let workspace = &request.source.workspace;
            let (mut resolved, options) = crate::config::load(&request, args, workspace).await?;
            let context = SessionContext {
                workspace,
                opening: Opening::New(None),
                attendance: Attendance::Attended,
            };
            let config = &mut resolved.config.config;
            let configured = product.configure(config, options, context).await?;
            let diagnostics = resolved.report.diagnostics.iter().map(ToString::to_string);
            for warning in diagnostics.chain(configured.warnings) {
                eprintln!("{name} config: {}", escape_controls(warning));
            }
            for source in &resolved.report.sources {
                eprintln!(
                    "{name} config: loaded {}",
                    escape_controls(source.display())
                );
            }
            resolved.config.config.clone().into_runtime()?;
            let text = resolved.config.to_yaml()?;
            output.write_all(text.as_bytes())?;
        }
        DumpRequest::Skills(request) => {
            let workspace = tokio::fs::canonicalize(&request)
                .await
                .map_err(|error| format!("workspace {}: {error}", request.display()))?;
            if !workspace.is_dir() {
                return Err("workspace must be a directory".into());
            }
            let (text, errors) = skyhook::tool::builtins::HostSkills::discover(&workspace)
                .await
                .describe()
                .await;
            for error in &errors {
                eprintln!("{name} skills: {}", escape_controls(error));
            }
            output.write_all(text.as_bytes())?;
            if !errors.is_empty() {
                return Err(
                    format!("skill inspection encountered {} error(s)", errors.len()).into(),
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{NoArgs, SourceArgs};
    use skyhook::product::Skyhook;

    /// The dump shows the configuration with the command line's policy, without a resolvable
    /// workspace, a session, a provider, a credential, an MCP server or a target's
    /// connection.
    #[tokio::test]
    async fn dump_config_shows_the_policy_and_starts_nothing() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.yaml");
        let ran = |name: &str| format!("touch {}", root.path().join(name).display());
        let quoted = |command: String| serde_json::to_string(&command).unwrap();
        std::fs::write(
            &config,
            format!(
                "providers:\n  test:\n    dialect: compatible\n    codec: chat_completions\n    base_url: http://127.0.0.1:1\n    api_key: {{command: {}}}\n    models:\n      first:\n        model: fixture\n        max_context: 128000\n        max_output: 4096\nmcp:\n  trap:\n    transport: stdio\n    start_command: [/bin/sh, -c, {}]\ntargets:\n  remote:\n    type: ssh\n    host: remote.test\n    via: defined-later\n",
                quoted(format!("{}; printf key", ran("CREDENTIAL_RAN"))),
                quoted(ran("MCP_RAN")),
            ),
        )
        .unwrap();
        let workspace = root.path().join("absent");
        let config = ConfigArgs {
            source: SourceArgs {
                workspace: workspace.clone(),
                config: Some(config),
            },
            approve_all: true,
        };
        let mut output = Vec::new();
        let request = DumpRequest::Config(config, NoArgs {});
        run(&Skyhook, request, &mut output).await.unwrap();
        let dumped = skyhook::yaml::parse::<skyhook::launch::config::HostConfig>(
            std::str::from_utf8(&output).unwrap(),
        )
        .unwrap();
        assert!(dumped.approve_all);
        assert!(
            std::str::from_utf8(&output)
                .unwrap()
                .contains("via: defined-later")
        );
        assert!(!workspace.exists());
        for path in ["CREDENTIAL_RAN", "MCP_RAN", ".skyhook"] {
            assert!(
                !root.path().join(path).exists(),
                "inspection created {path}"
            );
        }
    }

    /// A skill that cannot be read fails the dump, which still lists the others.
    #[tokio::test]
    async fn dump_skills_lists_the_readable_skills_and_fails_on_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let skills = root.path().join(".agents/skills");
        for (name, text) in [
            ("release", "---\ndescription: Release helper\n---\n"),
            ("broken", "---\ndescription: [unterminated\n---\n"),
        ] {
            std::fs::create_dir_all(skills.join(name)).unwrap();
            std::fs::write(skills.join(name).join("SKILL.md"), text).unwrap();
        }
        let request = DumpRequest::<NoArgs>::Skills(root.path().to_owned());
        let mut output = Vec::new();
        assert!(run(&Skyhook, request, &mut output).await.is_err());
        let listed = String::from_utf8(output).unwrap();
        assert!(listed.contains("Release helper") && !listed.contains("broken"));
    }
}
