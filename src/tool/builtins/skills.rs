use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::tool::{
    AdmissionError, RegistryError, ToolError, ToolOptions, ToolOutput, ToolRegistryBuilder,
    diagnostic::{FailureSite, Operation, PartialContext, Subject, escape_controls},
    policy::{Capability, CapabilitySet},
};
use crate::{
    fs::FileKind,
    media::{Classified, ImageRef, MAX_IMAGE_BYTES, MAX_TEXT_BYTES},
    session::SessionStore,
};

#[derive(Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SkillArgs {
    /// Skill to use; omit to list available skills.
    name: Option<String>,
    /// Relative file or directory within the skill; `.` lists its root. Omitted or null loads instructions.
    #[serde(default)]
    path: Option<String>,
}

enum SkillRequest {
    List,
    Instructions { name: String },
    Inspect { name: String, asset: String },
}

impl TryFrom<SkillArgs> for SkillRequest {
    type Error = AdmissionError;

    fn try_from(args: SkillArgs) -> Result<Self, Self::Error> {
        // Never trim the retained name or asset: spaces may be meaningful filename bytes.
        for (name, value) in [
            ("name", args.name.as_deref()),
            ("path", args.path.as_deref()),
        ] {
            if value.is_some_and(|value| value.trim().is_empty()) {
                return Err(AdmissionError::invalid_arguments(format!(
                    "{name} must not be empty"
                )));
            }
        }
        match (args.name, args.path) {
            (None, None) => Ok(Self::List),
            (None, Some(_)) => Err(AdmissionError::invalid_arguments("path requires name")),
            (Some(name), None) => Ok(Self::Instructions { name }),
            (Some(name), Some(asset)) => Ok(Self::Inspect { name, asset }),
        }
    }
}

const MAX_DESCRIPTION_CHARS: usize = 512;

#[derive(Clone, Default)]
pub struct HostSkills {
    entries: Arc<BTreeMap<String, SkillEntry>>,
    warnings: Arc<Vec<String>>,
}

#[derive(Clone)]
struct SkillEntry {
    name: String,
    description: String,
    root: PathBuf,
    instructions: String,
    frontmatter: serde_json::Value,
}

impl HostSkills {
    pub async fn discover(workspace: &Path) -> Self {
        Self::discover_from(workspace, user_skills_root().as_deref()).await
    }

    async fn discover_from(workspace: &Path, user_root: Option<&Path>) -> Self {
        let mut entries = BTreeMap::new();
        let mut warnings = Vec::new();
        if let Some(user_root) = user_root {
            scan_root(user_root, &mut entries, &mut warnings).await;
        }
        let workspace = match fs::canonicalize(workspace).await {
            Ok(workspace) => workspace,
            Err(error) => {
                warnings.push(host_warning(ToolError::from(error).operation(
                    Operation::Canonicalize,
                    Subject::working_directory(workspace),
                )));
                workspace.to_path_buf()
            }
        };
        let mut ancestors = workspace.ancestors().collect::<Vec<_>>();
        ancestors.reverse();
        for ancestor in ancestors {
            scan_root(
                &ancestor.join(".agents/skills"),
                &mut entries,
                &mut warnings,
            )
            .await;
        }
        Self {
            entries: Arc::new(entries),
            warnings: Arc::new(warnings),
        }
    }

    /// Render every winning skill's source, description, frontmatter and asset
    /// tree without reading asset contents. Returns the text and all errors.
    pub async fn describe(&self) -> (String, Vec<String>) {
        let (mut text, mut errors) = (String::new(), self.warnings.to_vec());
        let mut unreadable = Vec::new();
        for entry in self.entries.values() {
            let yaml = serde_saphyr::to_string(&entry.frontmatter)
                .unwrap_or_else(|error| format!("[cannot render YAML: {error}]"));
            // The canonical root may have been replaced since discovery; links are not followed.
            let listed = match fs::symlink_metadata(&entry.root).await {
                Ok(metadata) if !metadata.is_dir() => {
                    Err(ToolError::failed("skill root is no longer a directory")
                        .operation(Operation::ReadDirectory, Subject::path(&entry.root)))
                }
                _ => asset_tree(&entry.root, true, Some(&mut unreadable)).await,
            };
            errors.extend(unreadable.drain(..).map(host_warning));
            let assets = listed.unwrap_or_else(|error| {
                errors.push(host_warning(error));
                String::new()
            });
            text.push_str(&format!(
                "{}\n  source: {}\n  description: {}\n",
                escape_controls(&entry.name),
                escape_controls(entry.root.display()),
                escape_controls(&entry.description)
            ));
            for (title, body) in [("frontmatter", yaml), ("assets", assets)] {
                text.push_str(&format!("  {title}:\n"));
                for line in body.lines() {
                    text.push_str(&format!("    {}\n", escape_controls(line)));
                }
            }
        }
        if text.is_empty() {
            text.push_str("skills (empty)\n");
        }
        (text, errors)
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    fn summaries(&self) -> Vec<SkillSummary> {
        self.entries
            .values()
            .map(|entry| SkillSummary {
                name: entry.name.clone(),
                description: entry.description.clone(),
            })
            .collect()
    }

    fn get(&self, name: &str) -> Result<&SkillEntry, ToolError> {
        self.entries.get(name).ok_or_else(|| {
            ToolError::failed("unknown skill")
                .operation(Operation::Lookup, Subject::argument(["name"]))
        })
    }
}

fn user_skills_root() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".agents/skills"))
}

/// Discovery and describe report outside tool dispatch, which would otherwise bind the site.
fn host_warning(error: ToolError) -> String {
    error
        .at(FailureSite::Host)
        .diagnostic()
        .render(&CapabilitySet::default())
}

fn at(operation: Operation, path: &Path) -> PartialContext {
    PartialContext::new(operation, Subject::path(path))
}

async fn scan_root(
    root: &Path,
    entries: &mut BTreeMap<String, SkillEntry>,
    warnings: &mut Vec<String>,
) {
    let mut directory = match fs::read_dir(root).await {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            warnings.push(host_warning(
                ToolError::io(error).context(at(Operation::ReadDirectory, root)),
            ));
            return;
        }
    };
    let mut paths = Vec::new();
    loop {
        match directory.next_entry().await {
            Ok(Some(entry)) => paths.push(entry.path()),
            Ok(None) => break,
            Err(error) => {
                warnings.push(host_warning(
                    ToolError::io(error).context(at(Operation::ReadDirectory, root)),
                ));
                break;
            }
        }
    }
    paths.sort();
    for path in paths {
        match load_skill(&path).await {
            Ok(entry) => {
                entries.insert(entry.name.clone(), entry);
            }
            Err(error) => warnings.push(host_warning(error)),
        }
    }
}

async fn load_skill(path: &Path) -> Result<SkillEntry, ToolError> {
    let metadata = fs::metadata(path)
        .await
        .map_err(ToolError::annotated(at(Operation::Inspect, path)))?;
    if !metadata.is_dir() {
        return Err(ToolError::failed("entry is not a directory")
            .operation(Operation::Load, Subject::path(path)));
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
        .ok_or_else(|| {
            ToolError::failed("skill directory name is invalid")
                .operation(Operation::Validate, Subject::path(path))
        })?
        .to_owned();
    let root = fs::canonicalize(path)
        .await
        .map_err(ToolError::annotated(at(Operation::Canonicalize, path)))?;
    let instruction_path = path.join("SKILL.md");
    let instruction_path =
        fs::canonicalize(&instruction_path)
            .await
            .map_err(ToolError::annotated(at(
                Operation::Canonicalize,
                &instruction_path,
            )))?;
    if !instruction_path.starts_with(&root) {
        return Err(ToolError::failed("SKILL.md escapes its skill directory")
            .operation(Operation::Validate, Subject::path(&instruction_path)));
    }
    let bytes = crate::fs::read_regular(&instruction_path, MAX_TEXT_BYTES)
        .await
        .map_err(ToolError::annotated(at(Operation::Read, &instruction_path)))?;
    let instructions = String::from_utf8(bytes).map_err(|error| {
        ToolError::failed(error.utf8_error())
            .operation(Operation::Deserialize, Subject::path(&instruction_path))
    })?;
    let (description, frontmatter) = parse_instructions(&instructions).map_err(|error| {
        ToolError::failed(error).operation(Operation::Deserialize, Subject::path(&instruction_path))
    })?;
    Ok(SkillEntry {
        name,
        description,
        root,
        instructions,
        frontmatter,
    })
}

fn parse_instructions(instructions: &str) -> Result<(String, serde_json::Value), String> {
    use serde_json::Value;

    // Normalize only for parsing: the stored instructions retain their original bytes.
    let normalized = instructions.replace("\r\n", "\n");
    let instructions = normalized.as_str();
    let mut metadata = Value::Null;
    let body = if let Some(rest) = instructions.strip_prefix("---\n") {
        let (frontmatter, body) = rest
            .split_once("\n---\n")
            .or_else(|| {
                rest.strip_suffix("\n---")
                    .map(|frontmatter| (frontmatter, ""))
            })
            .ok_or_else(|| "unterminated YAML frontmatter".to_owned())?;
        metadata = crate::yaml::from_str(frontmatter)
            .map_err(|error| format!("invalid YAML frontmatter: {error}"))?;
        let description = match &metadata {
            Value::Null => None,
            Value::Object(fields) => fields.get("description"),
            _ => return Err("invalid YAML frontmatter: expected a mapping".to_owned()),
        };
        let description = match description {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(text)) => text.clone(),
            Some(_) => {
                return Err("invalid YAML frontmatter: description must be a string".to_owned());
            }
        };
        if !description.trim().is_empty() {
            return Ok((compact_description(description.trim()), metadata));
        }
        body
    } else {
        instructions
    };
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(compact_description)
        .map(|description| (description, metadata))
        .ok_or_else(|| "skill has no description or prose".to_owned())
}

fn compact_description(description: &str) -> String {
    description.chars().take(MAX_DESCRIPTION_CHARS).collect()
}

pub(super) fn register(
    builder: &mut ToolRegistryBuilder,
    skills: HostSkills,
    store: SessionStore,
) -> Result<(), RegistryError> {
    // Discovery is startup-only, so a session without skills never needs the tool.
    if skills.entries.is_empty() {
        return Ok(());
    }
    builder.register_product::<SkillArgs, SkillOutput, _, _>(
        "skill",
        "List host-owned skills, or with `name` load complete skill instructions and discover assets. Select a relative path to list a directory (use `.` for the root), read text, attach a supported image, or inspect binary metadata.",
        ToolOptions::new(vec![Capability::Read])
            .argument_validator(|args: &SkillArgs| SkillRequest::try_from(args.clone()).map(drop))
            .conditional_output(
                crate::target::TargetPath::SCHEMA,
                "target",
                Capability::Targets,
                crate::target::TargetPath::target_schema(),
            ),
        move |context, args| {
            let skills = skills.clone();
            let store = store.clone();
            async move {
                match SkillRequest::try_from(args)? {
                    SkillRequest::List => skill_output(SkillOutput::List {
                        skills: skills.summaries(),
                    }),
                    SkillRequest::Instructions { name } => {
                        let entry = skills.get(&name)?;
                        skill_output(SkillOutput::Skill {
                            name: entry.name.clone(),
                            description: entry.description.clone(),
                            location: location(&context, &entry.root),
                            content: entry.instructions.clone(),
                            assets: asset_tree(&entry.root, true, None).await?,
                        })
                    }
                    SkillRequest::Inspect { name, asset } => {
                        inspect_asset(skills.get(&name)?, asset, &store).await
                    }
                }
            }
        },
    )?;
    Ok(())
}

async fn inspect_asset(
    entry: &SkillEntry,
    asset: String,
    store: &SessionStore,
) -> Result<ToolOutput, ToolError> {
    let source = resolve_asset(entry, &asset).await?;
    let metadata = fs::metadata(&source)
        .await
        .map_err(ToolError::annotated(at(Operation::Inspect, &source)))?;
    if metadata.is_dir() {
        return skill_output(SkillOutput::Directory {
            name: entry.name.clone(),
            path: asset,
            assets: asset_tree(&source, false, None).await?,
        });
    }
    // Presentation reads only: larger assets are copied with write's source.
    let bytes = crate::fs::read_regular(&source, MAX_IMAGE_BYTES.max(MAX_TEXT_BYTES))
        .await
        .map_err(|error| {
            match error {
                crate::fs::RegularFileError::TooLarge { limit } => ToolError::failed(format!(
                    "skill asset exceeds {limit} bytes; copy it with write's source"
                )),
                error => error.into(),
            }
            .context(at(Operation::Read, &source))
        })?;
    match crate::media::classify(bytes) {
        Classified::Image(image) => {
            let image = store
            .store_image(Some(asset.clone()), &image)
                .await
                .map_err(ToolError::annotated(at(Operation::StoreImage, &source)))?;
            Ok(skill_output(SkillOutput::Image {
                name: entry.name.clone(),
                path: asset,
                image: image.clone(),
            })?
            .with_images(vec![image]))
        }
        Classified::Text(content) if content.len() as u64 > MAX_TEXT_BYTES => {
            Err(ToolError::failed(format!(
                "text skill asset exceeds {MAX_TEXT_BYTES} bytes; copy it with write's source"
            ))
            .operation(Operation::Read, Subject::path(&source)))
        }
        Classified::Text(content) => skill_output(SkillOutput::Text {
            name: entry.name.clone(),
            path: asset,
            bytes: content.len(),
            content,
        }),
        Classified::Binary(bytes) => skill_output(SkillOutput::Binary {
            name: entry.name.clone(),
            path: asset,
            bytes: bytes.len(),
            note: "Binary contents are not inlined; copy the asset with write's source from the skill location.".to_owned(),
        }),
    }
}

/// Skills live on the session host. The target is named only for callers that
/// can select one; a remote caller without that capability cannot reach the
/// directory, so it gets no location.
fn location(context: &crate::tool::ToolContext, root: &Path) -> Option<crate::target::TargetPath> {
    let target = context
        .capabilities()
        .visible_target(&crate::target::TargetRef::Root)
        .cloned();
    (target.is_some() || context.caller_location().is_root()).then(|| crate::target::TargetPath {
        path: root.to_string_lossy().into_owned(),
        target,
    })
}

fn skill_output(output: SkillOutput) -> Result<ToolOutput, ToolError> {
    Ok(ToolOutput::new(serde_json::to_value(output)?))
}

// Render the complete tree; the output schema, not discovery, controls model-view
// truncation. Keep an explicit stack so deeply nested assets do not recurse.
// With an error sink, an unreadable directory is marked and reported instead of
// failing the listing.
async fn asset_tree(
    root: &Path,
    exclude_instructions: bool,
    mut unreadable: Option<&mut Vec<ToolError>>,
) -> Result<String, ToolError> {
    let mut tree = String::new();
    let mut pending = vec![(root.to_path_buf(), String::new(), true, true)];
    while let Some((path, prefix, last, is_root)) = pending.pop() {
        let metadata = fs::symlink_metadata(&path)
            .await
            .map_err(ToolError::annotated(at(Operation::Inspect, &path)))?;
        let kind = FileKind::from(metadata.file_type());
        if !is_root {
            tree.push_str(&prefix);
            tree.push_str(if last { "└── " } else { "├── " });
            // Control characters in filenames must not create fake tree lines.
            tree.push_str(&escape_controls(
                path.file_name().unwrap().to_string_lossy(),
            ));
            tree.push_str(match kind {
                FileKind::File => "",
                FileKind::Directory => "/",
                FileKind::Symlink => " [symlink]",
                FileKind::Other => " [special]",
            });
            tree.push('\n');
        }
        // Never traverse a symlink, including links back to a parent directory.
        if kind != FileKind::Directory {
            continue;
        }
        let mut directory = match (fs::read_dir(&path).await, unreadable.as_deref_mut()) {
            (Ok(directory), _) => directory,
            (Err(error), Some(unreadable)) => {
                if !is_root {
                    tree.insert_str(tree.len() - 1, " [unreadable]");
                }
                unreadable.push(ToolError::io(error).context(at(Operation::ReadDirectory, &path)));
                continue;
            }
            (Err(error), None) => {
                return Err(ToolError::io(error).context(at(Operation::ReadDirectory, &path)));
            }
        };
        let mut children = Vec::new();
        while let Some(child) = directory
            .next_entry()
            .await
            .map_err(ToolError::annotated(at(Operation::ReadDirectory, &path)))?
        {
            if is_root && exclude_instructions && child.file_name() == "SKILL.md" {
                continue;
            }
            let kind = child
                .file_type()
                .await
                .map_err(ToolError::annotated(at(Operation::Inspect, &child.path())))?;
            children.push((!kind.is_dir(), child.file_name(), child.path()));
        }
        // Directories first, then other entries; each group is name-sorted.
        children.sort();
        let count = children.len();
        let child_prefix = if is_root {
            String::new()
        } else {
            format!("{prefix}{}", if last { "    " } else { "│   " })
        };
        for (index, (_, _, child)) in children.into_iter().enumerate().rev() {
            pending.push((child, child_prefix.clone(), index + 1 == count, false));
        }
    }
    // No trailing newline keeps empty trees an ordinary empty string.
    tree.pop();
    Ok(tree)
}

/// A nonblank relative source with no parent/root/prefix components. This does
/// not prove existence, file kind, or containment after following symlinks.
fn check_asset_syntax(asset: &str) -> Result<(), ToolError> {
    if asset.trim().is_empty()
        || Path::new(asset)
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(ToolError::invalid_arguments(
            "asset path must be relative and cannot contain `..`",
        ));
    }
    Ok(())
}

// Canonicalization checks actual symlink containment at use time; it is not TOCTOU immunity.
async fn resolve_asset(entry: &SkillEntry, asset: &str) -> Result<PathBuf, ToolError> {
    check_asset_syntax(asset)
        .map_err(|error| error.operation(Operation::Validate, Subject::argument(["path"])))?;
    let source = entry.root.join(asset);
    let source = fs::canonicalize(&source)
        .await
        .map_err(ToolError::annotated(at(Operation::Canonicalize, &source)))?;
    if !source.starts_with(&entry.root) {
        return Err(ToolError::failed("skill asset escapes its skill directory")
            .operation(Operation::Validate, Subject::path(&source)));
    }
    Ok(source)
}

#[derive(Serialize, JsonSchema)]
struct SkillSummary {
    name: String,
    description: String,
}

#[derive(Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SkillOutput {
    List {
        skills: Vec<SkillSummary>,
    },
    Skill {
        name: String,
        description: String,
        /// The skill directory, for `read` and `write` sources.
        #[serde(skip_serializing_if = "Option::is_none")]
        location: Option<crate::target::TargetPath>,
        content: String,
        #[schemars(extend("x-skyhook-truncatable" = true))]
        assets: String,
    },
    Directory {
        name: String,
        path: String,
        #[schemars(extend("x-skyhook-truncatable" = true))]
        assets: String,
    },
    Image {
        name: String,
        path: String,
        image: ImageRef,
    },
    Binary {
        name: String,
        path: String,
        bytes: usize,
        note: String,
    },
    Text {
        name: String,
        path: String,
        #[schemars(extend("x-skyhook-truncatable" = true))]
        content: String,
        bytes: usize,
    },
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::{
        tests::TestRuntime,
        tool::{ToolRegistryBuilder, diagnostic::Cause},
    };

    #[test]
    fn summary_comes_from_a_string_description_else_the_first_prose() {
        for (yaml, expected) in [
            ("description: a summary", Some("a summary")),
            ("description: '  a summary  '", Some("a summary")),
            (
                "description: >-\n  a folded\n  summary",
                Some("a folded summary"),
            ),
            ("description: null", Some("Fallback prose")),
            ("description:", Some("Fallback prose")),
            ("description: ''", Some("Fallback prose")),
            ("description: '  '", Some("Fallback prose")),
            ("other: value", Some("Fallback prose")),
            ("description: 'true'", Some("true")),
            ("description: true", None),
            ("description: 42", None),
            ("description: 0x10", None),
            ("description: [invalid]", None),
            ("description: {invalid: value}", None),
            ("scalar", None),
        ] {
            let parsed = parse_instructions(&format!("---\n{yaml}\n---\nFallback prose"));
            assert_eq!(
                parsed.ok().map(|parsed| parsed.0).as_deref(),
                expected,
                "{yaml}"
            );
        }
        let summary = |text: &str| parse_instructions(text).map(|parsed| parsed.0);
        assert_eq!(
            parse_instructions("# Heading\r\n\r\nProse here").unwrap(),
            ("Prose here".to_owned(), serde_json::Value::Null)
        );
        assert_eq!(
            summary("---\r\ndescription: summary\r\n---\r\nBody").unwrap(),
            "summary"
        );
        assert!(summary("---\ndescription: unterminated").is_err());
        assert!(summary("# Heading only").is_err());
        assert_eq!(summary(&"é".repeat(600)).unwrap().chars().count(), 512);
    }

    #[test]
    fn frontmatter_retains_json_metadata_and_rejects_yaml_only_values() {
        let (_, metadata) = parse_instructions(
            "---\ndescription: summary\nextra: [0x10, {two: null}]\nother: yes\n---\nBody",
        )
        .unwrap();
        assert_eq!(
            metadata,
            json!({"description": "summary", "extra": [16, {"two": null}], "other": "yes"})
        );
        for yaml in [
            "description: !custom value",
            "extra: !custom value",
            "extra: .nan",
            "extra: {1: value}",
            "extra: {[a, b]: value}",
            "extra: {same: 1, same: 2}",
            "extra: {<<: {merged: value}}",
        ] {
            let error = parse_instructions(&format!("---\n{yaml}\n---\nBody")).unwrap_err();
            assert!(error.starts_with("invalid YAML frontmatter:"), "{error}");
        }
    }

    #[tokio::test]
    async fn discovery_warnings_identify_the_operation_and_instruction_file() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join(".agents/skills/missing-instructions");
        fs::create_dir_all(&missing).await.unwrap();
        let error = load_skill(&missing).await.err().unwrap();
        let diagnostic = error.diagnostic();
        assert_eq!(diagnostic.context.operation, Operation::Canonicalize);
        assert_eq!(
            diagnostic.context.subject,
            Subject::path(missing.join("SKILL.md"))
        );
        let skills = HostSkills::discover_from(temp.path(), None).await;
        // Discovery is already host-facing; keep the subject without repeating the site.
        assert!(matches!(skills.warnings(), [warning]
            if warning.contains("SKILL.md") && !warning.contains("session host")));
        // Without discovered skills the session offers no skill tool at all.
        let runtime = TestRuntime::new().await;
        assert!(builder(&runtime, skills).build().get("skill").is_none());
    }

    #[tokio::test]
    async fn describe_renders_frontmatter_and_assets_and_reports_every_error() {
        let temp = tempfile::tempdir().unwrap();
        let skills = HostSkills::discover_from(temp.path(), None).await;
        assert_eq!(
            skills.describe().await,
            ("skills (empty)\n".to_owned(), Vec::new())
        );

        let root = temp.path().join(".agents/skills");
        for (name, text) in [
            (
                "good",
                "---\ndescription: Useful\nextra: [1, {two: null}]\n---\nBody",
            ),
            ("bad", "---\ndescription: [invalid]\n---\nBody"),
        ] {
            std::fs::create_dir_all(root.join(name).join("refs")).unwrap();
            std::fs::write(root.join(name).join("SKILL.md"), text).unwrap();
        }
        std::fs::write(root.join("good/refs/a\u{1b}.md"), "private").unwrap();
        std::os::unix::fs::symlink("/", root.join("good/link")).unwrap();
        // Unreadable directories are marked and reported; the rest is still listed.
        use std::os::unix::fs::PermissionsExt as _;
        let locked = ["locked", "sealed"].map(|name| root.join("good").join(name));
        for path in &locked {
            std::fs::create_dir(path).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        let is_root = std::fs::read_dir(&locked[0]).is_ok();
        let (text, errors) = HostSkills::discover_from(temp.path(), None)
            .await
            .describe()
            .await;
        for path in &locked {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mark = if is_root { "" } else { " [unreadable]" };
        for expected in [
            "good\n  source: ".to_owned(),
            "  description: Useful\n  frontmatter:\n    description: Useful\n    extra:\n"
                .to_owned(),
            format!(
                "  assets:\n    ├── locked/{mark}\n    ├── refs/\n    │   └── a\\u{{1b}}.md\n    \
                 ├── sealed/{mark}\n    └── link [symlink]\n"
            ),
        ] {
            assert!(text.contains(&expected), "missing {expected:?}: {text}");
        }
        assert!(!text.contains("private") && !text.contains("bad\n"));
        assert!(errors[0].contains("bad") && errors[0].contains("invalid YAML frontmatter"));
        if !is_root {
            assert_eq!(errors.len(), 3, "{errors:?}");
            assert!(errors[1].contains("good/locked") && errors[2].contains("good/sealed"));
        }

        // A root replaced after discovery is an error, never an empty listing.
        let skills = HostSkills::discover_from(temp.path(), None).await;
        std::fs::remove_dir_all(root.join("good")).unwrap();
        std::fs::write(root.join("good"), "not a directory").unwrap();
        let (_, errors) = skills.describe().await;
        assert!(
            errors
                .iter()
                .any(|error| error.contains("no longer a directory")),
            "{errors:?}"
        );
    }

    #[test]
    fn skill_requests_admit_operations_and_reject_blank_or_malformed_fields() {
        let request = |value| {
            serde_json::from_value::<SkillArgs>(value)
                .map_err(AdmissionError::invalid_arguments)
                .and_then(SkillRequest::try_from)
        };
        assert!(matches!(request(json!({})).unwrap(), SkillRequest::List));
        assert!(matches!(
            request(json!({"name":"demo","path":null})).unwrap(),
            SkillRequest::Instructions { name } if name == "demo"
        ));
        // Nothing is trimmed: spaces may be meaningful filename bytes.
        assert!(matches!(
            request(json!({"name":" demo ","path":" asset "})).unwrap(),
            SkillRequest::Inspect { name, asset } if name == " demo " && asset == " asset "
        ));
        for value in [
            json!({"path":"."}),
            json!({"name":1}),
            json!({"name":" \t\n"}),
            json!({"name":"demo","path":" "}),
            json!({"name":"demo","path":"file","to":"copy"}),
        ] {
            let result = request(value.clone());
            assert!(
                matches!(result, Err(error) if matches!(error.diagnostic().cause, Cause::InvalidArguments(_))),
                "{value}"
            );
        }
    }

    #[test]
    fn asset_syntax_is_confined_without_normalizing_spelling() {
        for value in [
            "",
            " \n",
            "/absolute",
            "..",
            "../file",
            "a/../b",
            "a/..",
            "./../file",
        ] {
            assert!(check_asset_syntax(value).is_err(), "{value:?}");
        }
        for value in [
            ".",
            "./",
            "file",
            "a/b",
            "./a//b",
            " leading ",
            "a/ trailing ",
            "...",
            "a/./b",
        ] {
            check_asset_syntax(value).unwrap();
        }
    }

    fn builder(runtime: &TestRuntime, skills: HostSkills) -> ToolRegistryBuilder {
        let mut builder = ToolRegistryBuilder::default();
        super::register(&mut builder, skills, runtime.store.clone()).unwrap();
        builder
    }

    async fn fixture_skills() -> HostSkills {
        let workspace =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skill-workspace");
        HostSkills::discover_from(&workspace, None).await
    }

    #[tokio::test]
    async fn malformed_requests_never_authorize_and_assets_stay_confined() {
        use crate::tool::policy::{AuthorizationRequest, Policy, PolicyFuture};
        struct UnexpectedAuthorization;
        impl Policy for UnexpectedAuthorization {
            fn authorize(&self, _: AuthorizationRequest) -> PolicyFuture<'_> {
                panic!("malformed skill arguments must not request authorization");
            }
        }
        let runtime = TestRuntime::new().await;
        let agent = &runtime.agent;
        let strict = runtime.executor_with_policy(
            builder(&runtime, fixture_skills().await),
            Arc::new(UnexpectedAuthorization),
        );
        let before = runtime.jobs.list(agent).await.len();
        for args in [
            json!({"path":"."}),
            json!({"name":""}),
            json!({"name":"mixed-assets", "path":""}),
            json!({"name":"mixed-assets", "path":"references/note.txt", "to":"copied"}),
        ] {
            let error = strict.run_host(agent, "skill", args).await.unwrap_err();
            assert!(matches!(
                error.diagnostic().cause,
                Cause::InvalidArguments(_)
            ));
        }
        assert_eq!(runtime.jobs.list(agent).await.len(), before);

        let executor = runtime.executor(builder(&runtime, fixture_skills().await));
        for path in ["../mixed-assets/SKILL.md", "/etc/passwd"] {
            let args = json!({"name":"mixed-assets", "path":path});
            assert!(
                executor.run_host(agent, "skill", args).await.is_err(),
                "{path}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_discovery_does_not_allow_asset_escape() {
        use std::os::unix::fs::symlink;
        let runtime = TestRuntime::new().await;
        let root = runtime.root.path().join(".agents/skills/demo");
        fs::create_dir_all(&root).await.unwrap();
        fs::write(root.join("SKILL.md"), "# Demo\nSafe instructions.")
            .await
            .unwrap();
        fs::write(runtime.root.path().join("secret"), "outside")
            .await
            .unwrap();
        symlink(runtime.root.path().join("secret"), root.join("escape")).unwrap();
        symlink("SKILL.md", root.join("inside")).unwrap();
        symlink(".", root.join("loop")).unwrap();
        let skills = HostSkills::discover_from(runtime.root.path(), None).await;
        let executor = runtime.executor(builder(&runtime, skills));
        let skill = async |args| executor.run_host(&runtime.agent, "skill", args).await;
        let listed = skill(json!({"name":"demo"})).await.unwrap();
        assert_eq!(
            listed.output.value["assets"],
            "├── escape [symlink]\n├── inside [symlink]\n└── loop [symlink]"
        );
        assert!(
            skill(json!({"name":"demo", "path":"escape"}))
                .await
                .is_err()
        );
        let loaded = skill(json!({"name":"demo", "path":"inside"}))
            .await
            .unwrap();
        assert_eq!(loaded.output.value["kind"], "text");
    }

    #[tokio::test]
    async fn nearest_skill_wins_and_its_location_sources_asset_copies() {
        let runtime = TestRuntime::new().await;
        let user = runtime.root.path().join("user-skills");
        let outer = runtime.root.path().join("project");
        let workspace = outer.join("workspace");
        for (skills_root, marker) in [
            (user.clone(), "user"),
            (outer.join(".agents/skills"), "outer"),
            (workspace.join(".agents/skills"), "nearest"),
        ] {
            let skill = skills_root.join("common");
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(skill.join("SKILL.md"), format!("# Common\n\n{marker}")).unwrap();
            std::fs::write(skill.join("asset.bin"), marker.as_bytes()).unwrap();
            std::fs::write(skill.join(" asset "), "spaced content").unwrap();
        }
        let workspace = std::fs::canonicalize(workspace).unwrap();
        let skills = HostSkills::discover_from(&workspace, Some(&user)).await;
        let location = crate::execution::ExecutionLocation::root(workspace.clone());
        let mut builder = builder(&runtime, skills);
        builder
            .register_local(crate::tool::builtins::register_local_tools)
            .unwrap();
        let executor = runtime.executor(builder).with_location(location);
        let skill = async |args| {
            executor
                .run_host(&runtime.agent, "skill", args)
                .await
                .unwrap()
        };
        // The host skill tool is always read-only.
        let capabilities = executor.registry().get("skill").unwrap().capabilities();
        assert_eq!(capabilities, vec![Capability::Read]);
        let listed = skill(json!({})).await;
        assert_eq!(
            listed.output.value,
            json!({"kind":"list", "skills":[{"name":"common", "description":"nearest"}]})
        );
        let loaded = skill(json!({"name":"common"})).await;
        assert!(
            loaded.output.value["content"]
                .as_str()
                .unwrap()
                .contains("nearest")
        );
        // The target is named only for callers that can select targets, and a
        // remote caller that cannot gets no location at all.
        let nearest = workspace.join(".agents/skills/common");
        let path = nearest.to_str().unwrap();
        assert_eq!(loaded.output.value["location"], json!({"path":path}));
        let mut targets = CapabilitySet::default();
        targets.insert(Capability::Targets);
        let agent = &runtime.agent;
        let location = async |executor: crate::tool::executor::ToolExecutor| {
            let args = json!({"name":"common"});
            let loaded = executor.run_host(agent, "skill", args).await.unwrap();
            loaded.output.value.get("location").cloned()
        };
        assert_eq!(
            location(executor.clone().with_capabilities(targets)).await,
            Some(json!({"path":path, "target":"root"}))
        );
        let remote = crate::execution::ExecutionLocation {
            target: "remote".parse().unwrap(),
            workspace: workspace.clone(),
        };
        assert_eq!(location(executor.clone().with_location(remote)).await, None);
        let source = nearest.join("asset.bin");
        let copy = json!({"path":"copied.bin", "source":{"path":source.to_str().unwrap()}});
        let copied = executor
            .run_host(&runtime.agent, "write", copy)
            .await
            .unwrap();
        assert_eq!(copied.output.value["bytes"], 7);
        assert_eq!(
            std::fs::read(workspace.join("copied.bin")).unwrap(),
            b"nearest"
        );
        // Spaced asset spellings round-trip untrimmed.
        let loaded = skill(json!({"name":"common","path":" asset "}))
            .await
            .output
            .value;
        assert_eq!(
            (&loaded["kind"], &loaded["path"], &loaded["content"]),
            (&json!("text"), &json!(" asset "), &json!("spaced content"))
        );
        // An asset too large to present points at the copy instead.
        let large = std::fs::File::create(nearest.join("large.bin")).unwrap();
        large.set_len(crate::media::MAX_IMAGE_BYTES + 1).unwrap();
        let error = executor
            .run_host(
                &runtime.agent,
                "skill",
                json!({"name":"common","path":"large.bin"}),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error.diagnostic().cause, Cause::Message(text) if text.contains("write's source"))
        );
    }
}
