use std::{
    collections::{BTreeSet, HashSet},
    path::{Path, PathBuf},
};

use super::{
    ROOT_TARGET, SshOptions, TargetConfig, TargetConfigType, TargetDefinition, TargetError,
    TargetSource,
};

pub(crate) async fn import_ssh_targets() -> Result<Vec<TargetDefinition>, TargetError> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Ok(Vec::new());
    };
    tokio::task::spawn_blocking(move || import_from(&home))
        .await
        .map_err(|error| TargetError::Import(error.to_string()))?
}

fn import_from(home: &Path) -> Result<Vec<TargetDefinition>, TargetError> {
    let ssh = home.join(".ssh");
    let mut aliases = BTreeSet::new();
    collect(
        &ssh.join("config"),
        &ssh,
        home,
        &mut HashSet::new(),
        &mut aliases,
    )?;
    aliases
        .into_iter()
        .filter(|name| name != ROOT_TARGET)
        .map(|name| {
            TargetDefinition::from_config(
                name.clone(),
                TargetConfig {
                    r#type: TargetConfigType::Ssh,
                    host: name,
                    ssh: SshOptions::default(),
                    workspace: PathBuf::from("."),
                    via: None,
                },
                TargetSource::SshConfig,
            )
        })
        .collect()
}

fn collect(
    path: &Path,
    ssh: &Path,
    home: &Path,
    visited: &mut HashSet<PathBuf>,
    aliases: &mut BTreeSet<String>,
) -> Result<(), TargetError> {
    let canonical = match std::fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(TargetError::Import(error.to_string())),
    };
    if !visited.insert(canonical.clone()) {
        return Ok(());
    }
    let text = std::fs::read_to_string(&canonical)
        .map_err(|error| TargetError::Import(error.to_string()))?;
    for line in text.lines() {
        let Some((keyword, arguments)) = directive(line) else {
            continue;
        };
        if keyword.eq_ignore_ascii_case("host") {
            aliases.extend(words(arguments).into_iter().filter(|value| concrete(value)));
        } else if keyword.eq_ignore_ascii_case("include") {
            for include in words(arguments) {
                let pattern = expand(&include, ssh, home);
                let pattern = pattern.to_string_lossy();
                for entry in
                    glob::glob(&pattern).map_err(|error| TargetError::Import(error.to_string()))?
                {
                    let entry = entry.map_err(|error| TargetError::Import(error.to_string()))?;
                    collect(&entry, ssh, home, visited, aliases)?;
                }
            }
        }
    }
    Ok(())
}

fn directive(line: &str) -> Option<(&str, &str)> {
    let line = line.trim_start();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let split = line
        .char_indices()
        .find(|(_, c)| c.is_whitespace() || *c == '=')
        .map_or(line.len(), |(i, _)| i);
    let keyword = &line[..split];
    let arguments = line[split..]
        .trim_start_matches(|c: char| c.is_whitespace() || c == '=')
        .trim_start();
    (!keyword.is_empty()).then_some((keyword, arguments))
}

fn words(input: &str) -> Vec<String> {
    let mut output = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in input.chars() {
        if escaped {
            word.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if quote.is_some_and(|delimiter| delimiter == character) {
            quote = None;
        } else if quote.is_some() {
            word.push(character);
        } else if matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if character == '#' {
            break;
        } else if character.is_whitespace() {
            if !word.is_empty() {
                output.push(std::mem::take(&mut word));
            }
        } else {
            word.push(character);
        }
    }
    if escaped {
        word.push('\\');
    }
    if !word.is_empty() {
        output.push(word);
    }
    output
}

fn concrete(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('!')
        && !value.chars().any(|c| matches!(c, '*' | '?' | '['))
}

fn expand(value: &str, ssh: &Path, home: &Path) -> PathBuf {
    if value == "~" || value == "%d" {
        return home.to_owned();
    }
    if let Some(relative) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("%d/"))
    {
        return home.join(relative);
    }
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_owned()
    } else {
        ssh.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_handles_quotes_comments_and_patterns() {
        assert_eq!(words("one \"two three\" # four"), ["one", "two three"]);
        assert!(concrete("server"));
        assert!(!concrete("*.example.com"));
        assert!(!concrete("!blocked"));
    }
}
