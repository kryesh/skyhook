//! Applying the configured modes: what sending an agent into one changes, the
//! context that change opens, and how the configuration has moved on from the
//! editions a reopened session recorded.

use super::*;

/// A resolved model and mode selection, journaled with the input it applies to.
pub(in crate::agent::runtime) struct Selected {
    pub(super) events: Vec<(AgentId, SessionEvent)>,
    pub(super) replacement: Option<AgentContext>,
    pub(super) mode: Option<ModeChange>,
}

impl Selected {
    /// Install what the journal now records: the replacement context and the
    /// mode with its grants.
    pub(super) fn install(
        self,
        turn: &mut TurnContext<'_>,
        context: &mut AgentContext,
        settings: &mut AgentSettings,
    ) {
        if let Some(replacement) = self.replacement {
            *context = replacement;
        }
        if let Some(ModeChange {
            mode, capabilities, ..
        }) = self.mode
        {
            turn.capabilities.clone_from(&capabilities);
            (settings.mode, settings.capabilities) = (Some(mode), capabilities);
        }
    }
}

/// A mode application that changes what an agent runs: a different mode, a new
/// edition of its own, or different grants under the live configuration.
pub(super) struct ModeChange {
    pub(super) mode: AppliedMode,
    pub(super) depth: usize,
    pub(super) capabilities: CapabilitySet,
}

impl ModeChange {
    pub(super) fn event(&self) -> SessionEvent {
        SessionEvent::ModeChanged {
            mode: self.mode.selection(),
            capabilities: self.capabilities.iter().collect(),
        }
    }
}

impl SessionRuntime {
    /// What sending `agent`, which runs `current` with `capabilities`, into the
    /// configured `name` changes; `None` when it already runs that edition with
    /// what it grants now.
    pub(super) fn mode_change(
        &self,
        agent: &AgentId,
        current: Option<&AppliedMode>,
        capabilities: &CapabilitySet,
        name: &ModeName,
    ) -> Result<Option<ModeChange>, HarnessError> {
        let mode = self.applied(name)?;
        let depth = self.held(agent, |_, depth, _| depth).unwrap_or(0);
        let granted = self.granted_by(&mode.definition).for_agent(depth);
        let unchanged = current == Some(&mode) && *capabilities == granted;
        Ok((!unchanged).then_some(ModeChange {
            mode,
            depth,
            capabilities: granted,
        }))
    }

    /// The mode the root runs next: the one `requested`, else its own as configured
    /// now, or the default mode once the configuration no longer has its own.
    pub(super) fn root_mode(
        &self,
        current: Option<&AppliedMode>,
        requested: Option<ModeName>,
    ) -> Option<ModeName> {
        requested.or_else(|| {
            let current = current?;
            match self.modes.contains_key(&current.name) {
                true => Some(current.name.clone()),
                false => self.harness.mode.clone(),
            }
        })
    }

    /// Whether the root runs a mode the configuration has since changed, which
    /// the next work that runs it adopts.
    pub(super) fn root_outdated(&self, root: &AgentId) -> bool {
        let held = self.held(root, |contract, _, granted| {
            (contract.mode.clone(), granted)
        });
        let Some((current, capabilities)) = held else {
            return false;
        };
        let current = current.as_ref();
        self.root_mode(current, None).is_some_and(|name| {
            let change = self.mode_change(root, current, &capabilities, &name);
            !matches!(change, Ok(None))
        })
    }
}

/// A warning that the configuration redefined or removed the mode a reopened
/// root last ran in, `running`, which the next work that runs it adopts; None
/// when the configuration still defines it so. Once adopted, it warns no more.
pub(super) fn drift_warning(
    running: Option<&AppliedMode>,
    modes: &indexmap::IndexMap<ModeName, Mode>,
    default: Option<&ModeName>,
) -> Option<String> {
    let running = running?;
    let name = &running.name;
    match (modes.get(name), default) {
        (Some(configured), _) if *configured == *running.definition => None,
        (Some(_), _) => Some(format!(
            "The configuration has changed mode `{name}` since this session last ran; the \
             root agent adopts it when it next runs."
        )),
        (None, Some(default)) => Some(format!(
            "The configuration no longer has mode `{name}`, which this session last ran in; \
             the root agent moves to the default mode `{default}` when it next runs."
        )),
        (None, None) => Some(format!(
            "The configuration no longer has mode `{name}`, which this session last ran in; \
             without a default mode, the root agent keeps running it within the current \
             capabilities."
        )),
    }
}
