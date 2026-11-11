//! What a configuration's parts say of each other: target routes, and the host capabilities
//! the running product declares. One run decodes a document and checks these against
//! whatever decoded, so each refusal is reported where it is written, beside every value
//! the decoding refused. The default mode and model are checked when the configuration is
//! sealed, after its host's edits.

use std::collections::BTreeMap;

use indexmap::IndexMap;
use skyhook_base::decode::{Context, Decode, Diagnostics, Expected, Node, Path, Segment, Value};

use super::Config;
use super::mode::{Mode, ModeName};
use crate::mcp::config::McpServerConfig;
use crate::tool::policy::{HostCapabilityDeclaration, UnknownCapability, undeclared};

/// A refused setting of a configuration, by its path from the document root.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{path}`: {message}")]
pub struct Refused {
    pub path: Path,
    pub message: String,
    /// The item of the list at `path` that is refused, where the list orders its items
    /// otherwise than written.
    pub item: Option<String>,
    /// What the refused value names, whose own refusal makes this one moot.
    pub about: Option<Vec<Segment>>,
}

impl Refused {
    pub fn new(path: Vec<Segment>, message: impl std::fmt::Display) -> Self {
        Self {
            path: Path(path),
            message: message.to_string(),
            item: None,
            about: None,
        }
    }

    /// The refusal of a name of what `about` holds.
    pub(crate) fn about(self, about: Vec<Segment>) -> Self {
        Self {
            about: Some(about),
            ..self
        }
    }
}

fn key(key: &str) -> Segment {
    Segment::Key(key.to_owned())
}

/// Decode `root` as `T`, then refuse what `check` refuses of whatever decoded, each where
/// it is written. A refusal of a name whose subject was itself refused is left out: the
/// subject's own refusal says why.
pub fn process<T: Decode>(
    root: &Node,
    check: impl FnOnce(&T) -> Vec<Refused>,
) -> Result<T, Diagnostics> {
    let mut cx = Context::default();
    let value = T::decode(root, &mut cx);
    if let Some(value) = &value {
        let refusals = check(value).into_iter();
        let moot = |refused: &Refused| {
            (refused.about.as_deref()).is_some_and(|about| cx.refused_at(about))
        };
        let refusals: Vec<_> = refusals.filter(|refused| !moot(refused)).collect();
        for refused in refusals {
            report(root, &mut cx, refused);
        }
    }
    cx.finish(value)
}

/// `refused` reported at the value it refuses, or the nearest one written.
fn report(root: &Node, cx: &mut Context, refused: Refused) {
    let mut path = refused.path.0;
    let item = refused
        .item
        .as_deref()
        .and_then(|item| match &root.locate(&path).value {
            Value::Seq(items) => items.iter().position(|node| node.as_str() == Some(item)),
            _ => None,
        });
    path.extend(item.map(Segment::Index));
    cx.error_within(root, path, refused.message);
}

/// Every capability a mode or an MCP server lists that `declared` does not, refused at the
/// grant.
pub fn undeclared_capabilities(
    modes: &IndexMap<ModeName, Mode>,
    mcp: &BTreeMap<String, McpServerConfig>,
    declared: &[HostCapabilityDeclaration],
) -> Vec<Refused> {
    let names = || declared.iter().map(|declaration| declaration.name.clone());
    let modes = (modes.iter()).map(|(name, mode)| {
        let capabilities = Vec::from_iter(mode.capabilities.iter());
        ("modes", name.as_str(), capabilities)
    });
    let mcp = (mcp.iter())
        .map(|(name, server)| ("mcp", name.as_str(), Vec::from_iter(server.capabilities())));
    let mut refusals = Vec::new();
    for (section, name, capabilities) in modes.chain(mcp) {
        let path = vec![key(section), key(name), key("capabilities")];
        let known = |name: &_| names().any(|declared| declared == *name);
        for name in undeclared(capabilities, known) {
            let error = UnknownCapability {
                name: name.to_string(),
                declared: names().collect(),
            };
            refusals.push(Refused {
                item: Some(name.to_string()),
                ..Refused::new(path.clone(), error)
            });
        }
    }
    refusals
}

impl Config {
    /// Everything the configuration's parts refuse of each other, and with `declared`, the
    /// host capabilities it grants that the running product does not declare. Only a
    /// `complete` configuration refuses a route through a target it does not define: a
    /// layer may name what a later one defines.
    pub fn refusals(
        &self,
        declared: Option<&[HostCapabilityDeclaration]>,
        complete: bool,
    ) -> Vec<Refused> {
        let mut refusals = self.targets.refusals(complete);
        if let Some(declared) = declared {
            let undeclared = undeclared_capabilities(&self.modes, &self.mcp, declared);
            refusals.extend(undeclared);
        }
        refusals
    }

    /// What sealing refuses of the configuration's parts, the mode and model new sessions
    /// start with among them; else where the default model is declared, by provider and
    /// model. A host edits those until it seals, so no document is refused for them.
    pub(crate) fn sealable(&self) -> Result<Option<(usize, usize)>, Refused> {
        if let Some(refused) = self.refusals(None, true).into_iter().next() {
            return Err(refused);
        }
        if !self.modes.contains_key(&self.default_mode) {
            let declared = self.declared_modes();
            let message = format!("default_mode is not a declared mode, {declared}");
            return Err(Refused::new(vec![key("default_mode")], message));
        }
        let Some(model) = &self.default_model else {
            return Ok(None);
        };
        let selected = self.selected(model).map(|(index, _)| Some(index));
        selected.map_err(|error| {
            let message = format!("{error}, {}", self.configured_models());
            Refused::new(vec![key("default_model")], message)
        })
    }

    /// The configured models, as a refused model name lists them.
    pub(crate) fn configured_models(&self) -> Expected<crate::provider::model::ModelRef> {
        Expected(self.models().map(|(model, _)| model).collect())
    }

    /// The declared modes, as a refused mode name lists them.
    pub(crate) fn declared_modes(&self) -> Expected<super::mode::ModeName> {
        Expected(self.modes.keys().cloned().collect())
    }
}
