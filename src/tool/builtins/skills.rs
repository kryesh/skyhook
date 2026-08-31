use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::fs;

use super::workspace::{atomic_write, resolve_writable};
use crate::tool::{RegistryError, ToolError, ToolOutput, ToolRegistryBuilder, policy::ToolEffect};

const MAX_INLINE_BYTES: u64 = 1024 * 1024;
const MAX_COPY_BYTES: u64 = 8 * 1024 * 1024;
const MAX_DESCRIPTION_CHARS: usize = 512;

#[derive(Clone, Default)]
pub struct HostSkills {
    entries: Arc<BTreeMap<String, SkillEntry>>,
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
        if let Some(user_root) = user_root {
            scan_root(user_root, &mut entries).await;
        }
        let mut ancestors = workspace.ancestors().collect::<Vec<_>>();
        ancestors.reverse();
        for ancestor in ancestors {
            scan_root(&ancestor.join(".agents/skills"), &mut entries).await;
        }
        Self {
            entries: Arc::new(entries),
        }
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
    if let Some(root) = std::env::var_os("XDG_CONFIG_HOME").filter(|root| !root.is_empty()) {
        return Some(PathBuf::from(root).join("skyhook/.agents/skills"));
    }
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".config/skyhook/.agents/skills"))
}

async fn scan_root(root: &Path, entries: &mut BTreeMap<String, SkillEntry>) {
    let mut directory = match fs::read_dir(root).await {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            eprintln!("skyhook: cannot scan skills at {}: {error}", root.display());
            return;
        }
    };
    let mut paths = Vec::new();
    loop {
        match directory.next_entry().await {
            Ok(Some(entry)) => paths.push(entry.path()),
            Ok(None) => break,
            Err(error) => {
                eprintln!("skyhook: cannot scan skills at {}: {error}", root.display());
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
            Err(error) => eprintln!("skyhook: skipping skill at {}: {error}", path.display()),
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
    if metadata.len() > MAX_INLINE_BYTES {
        return Err(format!("SKILL.md exceeds {MAX_INLINE_BYTES} bytes"));
    }
    let instructions = fs::read_to_string(&instruction_path)
        .await
        .map_err(|error| error.to_string())?;
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
    let body = if let Some(rest) = instructions.strip_prefix("---\n") {
        let (frontmatter, body) = rest
            .split_once("\n---\n")
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
) -> Result<(), RegistryError> {
    let list = skills.clone();
    builder.register::<NoArgs, Vec<SkillSummary>, _, _>(
        "skills",
        "List host-owned skills available to this agent.",
        vec![ToolEffect::ReadHostResource],
        false,
        false,
        move |_context, _args| {
            let output = list.summaries();
            async move { Ok(output) }
        },
    )?;

    let schema = serde_json::to_value(schema_for!(SkillArgs))
        .map_err(|error| RegistryError::Schema(error.to_string()))?;
    builder.register_dynamic_effects(
        "skill",
        "Load a host-owned skill or read/copy one of its assets.",
        schema,
        vec![ToolEffect::ReadHostResource],
        |arguments| {
            let args: SkillArgs = serde_json::from_value(arguments.clone())
                .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
            let mut effects = vec![ToolEffect::ReadHostResource];
            if args.to.is_some() {
                effects.push(ToolEffect::WriteWorkspace);
            }
            Ok(effects)
        },
        false,
        false,
        move |context, arguments| {
            let skills = skills.clone();
            async move {
                let args: SkillArgs = serde_json::from_value(arguments)
                    .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
                let entry = skills.get(&args.name)?;
                let Some(asset) = args.path else {
                    if args.to.is_some() {
                        return Err(ToolError::InvalidArguments("to requires path".to_owned()));
                    }
                    return Ok(ToolOutput::new(serde_json::json!({
                        "name": entry.name,
                        "description": entry.description,
                        "content": entry.instructions,
                    })));
                };
                let source = resolve_asset(entry, &asset).await?;
                let metadata = fs::metadata(&source).await?;
                if !metadata.is_file() {
                    return Err(ToolError::Failed("skill asset is not a file".to_owned()));
                }
                let maximum = if args.to.is_some() {
                    MAX_COPY_BYTES
                } else {
                    MAX_INLINE_BYTES
                };
                if metadata.len() > maximum {
                    return Err(ToolError::Failed(format!(
                        "skill asset exceeds {maximum} bytes"
                    )));
                }
                let bytes = fs::read(&source).await?;
                if let Some(destination) = args.to {
                    let target = resolve_writable(&context.workspace, &destination).await?;
                    atomic_write(&target, &bytes).await?;
                    return Ok(ToolOutput::new(serde_json::json!({
                        "name": entry.name, "path": asset, "to": destination,
                        "bytes": bytes.len(), "sha256": hex_hash(&bytes),
                    })));
                }
                let content = String::from_utf8(bytes).map_err(|_| {
                    ToolError::Failed("binary skill assets require `to`".to_owned())
                })?;
                Ok(ToolOutput::new(serde_json::json!({
                    "name": entry.name, "path": asset, "content": content,
                    "bytes": content.len(),
                })))
            }
        },
    )?;
    Ok(())
}

async fn resolve_asset(entry: &SkillEntry, asset: &str) -> Result<PathBuf, ToolError> {
    let relative = Path::new(asset);
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
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

fn hex_hash(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to a string cannot fail");
            output
        })
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SkillArgs {
    name: String,
    path: Option<String>,
    to: Option<String>,
}

#[derive(Serialize)]
struct SkillSummary {
    name: String,
    description: String,
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

    #[test]
    fn description_prefers_frontmatter_then_prose() {
        assert_eq!(
            extract_description("---\ndescription: concise\n---\n# Name\nBody").unwrap(),
            "concise"
        );
        assert_eq!(
            extract_description("# Name\n\nFirst paragraph").unwrap(),
            "First paragraph"
        );
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
        let jobs = JobManager::new(store);
        let mut builder = ToolRegistryBuilder::default();
        register(&mut builder, skills).unwrap();
        let registry = builder.build();
        let skill_tool = registry.get("skill").unwrap();
        assert_eq!(
            skill_tool
                .effects_for(&serde_json::json!({"name":"common"}))
                .unwrap(),
            vec![ToolEffect::ReadHostResource]
        );
        assert!(
            skill_tool
                .effects_for(
                    &serde_json::json!({"name":"common", "path":"asset.bin", "to":"copied.bin"})
                )
                .unwrap()
                .contains(&ToolEffect::WriteWorkspace)
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
