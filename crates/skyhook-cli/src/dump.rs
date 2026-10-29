//! Read-only CLI inspection, deliberately independent of harness/session startup.

use super::{
    Error, Opening, Product, SessionContext,
    args::{ConfigRequest, Inspection},
    launch,
};
use skyhook::tool::diagnostic::escape_controls;
use std::io::Write;

pub(super) async fn run<P: Product>(
    product: &P,
    request: Inspection<P::Args>,
    output: &mut impl Write,
) -> Result<(), Error> {
    let identity = product.identity();
    let name = identity.name();
    match request {
        Inspection::Config(request, args) => {
            let resolved = effective_config(product, &request, args).await?;
            for diagnostic in &resolved.report.diagnostics {
                eprintln!("{name} config: {}", escape_controls(diagnostic));
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
        Inspection::Skills(request) => {
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

/// The configuration a new session started with these arguments would run with. The
/// selected workspace is used as given, so an explicit file can be inspected even when
/// the workspace does not exist.
async fn effective_config<P: Product>(
    product: &P,
    request: &ConfigRequest,
    args: P::Args,
) -> Result<skyhook::config::ResolvedConfig<super::config::HostConfig>, Error> {
    let context = SessionContext {
        workspace: &request.source.workspace,
        opening: Opening::New(None),
        headless: false,
    };
    let (resolved, _factory) = launch::configure_product(product, args, request, context).await?;
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        NoArgs,
        args::{Invocation, tests::parse_from},
        tests::Configuring,
    };

    /// The dump shows the configuration after the product's edit, through the shared
    /// renderer, without a resolvable workspace, a session, or a provider.
    #[tokio::test]
    async fn dump_config_applies_product_edits_without_starting_anything() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.yaml");
        let credential = format!(
            "touch {}; printf key",
            root.path().join("CREDENTIAL_RAN").display()
        );
        std::fs::write(
            &config,
            format!(
                "providers:\n  test:\n    dialect: compatible\n    codec: chat_completions\n    base_url: http://127.0.0.1:1\n    api_key: {{command: {}}}\n    models:\n      first:\n        model: fixture\n        max_context: 128000\n        max_output: 4096\n",
                serde_json::to_string(&credential).unwrap()
            ),
        )
        .unwrap();
        let workspace = root.path().join("absent");
        let Invocation::Inspect(request) = parse_from([
            "skyhook",
            "dump",
            "--workspace",
            workspace.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
        ])
        .unwrap() else {
            panic!("inspection")
        };
        let Inspection::Config(config_request, _) = &request else {
            panic!("config")
        };
        let resolved = effective_config(&Configuring, config_request, NoArgs {})
            .await
            .unwrap();
        assert_eq!(resolved.report.diagnostics.len(), 1);
        assert!(!escape_controls(&resolved.report.diagnostics[0]).contains('\u{1b}'));
        let mut output = Vec::new();
        run(&Configuring, request, &mut output).await.unwrap();
        let dumped =
            skyhook::yaml::parse::<super::super::HostConfig>(std::str::from_utf8(&output).unwrap())
                .unwrap();
        assert!(!dumped.approve_all);
        assert_eq!(dumped.config.default_mode.as_str(), "inspect");
        assert!(!workspace.exists());
        for path in ["CREDENTIAL_RAN", ".skyhook"] {
            assert!(
                !root.path().join(path).exists(),
                "inspection created {path}"
            );
        }
    }
}
