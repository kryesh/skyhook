//! Read-only inventory of the already-discovered runtime winners.

use std::{collections::BTreeMap, path::PathBuf};

use tokio::fs;

use super::HostSkills;

/// Winning host skills and non-fatal, path-qualified discovery/asset errors.
#[derive(Clone, Debug, Default)]
pub struct SkillInventory {
    pub skills: Vec<SkillInventoryEntry>,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct SkillInventoryEntry {
    /// Effective runtime name, derived from the directory, not frontmatter.
    pub name: String,
    /// Canonical skill directory containing SKILL.md.
    pub source: PathBuf,
    pub description: String,
    /// Complete parsed YAML, or Null if there is no frontmatter.
    pub frontmatter: serde_yaml::Value,
    /// Recursive asset listing in sorted depth-first order. Paths are relative
    /// to source. Root SKILL.md is metadata/instructions, not an asset.
    pub assets: Vec<SkillAsset>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillAsset {
    pub path: PathBuf,
    pub kind: SkillAssetKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillAssetKind {
    Directory,
    File,
    /// Links are marked but never followed, including broken links.
    Symlink,
    Special,
}

pub(super) async fn collect(skills: &HostSkills) -> SkillInventory {
    let mut inventory = SkillInventory {
        skills: Vec::with_capacity(skills.entries.len()),
        diagnostics: skills.warnings().to_vec(),
    };
    for entry in skills.entries.values() {
        let mut assets = Vec::new();
        // Iterative traversal avoids recursion and never opens an asset file.
        let mut pending = vec![PathBuf::new()];
        while let Some(relative) = pending.pop() {
            let path = entry.root.join(&relative);
            let metadata = match fs::symlink_metadata(&path).await {
                Ok(metadata) => metadata,
                Err(error) => {
                    inventory.diagnostics.push(format!(
                        "Cannot inspect assets for skill `{}` at {}: {error}",
                        entry.name,
                        path.display()
                    ));
                    continue;
                }
            };
            let kind = if metadata.file_type().is_symlink() {
                SkillAssetKind::Symlink
            } else if metadata.is_dir() {
                SkillAssetKind::Directory
            } else if metadata.is_file() {
                SkillAssetKind::File
            } else {
                SkillAssetKind::Special
            };
            if !relative.as_os_str().is_empty() {
                assets.push(SkillAsset {
                    path: relative.clone(),
                    kind,
                });
            } else if kind != SkillAssetKind::Directory {
                // The canonical skill root may have changed after discovery.
                inventory.diagnostics.push(format!(
                    "Cannot list assets for skill `{}` at {}: skill root is no longer a directory (symlinks are not followed)",
                    entry.name,
                    path.display()
                ));
            }
            if kind != SkillAssetKind::Directory {
                continue;
            }
            let mut directory = match fs::read_dir(&path).await {
                Ok(directory) => directory,
                Err(error) => {
                    inventory.diagnostics.push(format!(
                        "Cannot list assets for skill `{}` at {}: {error}",
                        entry.name,
                        path.display()
                    ));
                    continue;
                }
            };
            let mut children = Vec::new();
            loop {
                match directory.next_entry().await {
                    Ok(Some(child)) => {
                        if relative.as_os_str().is_empty() && child.file_name() == "SKILL.md" {
                            continue;
                        }
                        children.push(relative.join(child.file_name()));
                    }
                    Ok(None) => break,
                    Err(error) => {
                        inventory.diagnostics.push(format!(
                            "Cannot list assets for skill `{}` at {}: {error}",
                            entry.name,
                            path.display()
                        ));
                        break;
                    }
                }
            }
            children.sort();
            pending.extend(children.into_iter().rev());
        }
        inventory.skills.push(SkillInventoryEntry {
            name: entry.name.clone(),
            source: entry.root.clone(),
            description: entry.description.clone(),
            frontmatter: entry.frontmatter.clone(),
            assets,
        });
    }
    inventory
}

impl SkillInventory {
    pub fn has_errors(&self) -> bool {
        !self.diagnostics.is_empty()
    }

    /// Render all valid winners without truncation. Diagnostics are separate so
    /// callers can choose stderr and their exit status independently.
    pub fn render_tree(&self) -> String {
        let mut rows = Vec::new();
        for skill in &self.skills {
            rows.push((0, escape(&skill.name)));
            rows.push((
                1,
                format!("source: {}", escape(&skill.source.to_string_lossy())),
            ));
            rows.push((1, format!("description: {}", escape(&skill.description))));
            rows.push((1, "frontmatter".to_owned()));
            // YAML preserves scalar types, tags, non-string mapping keys and
            // nested collections which a conversion to JSON could lose.
            let yaml = serde_yaml::to_string(&skill.frontmatter)
                .unwrap_or_else(|error| format!("[cannot render YAML: {error}]"));
            rows.extend(yaml.lines().map(|line| (2, escape(line))));
            rows.push((
                1,
                if skill.assets.is_empty() {
                    "assets (empty)"
                } else {
                    "assets"
                }
                .to_owned(),
            ));
            for asset in &skill.assets {
                let name = asset.path.file_name().unwrap_or_default().to_string_lossy();
                let suffix = match asset.kind {
                    SkillAssetKind::Directory => "/",
                    SkillAssetKind::File => "",
                    SkillAssetKind::Symlink => " [symlink]",
                    SkillAssetKind::Special => " [special]",
                };
                rows.push((
                    asset.path.components().count() + 1,
                    format!("{}{suffix}", escape(&name)),
                ));
            }
        }
        if rows.is_empty() {
            return "skills (empty)\n".to_owned();
        }
        // Locate each row's next sibling in linear time, then render with an
        // explicit ancestor stack (asset depth is not limited by call stack).
        let mut next = Vec::new();
        let mut last = vec![false; rows.len()];
        for (index, (depth, _)) in rows.iter().enumerate().rev() {
            while next.last().is_some_and(|next_depth| next_depth > depth) {
                next.pop();
            }
            last[index] = next.last() != Some(depth);
            next.push(*depth);
        }
        let mut output = "skills\n".to_owned();
        let mut ancestors = BTreeMap::new();
        for (index, (depth, label)) in rows.iter().enumerate() {
            for level in 0..*depth {
                output.push_str(if ancestors.get(&level) == Some(&true) {
                    "    "
                } else {
                    "│   "
                });
            }
            output.push_str(if last[index] {
                "└── "
            } else {
                "├── "
            });
            output.push_str(label);
            output.push('\n');
            ancestors.insert(*depth, last[index]);
        }
        output
    }
}

fn escape(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| {
            if character.is_control() {
                character.escape_default().collect::<Vec<_>>()
            } else {
                vec![character]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::{Frontmatter, parse_instructions};
    use super::*;
    use std::path::Path;

    fn write_skill(root: &Path, name: &str, text: &str) -> PathBuf {
        let path = root.join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("SKILL.md"), text).unwrap();
        path
    }

    #[tokio::test]
    async fn inventory_retains_full_metadata_and_runtime_names_and_summaries() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".agents/skills");
        let yaml = "name: frontmatter-name\ndescription: '  A useful summary  '\nextra:\n  enabled: true\n  count: 7\n  ratio: 1.25\n  missing: null\n  sequence: [one, {two: [3, false]}]\n  tagged: !custom value\n  ? [complex, key]\n  : retained\n";
        let instructions = format!("---\n{yaml}---\n# Body\n\nOriginal instructions\n");
        let path = write_skill(&root, "effective-name", &instructions);
        let skills = HostSkills::discover_from(temp.path(), None).await;
        let inventory = skills.inventory().await;
        assert!(!inventory.has_errors(), "{:?}", inventory.diagnostics);
        assert_eq!(inventory.skills.len(), 1);
        let skill = &inventory.skills[0];
        assert_eq!(skill.name, "effective-name");
        assert_eq!(skill.source, std::fs::canonicalize(path).unwrap());
        assert_eq!(skill.description, "A useful summary");
        assert_eq!(
            skill.frontmatter,
            serde_yaml::from_str::<serde_yaml::Value>(yaml).unwrap()
        );
        assert_eq!(
            skills.get("effective-name").unwrap().instructions,
            instructions
        );
        assert_eq!(
            serde_json::to_value(skills.summaries()).unwrap(),
            serde_json::json!([
                {"name": "effective-name", "description": "A useful summary"}
            ])
        );
        let tree = inventory.render_tree();
        for expected in [
            "effective-name",
            "name: frontmatter-name",
            "enabled: true",
            "count: 7",
            "ratio: 1.25",
            "missing: null",
            "!custom value",
            "assets (empty)",
        ] {
            assert!(tree.contains(expected), "missing {expected}: {tree}");
        }
    }

    #[test]
    fn summary_parser_preserves_existing_typed_yaml_validation_and_coercions() {
        for yaml in [
            "description: a summary",
            "description: null",
            "description: ''",
            "description: 0x10",
            "description: true",
            "description: 1.50",
            "description: [invalid]",
            "description: {invalid: type}",
            "[]",
            "scalar",
            "",
            "other: value",
        ] {
            let original = serde_yaml::from_str::<Frontmatter>(yaml);
            let parsed = parse_instructions(&format!("---\n{yaml}\n---\nFallback prose"));
            match original {
                Ok(original) => {
                    let expected = original
                        .description
                        .filter(|description| !description.trim().is_empty())
                        .map(|description| super::super::compact_description(description.trim()))
                        .unwrap_or_else(|| "Fallback prose".to_owned());
                    assert_eq!(parsed.unwrap().0, expected, "{yaml}");
                }
                Err(_) => assert!(parsed.is_err(), "{yaml}"),
            }
        }
        assert_eq!(
            parse_instructions("# Heading\r\n\r\nProse here").unwrap(),
            ("Prose here".to_owned(), serde_yaml::Value::Null)
        );
        assert_eq!(
            parse_instructions("---\r\ndescription: summary\r\n---\r\nBody")
                .unwrap()
                .0,
            "summary"
        );
        assert!(parse_instructions("---\ndescription: unterminated").is_err());
        assert!(parse_instructions("# Heading only").is_err());
        assert_eq!(
            parse_instructions(&"é".repeat(600))
                .unwrap()
                .0
                .chars()
                .count(),
            512
        );
    }

    #[tokio::test]
    async fn malformed_yaml_and_description_keep_valid_entries_and_context() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".agents/skills");
        let malformed = write_skill(&root, "bad-yaml", "---\ndescription: [broken\n---\nBody");
        let bad_description = write_skill(
            &root,
            "bad-description",
            "---\ndescription: {wrong: type}\n---\nBody",
        );
        write_skill(&root, "valid", "# Title\nValid prose");
        let skills = HostSkills::discover_from(temp.path(), None).await;
        let inventory = skills.inventory().await;
        assert!(inventory.has_errors());
        assert_eq!(
            inventory
                .skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            ["valid"]
        );
        assert_eq!(inventory.diagnostics.len(), 2);
        for path in [malformed, bad_description] {
            assert!(
                inventory
                    .diagnostics
                    .iter()
                    .any(|error| error.contains(&path.display().to_string())
                        && error.contains("invalid YAML frontmatter"))
            );
        }
        assert!(inventory.render_tree().contains("Valid prose"));
        assert_eq!(inventory.diagnostics, skills.warnings());
    }

    #[tokio::test]
    async fn inventory_uses_nearest_loaded_winner_and_does_not_reread_instructions() {
        let temp = tempfile::tempdir().unwrap();
        let user = temp.path().join("user");
        let outer = temp.path().join("project");
        let workspace = outer.join("nested");
        std::fs::create_dir_all(&workspace).unwrap();
        for (root, label) in [
            (user.clone(), "user"),
            (outer.join(".agents/skills"), "outer"),
            (workspace.join(".agents/skills"), "nearest"),
        ] {
            let skill = write_skill(
                &root,
                "common",
                &format!("---\ndescription: {label}\nmarker: {label}\n---\nBody"),
            );
            std::fs::write(skill.join(format!("{label}.txt")), label).unwrap();
        }
        write_skill(&user, "user-only", "User skill");
        write_skill(
            &outer.join(".agents/skills"),
            "fallback",
            "Valid outer skill",
        );
        write_skill(
            &workspace.join(".agents/skills"),
            "fallback",
            "---\ndescription: [bad]\n---\nBody",
        );
        let skills = HostSkills::discover_from(&workspace, Some(&user)).await;
        // Inventory works from the loaded SKILL.md snapshot, not a second read.
        std::fs::remove_file(workspace.join(".agents/skills/common/SKILL.md")).unwrap();
        let inventory = skills.inventory().await;
        assert!(inventory.has_errors());
        let winner = inventory
            .skills
            .iter()
            .find(|skill| skill.name == "common")
            .unwrap();
        assert_eq!(winner.description, "nearest");
        assert_eq!(winner.frontmatter["marker"].as_str(), Some("nearest"));
        assert_eq!(
            winner.assets,
            [SkillAsset {
                path: "nearest.txt".into(),
                kind: SkillAssetKind::File
            }]
        );
        assert_eq!(
            inventory
                .skills
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            ["common", "fallback", "user-only"]
        );
        assert_eq!(inventory.skills[1].description, "Valid outer skill");
    }

    #[tokio::test]
    async fn absent_optional_roots_and_empty_collections_succeed() {
        let temp = tempfile::tempdir().unwrap();
        let inventory = HostSkills::discover_from(temp.path(), Some(&temp.path().join("absent")))
            .await
            .inventory()
            .await;
        assert!(!inventory.has_errors(), "{:?}", inventory.diagnostics);
        assert!(inventory.skills.is_empty());
        assert_eq!(inventory.render_tree(), "skills (empty)\n");
    }

    #[tokio::test]
    async fn discovery_and_asset_errors_retain_valid_entries() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".agents/skills");
        let removed = write_skill(&root, "removed", "Removed assets");
        write_skill(&root, "valid", "Valid assets");
        let bad_root = temp.path().join("not-directory");
        std::fs::write(&bad_root, "not a directory").unwrap();
        let skills = HostSkills::discover_from(temp.path(), Some(&bad_root)).await;
        std::fs::remove_dir_all(&removed).unwrap();
        let inventory = skills.inventory().await;
        assert_eq!(inventory.skills.len(), 2);
        assert_eq!(inventory.diagnostics.len(), 2);
        assert!(
            inventory
                .diagnostics
                .iter()
                .any(|error| error.contains(&bad_root.display().to_string())
                    && error.contains("Cannot scan"))
        );
        assert!(
            inventory
                .diagnostics
                .iter()
                .any(|error| error.contains(&removed.display().to_string())
                    && error.contains("Cannot inspect assets"))
        );
        assert!(inventory.render_tree().contains("Valid assets"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn assets_are_recursive_sorted_and_symlinks_and_special_files_are_not_read() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let skill = write_skill(&temp.path().join(".agents/skills"), "demo", "Demo skill");
        std::fs::create_dir_all(skill.join("nested/deeper")).unwrap();
        for path in [
            "z.txt",
            "a.txt",
            "nested/deeper/asset.bin",
            "nested/SKILL.md",
            "line\nbreak",
        ] {
            std::fs::write(skill.join(path), b"\0binary").unwrap();
        }
        // Even an unreadable asset is listable: inventory only needs metadata.
        std::fs::set_permissions(skill.join("a.txt"), std::fs::Permissions::from_mode(0o0))
            .unwrap();
        symlink("nested", skill.join("link-dir")).unwrap();
        symlink("missing", skill.join("link-broken")).unwrap();
        symlink(".", skill.join("loop")).unwrap();
        symlink(temp.path(), skill.join("outside")).unwrap();
        let socket = std::os::unix::net::UnixListener::bind(skill.join("socket")).unwrap();
        let inventory = HostSkills::discover_from(temp.path(), None)
            .await
            .inventory()
            .await;
        assert!(!inventory.has_errors(), "{:?}", inventory.diagnostics);
        let assets = &inventory.skills[0].assets;
        assert_eq!(
            assets
                .iter()
                .map(|asset| asset.path.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            [
                "a.txt",
                "line\nbreak",
                "link-broken",
                "link-dir",
                "loop",
                "nested",
                "nested/SKILL.md",
                "nested/deeper",
                "nested/deeper/asset.bin",
                "outside",
                "socket",
                "z.txt"
            ]
        );
        assert_eq!(
            assets
                .iter()
                .filter(|asset| asset.kind == SkillAssetKind::Symlink)
                .count(),
            4
        );
        assert!(assets.iter().any(
            |asset| asset.path == Path::new("socket") && asset.kind == SkillAssetKind::Special
        ));
        let tree = inventory.render_tree();
        assert!(tree.contains("link-dir [symlink]"));
        assert!(tree.contains("socket [special]"));
        assert!(tree.contains("line\\nbreak"));
        assert!(!tree.contains("line\nbreak"));
        assert!(tree.contains("        ├── nested/\n        │   ├── SKILL.md\n        │   └── deeper/\n        │       └── asset.bin"), "{tree}");
        drop(socket);
    }
}
