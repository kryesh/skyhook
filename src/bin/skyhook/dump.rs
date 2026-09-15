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
            let output = resolved.config.to_toml()?;
            io::stdout().lock().write_all(output.as_bytes())?;
        }
        Inspection::Skills(request) => {
            let workspace = tokio::fs::canonicalize(&request)
                .await
                .map_err(|error| format!("workspace {}: {error}", request.display()))?;
            if !workspace.is_dir() {
                return Err("workspace must be a directory".into());
            }
            let inventory = skyhook::tool::builtins::HostSkills::discover(&workspace)
                .await
                .inventory()
                .await;
            for diagnostic in &inventory.diagnostics {
                eprintln!("skyhook skills: {}", diagnostic_text(diagnostic));
            }
            io::stdout()
                .lock()
                .write_all(inventory.render_tree().as_bytes())?;
            if inventory.has_errors() {
                return Err(format!(
                    "skill inspection encountered {} error(s)",
                    inventory.diagnostics.len()
                )
                .into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
