use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::{fs, io::AsyncReadExt};

use super::filesystem::detect_image;
use crate::tool::{
    RegistryError, ToolError, ToolOptions, ToolOutput, ToolRegistryBuilder, policy::Capability,
};
use crate::{
    media::{ImageReference, MAX_IMAGE_BYTES},
    session::SessionStore,
};

const MAX_INLINE_BYTES: u64 = 1024 * 1024;
const MAX_COPY_BYTES: u64 = 8 * 1024 * 1024;
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
        let workspace = fs::canonicalize(workspace)
            .await
            .unwrap_or_else(|_| workspace.to_path_buf());
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
        self.entries
            .get(name)
            .ok_or_else(|| ToolError::Failed(format!("unknown skill `{name}`")))
    }
}

fn user_skills_root() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".agents/skills"))
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
            warnings.push(format!("Cannot scan skills at {}: {error}", root.display()));
            return;
        }
    };
    let mut paths = Vec::new();
    loop {
        match directory.next_entry().await {
            Ok(Some(entry)) => paths.push(entry.path()),
            Ok(None) => break,
            Err(error) => {
                warnings.push(format!("Cannot scan skills at {}: {error}", root.display()));
                return;
            }
        }
    }
    paths.sort();
    for path in paths {
        match load_skill(&path).await {
            Ok(entry) => {
                entries.insert(entry.name.clone(), entry);
            }
            Err(error) => warnings.push(format!("Skipping skill at {}: {error}", path.display())),
        }
    }
}

async fn load_skill(path: &Path) -> Result<SkillEntry, String> {
    let metadata = fs::metadata(path)
        .await
        .map_err(|error| error.to_string())?;
    if !metadata.is_dir() {
        return Err("entry is not a directory".to_owned());
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
        .ok_or_else(|| "skill directory name is invalid".to_owned())?
        .to_owned();
    let root = fs::canonicalize(path)
        .await
        .map_err(|error| error.to_string())?;
    let instruction_path = fs::canonicalize(path.join("SKILL.md"))
        .await
        .map_err(|error| error.to_string())?;
    if !instruction_path.starts_with(&root) {
        return Err("SKILL.md escapes its skill directory".to_owned());
    }
    let metadata = fs::metadata(&instruction_path)
        .await
        .map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("SKILL.md is not a regular file".to_owned());
    }
    if metadata.len() > MAX_INLINE_BYTES {
        return Err(format!("SKILL.md exceeds {MAX_INLINE_BYTES} bytes"));
    }
    let mut bytes = Vec::new();
    fs::File::open(&instruction_path)
        .await
        .map_err(|error| error.to_string())?
        .take(MAX_INLINE_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_INLINE_BYTES {
        return Err(format!("SKILL.md exceeds {MAX_INLINE_BYTES} bytes"));
    }
    let instructions = String::from_utf8(bytes).map_err(|error| error.to_string())?;
    let description = extract_description(&instructions)?;
    Ok(SkillEntry {
        name,
        description,
        root,
        instructions,
    })
}

#[derive(Deserialize)]
struct Frontmatter {
    description: Option<String>,
}

fn extract_description(instructions: &str) -> Result<String, String> {
    // Normalize only for parsing: the stored instructions retain their original bytes.
    let normalized = instructions.replace("\r\n", "\n");
    let instructions = normalized.as_str();
    let body = if let Some(rest) = instructions.strip_prefix("---\n") {
        let (frontmatter, body) = rest
            .split_once("\n---\n")
            .or_else(|| {
                rest.strip_suffix("\n---")
                    .map(|frontmatter| (frontmatter, ""))
            })
            .ok_or_else(|| "unterminated YAML frontmatter".to_owned())?;
        let parsed: Frontmatter = serde_yaml::from_str(frontmatter)
            .map_err(|error| format!("invalid YAML frontmatter: {error}"))?;
        if let Some(description) = parsed.description.filter(|value| !value.trim().is_empty()) {
            return Ok(compact_description(description.trim()));
        }
        body
    } else {
        instructions
    };
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(compact_description)
        .ok_or_else(|| "skill has no description or prose".to_owned())
}

fn compact_description(description: &str) -> String {
    description.chars().take(MAX_DESCRIPTION_CHARS).collect()
}

pub(super) fn register(
    builder: &mut ToolRegistryBuilder,
    skills: HostSkills,
    store: SessionStore,
    router: crate::target::TargetRouter,
) -> Result<(), RegistryError> {
    let list = skills.clone();
    builder.register::<NoArgs, Vec<SkillSummary>, _, _>(
        "skills",
        "List host-owned skills available to this agent.",
        ToolOptions::new(vec![Capability::Read]),
        move |_context, _args| {
            let output = list.summaries();
            async move { Ok(output) }
        },
    )?;

    let schema = serde_json::to_value(schemars::schema_for!(SkillArgs))
        .map_err(|error| RegistryError::Schema(error.to_string()))?;
    let output_schema = serde_json::to_value(schemars::schema_for!(SkillOutput))
        .map_err(|error| RegistryError::Schema(error.to_string()))?;
    builder.register_dynamic(
        "skill",
        "Load complete skill instructions and discover assets. Select a relative path to list a directory (use `.` for the root), read text, attach a supported image, or inspect binary metadata. Supply `to` to copy a file into the workspace.",
        schema,
        ToolOptions::new(vec![Capability::Read])
            .output_schema(output_schema)
            .capability_resolver(|arguments| {
                let args: SkillArgs = serde_json::from_value(arguments.clone())
                    .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
                args.validate()?;
                // Copy permissions are scoped to the caller by skill_transfer, not this host tool.
                Ok(vec![Capability::Read])
            }),
        move |context, arguments| {
            let skills = skills.clone();
            let store = store.clone();
            let router = router.clone();
            async move {
                let args: SkillArgs = serde_json::from_value(arguments)
                    .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
                args.validate()?;
                let entry = skills.get(&args.name)?;
                let Some(asset) = args.path else {
                    return skill_output(SkillOutput::Skill {
                        name: entry.name.clone(),
                        description: entry.description.clone(),
                        content: entry.instructions.clone(),
                        assets: asset_tree(&entry.root, true).await?,
                    });
                };
                let source = resolve_asset(entry, &asset).await?;
                let metadata = fs::metadata(&source).await?;
                if metadata.is_dir() && args.to.is_none() {
                    return skill_output(SkillOutput::Directory {
                        name: entry.name.clone(),
                        path: asset,
                        assets: asset_tree(&source, false).await?,
                    });
                }
                if !metadata.is_file() {
                    return Err(ToolError::Failed("skill asset is not a file; directories cannot be copied".to_owned()));
                }
                let maximum = if args.to.is_some() { MAX_COPY_BYTES } else { MAX_COPY_BYTES.max(MAX_IMAGE_BYTES) };
                if metadata.len() > maximum {
                    return Err(ToolError::Failed(format!("skill asset exceeds {maximum} bytes")));
                }
                // Bound the actual read too, in case the asset grows after the metadata check.
                let mut bytes = Vec::new();
                fs::File::open(&source).await?.take(maximum + 1).read_to_end(&mut bytes).await?;
                if bytes.len() as u64 > maximum {
                    return Err(ToolError::Failed(format!("skill asset exceeds {maximum} bytes")));
                }
                if let Some(destination) = args.to {
                    let destination = super::skill_transfer::copy(&router, &context, &destination, &bytes).await?;
                    return skill_output(SkillOutput::Copied {
                        name: entry.name.clone(), path: asset, to: destination,
                        bytes: bytes.len(), sha256: crate::sha256_hex(&bytes),
                    });
                }
                if let Some(media_type) = detect_image(&bytes) {
                    if bytes.len() as u64 > MAX_IMAGE_BYTES {
                        return Err(ToolError::Failed(format!("image exceeds {MAX_IMAGE_BYTES} bytes")));
                    }
                    let name = Path::new(&asset).file_name().and_then(|name| name.to_str())
                        .ok_or_else(|| ToolError::Failed("image filename is not UTF-8".to_owned()))?;
                    let image = store.import_blob(&bytes, name.to_owned(), media_type.to_owned()).await
                        .map_err(|error| ToolError::Failed(error.to_string()))?;
                    return Ok(skill_output(SkillOutput::Image {
                        name: entry.name.clone(), path: asset, image: image.clone(),
                    })?.with_images(vec![image]));
                }
                if let Some(content) = text_content(&bytes) {
                    if bytes.len() as u64 > MAX_INLINE_BYTES {
                        return Err(ToolError::Failed(format!("text skill asset exceeds {MAX_INLINE_BYTES} bytes; use `to` to copy it")));
                    }
                    return skill_output(SkillOutput::Text {
                        name: entry.name.clone(), path: asset,
                        bytes: bytes.len(), content: content.to_owned(),
                    });
                }
                skill_output(SkillOutput::Binary {
                    name: entry.name.clone(), path: asset,
                    bytes: bytes.len(),
                    note: "Binary asset contents are not inlined. Supply `to` with a destination file path to copy this asset into the workspace.".to_owned(),
                })
            }
        },
    )?;
    Ok(())
}

fn skill_output(output: SkillOutput) -> Result<ToolOutput, ToolError> {
    Ok(ToolOutput::new(serde_json::to_value(output)?))
}

fn text_content(bytes: &[u8]) -> Option<&str> {
    if bytes.contains(&0) {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

// Render the complete tree; the output schema, not discovery, controls model-view
// truncation. Keep an explicit stack so deeply nested assets do not recurse.
async fn asset_tree(root: &Path, exclude_instructions: bool) -> Result<String, ToolError> {
    let mut tree = String::new();
    let mut pending = vec![(root.to_path_buf(), String::new(), true, true)];
    while let Some((path, prefix, last, is_root)) = pending.pop() {
        let metadata = fs::symlink_metadata(&path).await?;
        let is_symlink = metadata.file_type().is_symlink();
        let is_directory = metadata.is_dir() && !is_symlink;
        if !is_root {
            tree.push_str(&prefix);
            tree.push_str(if last { "└── " } else { "├── " });
            // Control characters in filenames must not create fake tree lines.
            for character in path.file_name().unwrap().to_string_lossy().chars() {
                if character.is_control() {
                    tree.extend(character.escape_default());
                } else {
                    tree.push(character);
                }
            }
            if is_symlink {
                tree.push_str(" [symlink]");
            } else if is_directory {
                tree.push('/');
            } else if !metadata.is_file() {
                tree.push_str(" [special]");
            }
            tree.push('\n');
        }
        // Never traverse a symlink, including links back to a parent directory.
        if !is_directory {
            continue;
        }
        let mut directory = fs::read_dir(&path).await?;
        let mut children = Vec::new();
        while let Some(child) = directory.next_entry().await? {
            if is_root && exclude_instructions && child.file_name() == "SKILL.md" {
                continue;
            }
            let kind = child.file_type().await?;
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

async fn resolve_asset(entry: &SkillEntry, asset: &str) -> Result<PathBuf, ToolError> {
    let relative = Path::new(asset);
    if asset.trim().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(ToolError::InvalidArguments(
            "asset path must be relative and cannot contain `..`".to_owned(),
        ));
    }
    let source = fs::canonicalize(entry.root.join(relative)).await?;
    if !source.starts_with(&entry.root) {
        return Err(ToolError::Failed(
            "skill asset escapes its skill directory".to_owned(),
        ));
    }
    Ok(source)
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SkillArgs {
    name: String,
    /// Relative file or directory within the skill; `.` lists its root. Omitted or null loads instructions.
    #[serde(default)]
    path: Option<String>,
    /// Destination file path for copying an asset; requires path. Omitted or null reads without copying.
    #[serde(default)]
    to: Option<String>,
}

impl SkillArgs {
    fn validate(&self) -> Result<(), ToolError> {
        for (name, value) in [
            ("name", Some(self.name.as_str())),
            ("path", self.path.as_deref()),
            ("to", self.to.as_deref()),
        ] {
            if value.is_some_and(|value| value.trim().is_empty()) {
                return Err(ToolError::InvalidArguments(format!(
                    "{name} must not be empty"
                )));
            }
        }
        if self.to.is_some() && self.path.is_none() {
            return Err(ToolError::InvalidArguments("to requires path".to_owned()));
        }
        Ok(())
    }
}

#[derive(Serialize, JsonSchema)]
struct SkillSummary {
    name: String,
    description: String,
}

#[derive(Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SkillOutput {
    Skill {
        name: String,
        description: String,
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
        image: ImageReference,
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
    Copied {
        name: String,
        path: String,
        to: String,
        bytes: usize,
        sha256: String,
    },
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{
        identity::AgentId,
        job::JobManager,
        session::SessionStore,
        tool::{ToolRegistryBuilder, executor::ToolExecutor, policy::AllowAll},
    };

    fn register(
        builder: &mut ToolRegistryBuilder,
        skills: HostSkills,
        store: SessionStore,
    ) -> Result<(), RegistryError> {
        let authorization =
            crate::tool::authorization::AuthorizationCoordinator::new(Arc::new(AllowAll));
        let remote = crate::remote::RemoteManager::new(
            crate::remote::EmbeddedShimCatalog::default(),
            Arc::new(crate::remote::RejectSensitivePrompts),
            authorization.clone(),
        );
        let router = crate::target::TargetRouter::new(
            crate::target::TargetRegistry::default(),
            remote,
            authorization,
        );
        super::register(builder, skills, store, router)
    }

    const FIXTURE_TREE: &str = "├── assets/
│   ├── diagram.svg
│   ├── nul.dat
│   ├── payload.bin
│   └── pixel.png
├── references/
│   ├── config.json
│   ├── data.csv
│   ├── empty.txt
│   ├── note.txt
│   └── settings.yaml
└── scripts/
    └── example.py";

    async fn fixture_skills() -> HostSkills {
        let workspace =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/skill-workspace");
        HostSkills::discover_from(&workspace, None).await
    }

    #[tokio::test]
    async fn standard_skill_layout_discovers_directories_and_reads_text() {
        let runtime = crate::test_support::TestRuntime::new().await;
        let skills = fixture_skills().await;
        assert!(skills.warnings().is_empty(), "{:?}", skills.warnings());
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, skills.clone(), runtime.store.clone()).unwrap();
        let registry = builder.build();
        let surface = registry.surface(&Default::default());
        let spec = surface.get("skill").unwrap();
        assert_eq!(spec.input_schema["required"], serde_json::json!(["name"]));
        for argument in ["path", "to"] {
            assert_eq!(
                spec.input_schema["properties"][argument]["default"],
                serde_json::Value::Null
            );
        }
        let executor = ToolExecutor::new(
            registry,
            Arc::new(AllowAll),
            runtime.jobs.clone(),
            runtime.root.path().to_path_buf(),
        );
        let listed = executor
            .execute(runtime.agent.clone(), "skills", serde_json::json!({}), None)
            .await
            .unwrap();
        assert!(
            listed
                .output
                .value
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["name"] == "mixed-assets")
        );
        {
            let args = serde_json::json!({"name":"mixed-assets"});
            let loaded = executor
                .execute(runtime.agent.clone(), "skill", args, None)
                .await
                .unwrap();
            let result = &loaded.output.value;
            assert_eq!(result["kind"], "skill");
            assert_eq!(
                result["content"],
                skills.get("mixed-assets").unwrap().instructions
            );
            assert_eq!(result["assets"], FIXTURE_TREE);
            assert!(result.get("entries").is_none());
        }
        for path in [".", "references", "scripts", "assets"] {
            let loaded = executor
                .execute(
                    runtime.agent.clone(),
                    "skill",
                    serde_json::json!({"name":"mixed-assets", "path":path}),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(loaded.output.value["kind"], "directory");
            assert_eq!(loaded.output.value["path"], path);
            let expected = match path {
                "." => {
                    FIXTURE_TREE
                        .replace("└── scripts/", "├── scripts/")
                        .replace("    └── example.py", "│   └── example.py")
                        + "\n└── SKILL.md"
                }
                "references" => {
                    "├── config.json\n├── data.csv\n├── empty.txt\n├── note.txt\n└── settings.yaml"
                        .to_owned()
                }
                "scripts" => "└── example.py".to_owned(),
                "assets" => {
                    "├── diagram.svg\n├── nul.dat\n├── payload.bin\n└── pixel.png".to_owned()
                }
                _ => unreachable!(),
            };
            assert_eq!(loaded.output.value["assets"], expected);
        }
        for path in [
            "references/note.txt",
            "references/config.json",
            "references/settings.yaml",
            "references/data.csv",
            "references/empty.txt",
            "scripts/example.py",
            "assets/diagram.svg",
        ] {
            let loaded = executor
                .execute(
                    runtime.agent.clone(),
                    "skill",
                    serde_json::json!({"name":"mixed-assets", "path":path}),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(loaded.output.value["kind"], "text", "{path}");
            let expected = fs::read_to_string(skills.get("mixed-assets").unwrap().root.join(path))
                .await
                .unwrap();
            assert_eq!(loaded.output.value["content"], expected);
            assert_eq!(loaded.output.value["bytes"], expected.len());
            assert!(loaded.output.images.is_empty());
        }
    }

    #[tokio::test]
    async fn explicit_null_optional_arguments_read_without_copying() {
        let runtime = crate::test_support::TestRuntime::new().await;
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, fixture_skills().await, runtime.store.clone()).unwrap();
        let executor = runtime.executor(builder);
        for (path, kind) in [(None, "skill"), (Some("references/note.txt"), "text")] {
            let loaded = executor
                .execute(
                    runtime.agent.clone(),
                    "skill",
                    serde_json::json!({"name":"mixed-assets", "path":path, "to":null}),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(loaded.output.value["kind"], kind);
        }
    }

    #[tokio::test]
    async fn image_attachments_and_binary_metadata_and_copy() {
        let runtime = crate::test_support::TestRuntime::new().await;
        let skills = fixture_skills().await;
        let root = skills.get("mixed-assets").unwrap().root.clone();
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, skills, runtime.store.clone()).unwrap();
        let executor = runtime.executor(builder);
        let loaded = executor
            .execute(
                runtime.agent.clone(),
                "skill",
                serde_json::json!({"name":"mixed-assets", "path":"assets/pixel.png"}),
                None,
            )
            .await
            .unwrap();
        let bytes = fs::read(root.join("assets/pixel.png")).await.unwrap();
        assert_eq!(loaded.output.value["kind"], "image");
        assert_eq!(
            loaded.output.value["image"]["sha256"],
            crate::sha256_hex(&bytes)
        );
        assert_eq!(loaded.output.value["image"]["media_type"], "image/png");
        assert_eq!(loaded.output.images.len(), 1);
        assert_eq!(
            serde_json::to_value(&loaded.output.images[0]).unwrap(),
            loaded.output.value["image"]
        );
        assert_eq!(
            runtime.jobs.images(loaded.job).await.unwrap(),
            loaded.output.images
        );
        for path in ["assets/payload.bin", "assets/nul.dat"] {
            let bytes = fs::read(root.join(path)).await.unwrap();
            let loaded = executor
                .execute(
                    runtime.agent.clone(),
                    "skill",
                    serde_json::json!({"name":"mixed-assets", "path":path}),
                    None,
                )
                .await
                .unwrap();
            let result = &loaded.output.value;
            assert_eq!(result["kind"], "binary");
            assert_eq!(result["bytes"], bytes.len());
            assert!(result.get("sha256").is_none());
            assert!(result["note"].as_str().unwrap().contains("`to`"));
            assert!(result.get("content").is_none());
            assert!(loaded.output.images.is_empty());
            let copied = executor
                .execute(
                    runtime.agent.clone(),
                    "skill",
                    serde_json::json!({"name":"mixed-assets", "path":path, "to":"copied.bin"}),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(copied.output.value["kind"], "copied");
            assert_eq!(copied.output.value["sha256"], crate::sha256_hex(&bytes));
            assert_eq!(
                fs::read(runtime.root.path().join("copied.bin"))
                    .await
                    .unwrap(),
                bytes
            );
        }
    }

    #[tokio::test]
    async fn invalid_arguments_and_read_only_copy_never_write() {
        let runtime = crate::test_support::TestRuntime::new().await;
        let skills = fixture_skills().await;
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, skills.clone(), runtime.store.clone()).unwrap();
        let executor = runtime.executor(builder);
        for args in [
            serde_json::json!({}),
            serde_json::json!({"name":""}),
            serde_json::json!({"name":"mixed-assets", "path":""}),
            serde_json::json!({"name":"mixed-assets", "path":"references/note.txt", "to":""}),
            serde_json::json!({"name":"mixed-assets", "to":"must-not-exist"}),
            serde_json::json!({"name":"mixed-assets", "path":"references", "to":"must-not-exist"}),
            serde_json::json!({"name":"mixed-assets", "path":"references/note.txt", "to":"."}),
            serde_json::json!({"name":"mixed-assets", "path":"../mixed-assets/SKILL.md", "to":"must-not-exist"}),
            serde_json::json!({"name":"mixed-assets", "path":"/etc/passwd", "to":"must-not-exist"}),
        ] {
            assert!(
                executor
                    .execute(runtime.agent.clone(), "skill", args.clone(), None)
                    .await
                    .is_err(),
                "{args}"
            );
        }
        assert!(!runtime.root.path().join("must-not-exist").exists());
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, skills, runtime.store.clone()).unwrap();
        let mut capabilities = crate::tool::policy::CapabilitySet::default();
        capabilities.remove(Capability::Write);
        let executor = runtime.executor(builder).with_capabilities(capabilities);
        executor
            .execute(
                runtime.agent.clone(),
                "skill",
                serde_json::json!({"name":"mixed-assets", "path":"assets/payload.bin"}),
                None,
            )
            .await
            .unwrap();
        assert!(executor.execute(runtime.agent.clone(), "skill", serde_json::json!({"name":"mixed-assets", "path":"assets/payload.bin", "to":"must-not-exist"}), None).await.is_err());
        assert!(!runtime.root.path().join("must-not-exist").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_discovery_does_not_allow_asset_escape() {
        let runtime = crate::test_support::TestRuntime::new().await;
        let root = runtime.root.path().join(".agents/skills/demo");
        fs::create_dir_all(&root).await.unwrap();
        fs::write(root.join("SKILL.md"), "# Demo\nSafe instructions.")
            .await
            .unwrap();
        fs::write(runtime.root.path().join("secret"), "outside")
            .await
            .unwrap();
        std::os::unix::fs::symlink(runtime.root.path().join("secret"), root.join("escape"))
            .unwrap();
        std::os::unix::fs::symlink("SKILL.md", root.join("inside")).unwrap();
        std::os::unix::fs::symlink(".", root.join("loop")).unwrap();
        let skills = HostSkills::discover_from(runtime.root.path(), None).await;
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, skills, runtime.store.clone()).unwrap();
        let executor = runtime.executor(builder);
        let listed = executor
            .execute(
                runtime.agent.clone(),
                "skill",
                serde_json::json!({"name":"demo"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            listed.output.value["assets"],
            "├── escape [symlink]\n├── inside [symlink]\n└── loop [symlink]"
        );
        for to in [serde_json::Value::Null, serde_json::json!("must-not-exist")] {
            assert!(
                executor
                    .execute(
                        runtime.agent.clone(),
                        "skill",
                        serde_json::json!({"name":"demo", "path":"escape", "to":to}),
                        None
                    )
                    .await
                    .is_err()
            );
        }
        assert!(!runtime.root.path().join("must-not-exist").exists());
        let loaded = executor
            .execute(
                runtime.agent.clone(),
                "skill",
                serde_json::json!({"name":"demo", "path":"inside"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(loaded.output.value["kind"], "text");
    }

    #[test]
    fn description_prefers_frontmatter_then_prose() {
        assert_eq!(
            extract_description("---\ndescription: concise\n---\n# Name\nBody").unwrap(),
            "concise"
        );
        assert_eq!(
            extract_description("---\r\ndescription: CRLF description\r\n---\r\n# Name\r\nBody")
                .unwrap(),
            "CRLF description"
        );
        assert_eq!(
            extract_description("---\ndescription: EOF delimiter\n---").unwrap(),
            "EOF delimiter"
        );
        assert_eq!(
            extract_description("# Name\n\nFirst paragraph").unwrap(),
            "First paragraph"
        );
    }

    #[tokio::test]
    async fn asset_content_truncates_but_skill_instructions_remain_complete() {
        let runtime = crate::test_support::TestRuntime::new().await;
        let skill = runtime.root.path().join(".agents/skills/demo");
        fs::create_dir_all(&skill).await.unwrap();
        let instructions = "# Demo\n\n".to_owned() + &"Important instruction.\n".repeat(300);
        let asset = "x".repeat(5000);
        fs::write(skill.join("SKILL.md"), &instructions)
            .await
            .unwrap();
        fs::write(skill.join("asset.txt"), &asset).await.unwrap();
        let skills = HostSkills::discover_from(runtime.root.path(), None).await;
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, skills, runtime.store.clone()).unwrap();
        let executor = runtime.executor(builder);

        let loaded = executor
            .execute_model(
                runtime.agent.clone(),
                "skill",
                serde_json::json!({"name":"demo"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(loaded.output.value["result"]["content"], instructions);
        assert!(loaded.output.value.get("truncated").is_none());
        let mut args = crate::job::output::OutputArgs::new(loaded.job);
        args.field = Some("/result/content".into());
        args.start = Some(300);
        let page = runtime
            .jobs
            .present_output(args, &Default::default())
            .await
            .unwrap();
        assert_eq!(page["preview"]["lines"][0], "Important instruction.");
        assert_eq!(page["preview"]["total_lines"], instructions.lines().count());

        let loaded = executor
            .execute_model(
                runtime.agent.clone(),
                "skill",
                serde_json::json!({"name":"demo","path":"asset.txt"}),
                None,
            )
            .await
            .unwrap();
        let view = &loaded.output.value;
        assert_eq!(view["result"]["content"], "x".repeat(2048));
        assert_eq!(view["result"]["name"], "demo");
        assert_eq!(view["result"]["path"], "asset.txt");
        assert_eq!(view["result"]["bytes"], 5000);
        assert_eq!(view["truncated"][0]["field"], "/result/content");
        assert_eq!(
            runtime
                .jobs
                .snapshot(loaded.job)
                .await
                .unwrap()
                .output
                .unwrap()["content"],
            asset
        );
        let mut args = crate::job::output::OutputArgs::new(loaded.job);
        args.field = Some(view["truncated"][0]["field"].as_str().unwrap().into());
        args.start = Some(view["truncated"][0]["next_start"].as_u64().unwrap() as usize);
        args.offset = Some(view["truncated"][0]["next_offset"].as_u64().unwrap_or(0) as usize);
        let page = runtime
            .jobs
            .present_output(args, &Default::default())
            .await
            .unwrap();
        assert_eq!(view["truncated"][0]["next_offset"], 2048);
        assert!(asset[2048..].starts_with(page["preview"]["lines"][0].as_str().unwrap()));
    }

    #[tokio::test]
    async fn complete_recursive_tree_is_saved_and_pageable() {
        let runtime = crate::test_support::TestRuntime::new().await;
        let skill = runtime.root.path().join(".agents/skills/demo");
        let nested = skill.join("references/nested");
        fs::create_dir_all(&nested).await.unwrap();
        fs::write(skill.join("SKILL.md"), "# Demo\nAll instructions.")
            .await
            .unwrap();
        for index in 0..240 {
            fs::write(nested.join(format!("asset-{index:03}.txt")), "")
                .await
                .unwrap();
        }
        // Only the root instructions are excluded, not identically named assets.
        fs::write(nested.join("SKILL.md"), "Nested asset.")
            .await
            .unwrap();
        let skills = HostSkills::discover_from(runtime.root.path(), None).await;
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, skills, runtime.store.clone()).unwrap();
        let executor = runtime.executor(builder);
        for path in [None, Some("references")] {
            let loaded = executor
                .execute_model(
                    runtime.agent.clone(),
                    "skill",
                    serde_json::json!({"name":"demo", "path":path}),
                    None,
                )
                .await
                .unwrap();
            let view = &loaded.output.value;
            let truncated = view["truncated"]
                .as_array()
                .unwrap()
                .iter()
                .find(|field| field["field"] == "/result/assets")
                .unwrap();
            let saved = runtime
                .jobs
                .snapshot(loaded.job)
                .await
                .unwrap()
                .output
                .unwrap();
            let tree = saved["assets"].as_str().unwrap();
            assert!(tree.contains("SKILL.md"));
            assert!(tree.ends_with("└── asset-239.txt"));
            assert_eq!(tree.lines().count(), if path.is_some() { 242 } else { 243 });
            let mut args = crate::job::output::OutputArgs::new(loaded.job);
            args.field = Some("/result/assets".into());
            args.start = Some(truncated["next_start"].as_u64().unwrap() as usize);
            args.offset = Some(truncated["next_offset"].as_u64().unwrap_or(0) as usize);
            let page = runtime
                .jobs
                .present_output(args, &Default::default())
                .await
                .unwrap();
            assert!(!page["preview"]["lines"].as_array().unwrap().is_empty());
            // The final line is addressable independently of the first view's cap.
            let mut args = crate::job::output::OutputArgs::new(loaded.job);
            args.field = Some("/result/assets".into());
            args.start = Some(tree.lines().count());
            let page = runtime
                .jobs
                .present_output(args, &Default::default())
                .await
                .unwrap();
            assert!(
                page["preview"]["lines"][0]
                    .as_str()
                    .unwrap()
                    .ends_with("asset-239.txt")
            );
        }
    }

    #[tokio::test]
    async fn instructions_must_be_a_regular_file() {
        let root = tempfile::tempdir().unwrap();
        let skill = root.path().join("not-a-file");
        fs::create_dir_all(skill.join("SKILL.md")).await.unwrap();
        let error = load_skill(&skill).await.err().unwrap();
        assert_eq!(error, "SKILL.md is not a regular file");
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            fs::remove_dir(skill.join("SKILL.md")).await.unwrap();
            let path =
                std::ffi::CString::new(skill.join("SKILL.md").as_os_str().as_bytes()).unwrap();
            // SAFETY: path is a valid NUL-terminated string for this call.
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
            let error = tokio::time::timeout(std::time::Duration::from_secs(1), load_skill(&skill))
                .await
                .expect("discovery must not open and block on a FIFO")
                .err()
                .unwrap();
            assert_eq!(error, "SKILL.md is not a regular file");
        }
    }

    #[tokio::test]
    async fn global_and_ancestor_discovery_and_crlf_instructions() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("home/.agents/skills");
        let project = root.path().join("project");
        let workspace = project.join("nested/workspace");
        fs::create_dir_all(&workspace).await.unwrap();
        let crlf = "---\r\ndescription: Global description\r\n---\r\n# Global\r\nBody\r\n";
        for (directory, instructions) in [
            (global.join("global-only"), crlf),
            (global.join("common"), "Global common"),
            (project.join(".agents/skills/common"), "Ancestor common"),
            (workspace.join(".agents/skills/local-only"), "Local only"),
        ] {
            fs::create_dir_all(&directory).await.unwrap();
            fs::write(directory.join("SKILL.md"), instructions)
                .await
                .unwrap();
        }
        let skills = HostSkills::discover_from(&workspace, Some(&global)).await;
        assert!(skills.warnings().is_empty());
        assert_eq!(
            skills.get("common").unwrap().instructions,
            "Ancestor common"
        );
        assert_eq!(skills.get("global-only").unwrap().instructions, crlf);
        assert_eq!(
            skills.get("global-only").unwrap().description,
            "Global description"
        );
        assert_eq!(skills.get("local-only").unwrap().instructions, "Local only");
        if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
            assert_eq!(
                user_skills_root().unwrap(),
                PathBuf::from(home).join(".agents/skills")
            );
        }
    }

    #[tokio::test]
    async fn nearest_skill_wins_and_assets_copy_from_the_host() {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("user-skills");
        let outer = root.path().join("project");
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
        }
        let workspace = std::fs::canonicalize(workspace).unwrap();
        let skills = HostSkills::discover_from(&workspace, Some(&user)).await;
        assert!(
            skills
                .get("common")
                .unwrap()
                .instructions
                .contains("nearest")
        );

        let sessions = root.path().join("sessions");
        let store = SessionStore::create(&sessions).await.unwrap();
        let agent = AgentId::root(store.id());
        let jobs = JobManager::new(store.clone());
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, skills, store).unwrap();
        let registry = builder.build();
        let skill_tool = registry.get("skill").unwrap();
        assert_eq!(
            skill_tool
                .capabilities_for(&serde_json::json!({"name":"common"}))
                .unwrap(),
            vec![Capability::Read]
        );
        assert!(
            !skill_tool
                .capabilities_for(
                    &serde_json::json!({"name":"common", "path":"asset.bin", "to":"copied.bin"})
                )
                .unwrap()
                .contains(&Capability::Write)
        );
        let executor = ToolExecutor::new(registry, Arc::new(AllowAll), jobs, workspace.clone());
        let loaded = executor
            .execute(
                agent.clone(),
                "skill",
                serde_json::json!({"name":"common"}),
                None,
            )
            .await
            .unwrap();
        assert!(
            loaded.output.value["content"]
                .as_str()
                .unwrap()
                .contains("nearest")
        );
        let copied = executor
            .execute(
                agent,
                "skill",
                serde_json::json!({"name":"common", "path":"asset.bin", "to":"copied.bin"}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(copied.output.value["bytes"], 7);
        assert_eq!(
            std::fs::read(workspace.join("copied.bin")).unwrap(),
            b"nearest"
        );
    }
}
