//! Read-only CLI inspection, deliberately independent of harness/session startup.

use super::{cli::Inspection, launch};
use skyhook::tool::diagnostic::escape_controls;
use std::io::{self, Write};

pub(super) async fn run(request: Inspection) -> Result<(), Box<dyn std::error::Error>> {
    match request {
        Inspection::Config(request) => {
            let resolved = launch::resolve_config(&request).await?;
            for diagnostic in &resolved.report.diagnostics {
                eprintln!("skyhook config: {}", escape_controls(diagnostic));
            }
            for source in &resolved.report.sources {
                eprintln!(
                    "skyhook config: loaded {}",
                    escape_controls(source.display())
                );
            }
            resolved.config.clone().into_runtime()?;
            let output = resolved.config.to_yaml()?;
            io::stdout().lock().write_all(output.as_bytes())?;
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
                eprintln!("skyhook skills: {}", escape_controls(error));
            }
            io::stdout().lock().write_all(text.as_bytes())?;
            if !errors.is_empty() {
                return Err(
                    format!("skill inspection encountered {} error(s)", errors.len()).into(),
                );
            }
        }
    }
    Ok(())
}
