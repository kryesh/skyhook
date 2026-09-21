//! Read-only CLI inspection, deliberately independent of harness/session startup.

use super::{cli::Inspection, launch};
use std::io::{self, Write};

/// Diagnostics can contain repository-controlled paths and parser messages.
/// Keep terminal controls visible as escapes rather than executing them.
pub(super) fn diagnostic_text(value: impl std::fmt::Display) -> String {
    let mut output = String::new();
    for character in value.to_string().chars() {
        if character.is_control() {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    output
}

pub(super) async fn run(request: Inspection) -> Result<(), Box<dyn std::error::Error>> {
    match request {
        Inspection::Config(request) => {
            let resolved = launch::resolve_config(&request).await?;
            for diagnostic in &resolved.report.diagnostics {
                eprintln!("skyhook config: {}", diagnostic_text(diagnostic));
            }
            for source in &resolved.report.sources {
                eprintln!(
                    "skyhook config: loaded {}",
                    diagnostic_text(source.display())
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
                eprintln!("skyhook skills: {}", diagnostic_text(error));
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
