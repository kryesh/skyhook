use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::fs;

use super::workspace::{atomic_write, resolve_writable};
use crate::tool::{
    PathKind, RegistryError, ToolError, ToolOptions, ToolRegistryBuilder,
    policy::{Capability, PathAccess},
};

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
    crate::config::user_config_directory().map(|root| root.join(".agents/skills"))
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
        ToolOptions::new(vec![Capability::Read]),
        move |_context, _args| {
            let output = list.summaries();
            async move { Ok(output) }
        },
    )?;

    builder.register_capability_resolver::<SkillArgs, SkillOutput, _, _, _>(
        "skill",
        "Load a host-owned skill or read/copy one of its assets.",
        ToolOptions::new(vec![Capability::Read]).path_argument(
            "to",
            PathAccess::Write,
            PathKind::Writable,
        ),
        |args| {
            let mut effects = vec![Capability::Read];
            if args.to.is_some() {
                effects.push(Capability::Write);
            }
            effects
        },
        move |context, args| {
            let skills = skills.clone();
            async move {
                let entry = skills.get(&args.name)?;
                let Some(asset) = args.path else {
                    if args.to.is_some() {
                        return Err(ToolError::InvalidArguments("to requires path".to_owned()));
                    }
                    return Ok(SkillOutput::Instructions {
                        name: entry.name.clone(),
                        description: entry.description.clone(),
                        content: entry.instructions.clone(),
                    });
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
                    let target =
                        resolve_writable(&context.execution_location.workspace, &destination)
                            .await?;
                    atomic_write(&target, &bytes).await?;
                    return Ok(SkillOutput::Copied {
                        name: entry.name.clone(),
                        path: asset,
                        to: destination,
                        bytes: bytes.len(),
                        sha256: crate::sha256_hex(&bytes),
                    });
                }
                let content = String::from_utf8(bytes).map_err(|_| {
                    ToolError::Failed("binary skill assets require `to`".to_owned())
                })?;
                Ok(SkillOutput::Asset {
                    name: entry.name.clone(),
                    path: asset,
                    bytes: content.len(),
                    content,
                })
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

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SkillArgs {
    name: String,
    /// Asset path within the named skill; omitted loads its instructions.
    path: Option<String>,
    /// Destination path for copying the selected asset instead of returning its content.
    to: Option<String>,
}

#[derive(Serialize, JsonSchema)]
struct SkillSummary {
    name: String,
    description: String,
}

#[derive(Serialize, JsonSchema)]
#[serde(untagged)]
enum SkillOutput {
    Instructions {
        name: String,
        description: String,
        content: String,
    },
    Asset {
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
        register(&mut builder, skills).unwrap();
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
        args.cursor = Some(view["truncated"][0]["next"].as_str().unwrap().into());
        let page = runtime
            .jobs
            .present_output(args, &Default::default())
            .await
            .unwrap();
        assert_eq!(page["preview"]["lines"][0]["offset"], 2048);
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
                .capabilities_for(&serde_json::json!({"name":"common"}))
                .unwrap(),
            vec![Capability::Read]
        );
        assert!(
            skill_tool
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
