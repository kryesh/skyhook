//! Read-only CLI inspection, deliberately independent of harness/session startup.

use super::{Args, DumpKind, launch};
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

pub(super) fn validate_options(args: &Args) -> Result<(), clap::Error> {
    let Some(kind) = args.dump else {
        return Ok(());
    };
    let conflict = if args.command.is_some() {
        Some("--dump cannot be combined with an auth command")
    } else if kind == DumpKind::Skills
        && (args.config.is_some() || args.capabilities.is_some() || args.approve_all)
    {
        Some("--dump skills uses skill discovery, not --config, --capabilities, or --approve-all")
    } else {
        None
    };
    if let Some(message) = conflict {
        return Err(clap::Error::raw(
            clap::error::ErrorKind::ArgumentConflict,
            message,
        ));
    }
    Ok(())
}

pub(super) async fn run(args: &Args, kind: DumpKind) -> Result<(), Box<dyn std::error::Error>> {
    match kind {
        DumpKind::Config => {
            let resolved = launch::resolve_config(args).await?;
            for diagnostic in &resolved.report.diagnostics {
                eprintln!("skyhook config: {}", diagnostic_text(diagnostic));
            }
            for source in &resolved.report.sources {
                eprintln!(
                    "skyhook config: loaded {}",
                    diagnostic_text(source.display())
                );
            }
            launch::validate_config(&resolved.config)?;
            let output = resolved.config.to_toml()?;
            io::stdout().lock().write_all(output.as_bytes())?;
        }
        DumpKind::Skills => {
            let workspace = tokio::fs::canonicalize(&args.workspace)
                .await
                .map_err(|error| format!("workspace {}: {error}", args.workspace.display()))?;
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
