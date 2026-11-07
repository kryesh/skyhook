//! Prompt text: embedded Tera templates, each checked at registration to read only the
//! names its context defines. Every template renders from one agent's context: the
//! library's for its system prompt, and a host's, beside them, for its persona and its
//! modes' instructions, with the host's section of that context.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    marker::PhantomData,
    sync::{Arc, LazyLock},
};

use serde::Serialize;

use crate::{
    newtype::{Blank, Prose, nonblank, string_newtype},
    session::cell,
    target::{TargetDefinition, TargetRef, TimeZone},
    tool::policy::{Capability, CapabilitySet, Edition, Instructions, Mode, ModeName},
};

string_newtype! {
    /// The name a template is registered and included under, such as `skyhook/system.md`.
    pub struct TemplateName(Blank) = |name| nonblank("template name", name);
}

/// A template's text as embedded. Its final newline is not part of it, so an include
/// can sit mid-line.
pub enum Source {
    Template(&'static str),
    /// Included as written and never parsed, such as a script shown to the model, which
    /// stays valid in its own language.
    Verbatim(&'static str),
}

#[derive(Debug, thiserror::Error)]
#[error("prompt template: {0}")]
pub struct TemplateError(Box<tera::Error>);

impl From<tera::Error> for TemplateError {
    fn from(error: tera::Error) -> Self {
        Self(Box::new(error))
    }
}

/// Each template `name` under `prefix/`, from its file in the invoking crate's `src/prompts`:
/// a Tera template's file name adds `.tera`, a verbatim one's is the name.
#[macro_export]
macro_rules! templates {
    ($prefix:literal: $($kind:ident $name:literal),* $(,)?) => {
        [$((
            concat!($prefix, "/", $name),
            $crate::prompts::Source::$kind(include_str!($crate::templates!(@$kind $name))),
        )),*]
    };
    (@Template $name:literal) => { concat!("prompts/", $name, ".tera") };
    (@Verbatim $name:literal) => { concat!("prompts/", $name) };
}

/// The library's templates, and a host's beside them, which read the fields of its
/// section `H` as top-level names.
pub struct Templates<H> {
    tera: Arc<tera::Tera>,
    section: PhantomData<fn(&H)>,
}

impl<H: Serialize> Templates<H> {
    /// The library's templates with the host's `sources` under their names. A template
    /// reading a top-level name its context does not define is refused, with the names its
    /// includes read: only the system prompt reads the parts placed around an agent's
    /// context, and only a host's templates read `sample`'s fields.
    pub fn with(
        sources: impl IntoIterator<Item = (&'static str, Source)>,
        sample: &H,
    ) -> Result<Self, TemplateError> {
        let section = tera::Context::from_serialize(sample)?;
        let capabilities = CapabilitySet::default();
        let agent = Agent {
            depth: 0,
            available_depth: 0,
            capabilities: &capabilities,
            mode: None,
            workspace: std::path::Path::new(""),
            target: &TargetRef::Root,
            definition: None,
            zone: None,
        };
        let system = System::new(&agent, None, None, &[]);
        let agent = tera::Context::from_serialize(&system.agent)?;
        let system = tera::Context::from_serialize(&system)?;
        let library: BTreeSet<_> = LIBRARY.iter().map(|(name, _)| *name).collect();
        let tera = registered(
            LIBRARY.into_iter().chain(sources),
            |template, read| match template {
                SYSTEM => system.contains_key(read),
                _ if library.contains(template) => agent.contains_key(read),
                _ => agent.contains_key(read) || section.contains_key(read),
            },
        )?;
        Ok(Self {
            tera: Arc::new(tera),
            section: PhantomData,
        })
    }
}

/// `sources` under their names, refusing a template that reads a name for which
/// `defined(template, name)` is false.
fn registered(
    sources: impl IntoIterator<Item = (&'static str, Source)>,
    defined: impl Fn(&str, &str) -> bool,
) -> Result<tera::Tera, TemplateError> {
    let mut tera = tera::Tera::default();
    tera.autoescape_on(Vec::<&str>::new());
    tera.register_filter(
        "json",
        |value: tera::Value, _: tera::Kwargs, _: &tera::State| {
            serde_json::to_string(&value).map_err(tera::Error::message)
        },
    );
    // Verbatim text reaches the render as a value, so the parser never sees it; its
    // template only outputs that value, keeping it includable by name.
    let mut verbatim = BTreeMap::new();
    let sources: Vec<_> = (sources.into_iter())
        .map(|(name, source)| match source {
            Source::Template(text) => (name, text.strip_suffix('\n').unwrap_or(text).to_owned()),
            Source::Verbatim(text) => {
                verbatim.insert(name, text.strip_suffix('\n').unwrap_or(text));
                (name, format!("{{{{ {VERBATIM}[{name:?}] }}}}"))
            }
        })
        .collect();
    tera.global_context().insert(VERBATIM, &verbatim);
    tera.add_raw_templates(sources)?;
    for name in tera.get_template_names() {
        let read = tera.get_template_variables(name)?;
        let defined = |read: &&str| *read == VERBATIM || defined(name, read);
        if let Some(undefined) = read.iter().find(|read| !defined(read)) {
            let message = format!("template `{name}` reads undefined `{undefined}`");
            return Err(tera::Error::message(message).into());
        }
    }
    Ok(tera)
}

const LIBRARY: [(&str, Source); 9] = crate::templates!["skyhook":
    Template "system.md",
    Template "root.md",
    Template "child.md",
    Template "tools.md",
    Template "workspace.md",
    Template "todo.md",
    Template "lifecycle.md",
    Template "targets.md",
    Template "agents.md",
];

const SYSTEM: &str = "skyhook/system.md";

/// The global holding every verbatim source's text by name.
const VERBATIM: &str = "verbatim";

/// What renders every agent's prompt in a session: the templates, the host's section of
/// each agent's context, and the host's persona template.
#[derive(Clone)]
pub struct Prompts {
    tera: Arc<tera::Tera>,
    section: tera::Context,
    persona: Option<TemplateName>,
}

impl Default for Prompts {
    /// The library's templates alone.
    fn default() -> Self {
        #[derive(Serialize)]
        struct Library {}
        static LIBRARY: LazyLock<Templates<Library>> =
            LazyLock::new(|| Templates::with([], &Library {}).expect("library templates parse"));
        Self {
            tera: LIBRARY.tera.clone(),
            section: tera::Context::new(),
            persona: None,
        }
    }
}

impl Prompts {
    /// The host's `templates`, with `section` in every agent's context and its `persona`
    /// template placed after each agent's role.
    pub fn host<H: Serialize>(
        templates: &Templates<H>,
        section: &H,
        persona: TemplateName,
    ) -> Result<Self, TemplateError> {
        Ok(Self {
            tera: templates.tera.clone(),
            section: tera::Context::from_serialize(section)?,
            persona: Some(persona),
        })
    }

    /// `mode` as `agent`, entering it, runs it: a template instructing it rendered for it.
    pub fn edition(&self, mode: &Mode, agent: &Agent<'_>) -> Result<Edition, TemplateError> {
        let instructions = match &mode.instructions {
            Some(Instructions::Text(text)) => Some(text.clone()),
            Some(Instructions::Template(name)) => {
                let text = self.render(name.as_str(), agent)?;
                let blank = |_| tera::Error::message(format!("template `{name}` renders blank"));
                Some(text.parse().map_err(blank)?)
            }
            None => None,
        };
        Ok(Edition {
            capabilities: mode.capabilities.clone(),
            instructions,
            hint: mode.hint.clone(),
        })
    }

    /// The system prompt of `agent`: the library's parts with the persona rendered for it,
    /// its mode's `instructions` and the workspace's `files`, as written.
    pub(crate) fn system(
        &self,
        agent: &Agent<'_>,
        instructions: Option<&Prose>,
        files: &[String],
    ) -> Result<String, TemplateError> {
        let persona = self.persona.as_ref();
        let persona = persona
            .map(|name| self.render(name.as_str(), agent))
            .transpose()?;
        let system = System::new(agent, persona.as_deref(), instructions, files);
        let context = tera::Context::from_serialize(&system)?;
        Ok(self.tera.render(SYSTEM, &context)?)
    }

    /// Template `name` rendered from `agent`'s context with the host's section.
    fn render(&self, name: &str, agent: &Agent<'_>) -> Result<String, TemplateError> {
        let mut context = tera::Context::from_serialize(&PromptContext::new(agent))?;
        context.extend(self.section.clone());
        Ok(self.tera.render(name, &context)?)
    }
}

/// An agent's contract as its prompt describes it.
pub struct Agent<'a> {
    pub depth: usize,
    pub available_depth: usize,
    pub capabilities: &'a CapabilitySet,
    pub mode: Option<&'a ModeName>,
    pub workspace: &'a std::path::Path,
    pub target: &'a TargetRef,
    /// The target's definition; None on the session host.
    pub definition: Option<TargetDefinition>,
    /// The target's time zone, when it is known.
    pub zone: Option<TimeZone>,
}

/// What the system prompt renders from: the agent's context and the parts around it.
#[derive(Serialize)]
struct System {
    #[serde(flatten)]
    agent: PromptContext,
    persona: Option<String>,
    mode_instructions: Option<String>,
    instructions: Vec<String>,
}

impl System {
    fn new(
        agent: &Agent<'_>,
        persona: Option<&str>,
        instructions: Option<&Prose>,
        files: &[String],
    ) -> Self {
        Self {
            agent: PromptContext::new(agent),
            persona: persona.map(|text| text.trim().to_owned()),
            mode_instructions: instructions.map(|text| text.as_str().trim().to_owned()),
            instructions: files.to_vec(),
        }
    }
}

/// Whether an agent converses with the user, as a root that can ask them, or works an
/// assigned task to its result, as a child or as a root no one answers.
#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum Role {
    Root,
    Child,
}

/// What every library template may read.
#[derive(Serialize)]
struct PromptContext {
    role: Role,
    /// How deep its children may delegate.
    available_depth: usize,
    /// Every built-in capability by name, whether granted.
    capabilities: BTreeMap<&'static str, bool>,
    mode: Option<ModeName>,
    /// Its `context` lines: the workspace, the target where it may choose one,
    /// and the session host's time zone when it is known.
    context: String,
    /// The type every direct tool result follows.
    job_view: &'static str,
}

impl PromptContext {
    fn new(agent: &Agent<'_>) -> Self {
        let definition = agent.definition.as_ref();
        let mut context = format!("workspace:{}", cell(&agent.workspace.to_string_lossy()));
        if let Some(name) = agent.capabilities.visible_target(agent.target) {
            let host = definition.map_or("localhost", |target| &target.host);
            write!(
                context,
                "\ntarget:{} host={}",
                cell(&name.to_string()),
                cell(host)
            )
            .expect("strings take writes");
            let routes =
                definition.map(|target| [("via", &target.via), ("origin", &target.origin)]);
            for (key, route) in routes.into_iter().flatten() {
                if let Some(route) = route {
                    write!(context, " {key}={}", cell(route.as_str()))
                        .expect("strings take writes");
                }
            }
        }
        if let Some(zone) = &agent.zone {
            let name = zone.name.as_deref().map(|name| format!("{} ", cell(name)));
            write!(
                context,
                "\ntz:{}{}",
                name.unwrap_or_default(),
                zone.offset()
            )
            .expect("strings take writes");
        }
        let granted = |capability: &'static Capability| {
            (capability.as_str(), agent.capabilities.contains(capability))
        };
        Self {
            role: match agent.depth {
                0 if agent.capabilities.contains(&Capability::Interactive) => Role::Root,
                _ => Role::Child,
            },
            available_depth: agent.available_depth,
            capabilities: Capability::ALL.iter().map(granted).collect(),
            mode: agent.mode.cloned(),
            context,
            job_view: crate::tool::job_view_type(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Host {
        live: bool,
    }

    /// A root on the session host holding `capabilities`, in no mode.
    fn root(capabilities: &CapabilitySet) -> Agent<'_> {
        Agent {
            depth: 0,
            available_depth: 0,
            capabilities,
            mode: None,
            workspace: std::path::Path::new("/workspace"),
            target: &TargetRef::Root,
            definition: None,
            zone: None,
        }
    }

    /// A bare root's prompt carries no capability-gated fragment and none of the parts it
    /// is not given, and works its task as a child does until it can ask the user; a child
    /// given every part carries the fragments its grants open, and its persona, its mode's
    /// instructions as they render for it, its instruction file and its target, in order.
    #[test]
    fn system_prompts_carry_the_parts_their_agent_is_given() {
        let gated = ["skyhook/targets.md", "skyhook/agents.md"];
        let inputs = ["Persona text.", "Mode text.", "Instruction file.", "jump"];

        let read = [Capability::Read].into_iter().collect();
        let bare = root(&read);
        let library = Prompts::default();
        let text = library.system(&bare, None, &[]).unwrap();
        let fragments = gated.map(|name| library.render(name, &bare).unwrap());
        for absent in fragments.iter().map(String::as_str).chain(inputs) {
            assert!(!text.contains(absent), "{absent}");
        }
        assert!(text.starts_with(&library.render("skyhook/child.md", &bare).unwrap()));
        let interactive = [Capability::Read, Capability::Interactive]
            .into_iter()
            .collect();
        let asking = root(&interactive);
        let text = library.system(&asking, None, &[]).unwrap();
        assert!(text.starts_with(&library.render("skyhook/root.md", &asking).unwrap()));

        let sources = [
            ("host/persona.md", Source::Template("Persona text.")),
            (
                "host/look.md",
                Source::Template("{% if live and capabilities.agents %}Mode text.{% endif %}"),
            ),
        ];
        let host = Host { live: true };
        let templates = Templates::with(sources, &host).unwrap();
        let prompts = Prompts::host(&templates, &host, "host/persona.md".parse().unwrap()).unwrap();
        let target = TargetDefinition {
            name: "box".parse().unwrap(),
            host: "box.example".into(),
            ssh: Default::default(),
            workspace: "/remote".into(),
            via: Some("jump".parse().unwrap()),
            origin: None,
            source: crate::target::TargetSource::Config,
            revision: 1,
        };
        let capabilities = [Capability::Read, Capability::Targets, Capability::Agents];
        let capabilities: CapabilitySet = capabilities.into_iter().collect();
        let look = Mode {
            capabilities: capabilities.clone().try_into().unwrap(),
            instructions: Some(Instructions::Template("host/look.md".parse().unwrap())),
            hint: None,
        };
        let child = Agent {
            depth: 1,
            available_depth: 2,
            capabilities: &capabilities,
            mode: Some(&"look".parse().unwrap()),
            workspace: std::path::Path::new("/remote/work"),
            target: &TargetRef::Named(target.name.clone()),
            definition: Some(target.clone()),
            zone: Some(TimeZone {
                name: Some("Europe/Berlin".into()),
                offset: 3600,
            }),
        };
        let edition = prompts.edition(&look, &child).unwrap();
        let files = ["Instruction file.".to_owned()];
        let text = prompts
            .system(&child, edition.instructions.as_ref(), &files)
            .unwrap();
        for name in gated {
            let fragment = prompts.render(name, &child).unwrap();
            assert!(text.contains(&fragment), "{name}");
        }
        let positions = inputs.map(|part| text.find(part).unwrap_or_else(|| panic!("{part}")));
        assert!(positions.is_sorted());
        // The context is plain lines: a visible target's, and its zone when known.
        let block = |lines: &str| format!("<context>\n{lines}\n</context>");
        let remote =
            "workspace:/remote/work\ntarget:box host=box.example via=jump\ntz:Europe/Berlin +01:00";
        assert!(text.ends_with(&block(remote)));
        let bare = library.system(&bare, None, &[]).unwrap();
        assert!(bare.ends_with(&block("workspace:/workspace")));
    }

    /// A template may read only the names its context defines, those its includes read
    /// among them: a host's reads an agent's context and its section's fields, and only
    /// the system prompt reads the parts placed around an agent's context. Verbatim text
    /// is never parsed.
    #[test]
    fn templates_read_only_their_context_and_verbatim_text_is_never_parsed() {
        let host = |template: &'static str| {
            let including = Source::Template("{% include \"host/b.md\" %}");
            let sources = [
                ("host/a.md", including),
                ("host/b.md", Source::Template(template)),
            ];
            Templates::with(sources, &Host { live: false })
        };
        assert!(host("{% if live and capabilities.agents %}live{% endif %}").is_ok());
        assert!(host("{% if lvie %}live{% endif %}").is_err());
        assert!(host("{{ persona }}").is_err());

        let script = Source::Verbatim("{{ missing }} {% if %} {% endraw %}\n");
        let shown = Source::Template("```js\n{% include \"host/script.js\" %}\n```\n");
        let sources = [("host/script.js", script), ("host/t.md", shown)];
        let host = Host { live: true };
        let templates = Templates::with(sources, &host).unwrap();
        let prompts = Prompts::host(&templates, &host, "host/t.md".parse().unwrap()).unwrap();
        let rendered = prompts.render("host/t.md", &root(&CapabilitySet::default()));
        assert_eq!(
            rendered.unwrap(),
            "```js\n{{ missing }} {% if %} {% endraw %}\n```"
        );
    }
}
