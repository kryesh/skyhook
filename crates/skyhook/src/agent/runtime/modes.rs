//! Applying the configured modes: what sending an agent into one changes, the
//! context that change opens, and how the configuration has moved on from the
//! model and mode a reopened session recorded.

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
    pub(super) fn install(self, turn: &mut TurnContext<'_>, context: &mut AgentContext) {
        if let Some(replacement) = self.replacement {
            *context = replacement;
        }
        if let Some(ModeChange {
            mode, capabilities, ..
        }) = self.mode
        {
            (turn.settings.mode, turn.settings.capabilities) = (Some(mode), capabilities);
        }
    }
}

/// A mode application that changes what an agent runs: a different mode, a new
/// edition of its own, or different grants under the live configuration.
pub(super) struct ModeChange {
    pub(super) mode: AppliedMode,
    pub(super) depth: usize,
    pub(super) capabilities: CapabilitySet,
    /// The zone the mode rendered in, which the root's new prompt is built in too.
    pub(super) zone: Option<crate::target::TimeZone>,
}

impl ModeChange {
    pub(super) fn event(&self) -> SessionEvent {
        SessionEvent::ModeChanged {
            mode: self.mode.selection(),
            capabilities: self.capabilities.clone(),
        }
    }
}

impl SessionRuntime {
    /// What sending `agent`, which runs `current`, into the configured `name` changes;
    /// `None` when it already runs the edition it renders now with what it grants now.
    /// Its prompt was rendered for the grant last journaled, so a live ceiling that
    /// narrows or widens that grant, as a host without the user does, changes it too.
    pub(super) fn mode_change(
        &self,
        agent: &AgentId,
        current: Option<&AppliedMode>,
        name: &ModeName,
    ) -> Result<Option<ModeChange>, HarnessError> {
        let held = self.store.read(|state| {
            let location = state.agents.journaled(agent)?.contract.location.clone();
            let (depth, _) = self.granted(state, agent)?;
            Some((
                location,
                depth,
                state.agent_states.get(agent)?.capabilities.clone(),
            ))
        });
        let (location, depth, rendered) =
            held.ok_or_else(|| HarnessError::NoRecordedModel(agent.clone()))?;
        // Only a root changes mode, and a root runs on the session host.
        let zone = Some(crate::target::TimeZone::local());
        let (mode, granted) = self.applied(
            name,
            agent.depth(),
            &location,
            depth,
            &self.capabilities,
            zone.clone(),
        )?;
        let unchanged = current == Some(&mode) && rendered == granted;
        Ok((!unchanged).then_some(ModeChange {
            mode,
            depth,
            capabilities: granted,
            zone,
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
            match self.harness.modes.contains_key(&current.name) {
                true => Some(current.name.clone()),
                false => self.harness.mode.clone(),
            }
        })
    }

    /// Whether the root runs a mode the configuration has since changed, which
    /// the next work that runs it adopts, or a model it no longer has, which
    /// refuses that work.
    pub(super) fn root_outdated(&self, root: &AgentId) -> bool {
        let held = self.held(root, |contract, _, _| {
            let gone = !self.harness.models.contains_key(&contract.profile.name);
            (contract.mode.clone(), gone)
        });
        let Some((current, gone)) = held else {
            return false;
        };
        let current = current.as_ref();
        gone || self.root_mode(current, None).is_some_and(|name| {
            let change = self.mode_change(root, current, &name);
            !matches!(change, Ok(None))
        })
    }

    /// Warnings that the configuration has moved on from the selection the reopened
    /// `root` last ran under: it no longer has the model, which another must replace
    /// before the root runs again; its mode renders for it otherwise or is gone, which
    /// the next work that runs the root adopts; or its mode no longer renders for it,
    /// which fails that work. A selection the root has replaced warns no more.
    pub(super) fn drift_warnings(&self, root: &AgentId) -> Vec<String> {
        let held = self.held(root, |contract, _, _| {
            (contract.profile.name.clone(), contract.mode.clone())
        });
        let Some((model, running)) = held else {
            return Vec::new();
        };
        let model = (!self.harness.models.contains_key(&model)).then(|| {
            format!(
                "The configuration no longer has model `{model}`, which this session last ran \
                 on; the root agent runs only once another is chosen."
            )
        });
        let Some(running) = running else {
            return model.into_iter().collect();
        };
        let name = &running.name;
        let mode = match (self.harness.modes.contains_key(name), &self.harness.mode) {
            (true, _) => match self.mode_change(root, Some(&running), name) {
                Ok(change) => change
                    .is_some_and(|change| change.mode != running)
                    .then(|| {
                        format!(
                            "The configuration has changed mode `{name}` since this session last \
                         ran; the root agent adopts it when it next runs."
                        )
                    }),
                Err(error) => Some(format!(
                    "Mode `{name}`, which this session last ran in, no longer renders for the \
                     root agent: {error}. Running the root agent in it fails until it does."
                )),
            },
            (false, Some(default)) => Some(format!(
                "The configuration no longer has mode `{name}`, which this session last ran \
                 in; the root agent moves to the default mode `{default}` when it next runs."
            )),
            (false, None) => Some(format!(
                "The configuration no longer has mode `{name}`, which this session last ran \
                 in; without a default mode, the root agent keeps running it within the \
                 current capabilities."
            )),
        };
        model.into_iter().chain(mode).collect()
    }
}
