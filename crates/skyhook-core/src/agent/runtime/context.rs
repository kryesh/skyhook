//! One agent's context window and independently owned provider conversation.

use std::collections::HashMap;

use super::{HarnessError, compact::TokenMeter};
use crate::{
    agent::ContextUsage,
    identity::AgentId,
    provider::{
        Provider, ProviderContext,
        profile::ModelProfile,
        protocol::{Message, ModelRequest, UserContent},
    },
    session::{EventRecord, SessionEvent, project_history},
};

pub(super) struct AgentContext {
    pub profile: ModelProfile,
    pub template: ModelRequest,
    pub projected: Vec<(u64, Message)>,
    pub meter: TokenMeter,
    pub provider: Box<dyn ProviderContext>,
    checkpoint: Option<u64>,
}

impl AgentContext {
    pub fn open(
        agent: &AgentId,
        profile: ModelProfile,
        template: ModelRequest,
        factory: &dyn Provider,
        records: &[EventRecord],
        restore_meter: bool,
    ) -> Result<Self, HarnessError> {
        let projected = project_history(records, agent)?;
        let provider = factory.open_context(agent.to_string())?;
        let meter = if restore_meter {
            TokenMeter::restore(records, agent, &template)
        } else {
            TokenMeter::default()
        };
        Ok(Self {
            profile,
            template,
            projected,
            meter,
            provider,
            checkpoint: checkpoint(records, agent),
        })
    }

    /// The journal remains authoritative, including compactions committed externally.
    pub fn refresh(
        &mut self,
        records: &[EventRecord],
        agent: &AgentId,
    ) -> Result<(), HarnessError> {
        let projected = project_history(records, agent)?;
        let checkpoint = checkpoint(records, agent);
        if self.checkpoint != checkpoint {
            self.meter = TokenMeter::default();
            self.checkpoint = checkpoint;
        }
        self.projected = projected;
        Ok(())
    }

    pub fn request(&self, runtime: UserContent) -> ModelRequest {
        let mut request = self.template.clone();
        request.messages = self
            .projected
            .iter()
            .map(|(_, message)| message.clone())
            .collect();
        request.messages.push(Message::User(vec![runtime]));
        request
    }

    pub fn needs_compaction(&self, request: &ModelRequest) -> bool {
        self.meter.estimate(request)
            >= self
                .profile
                .max_context
                .saturating_sub(self.profile.max_output)
    }

    pub fn contains_images(&self) -> bool {
        self.projected
            .iter()
            .any(|(_, message)| super::contains_images(std::slice::from_ref(message)))
    }
}

fn checkpoint(records: &[EventRecord], agent: &AgentId) -> Option<u64> {
    records
        .iter()
        .rev()
        .find(|record| {
            &record.agent == agent && matches!(record.event, SessionEvent::Compaction { .. })
        })
        .map(|record| record.sequence)
}

/// Reconstruct context occupancy for historical agents without consulting current config.
pub(in crate::agent) fn recorded_context(
    records: &[EventRecord],
) -> HashMap<AgentId, ContextUsage> {
    let mut contexts = HashMap::new();
    let capacities: HashMap<_, _> = records
        .iter()
        .filter_map(|record| match &record.event {
            SessionEvent::AgentStarted {
                max_context: Some(capacity),
                ..
            }
            | SessionEvent::ModelChanged {
                max_context: capacity,
                ..
            } => Some((&record.agent, *capacity)),
            _ => None,
        })
        .collect();
    for (agent, capacity) in capacities {
        let Some(mut request) = records.iter().rev().find_map(|record| {
            if &record.agent == agent
                && matches!(
                    record.event,
                    SessionEvent::ModelRequested {
                        purpose: crate::session::ModelPurpose::Agent,
                        ..
                    }
                )
            {
                crate::session::reconstruct_model_request(records, record.sequence)
                    .ok()
                    .map(|(_, request)| request)
            } else {
                None
            }
        }) else {
            continue;
        };
        let runtime = request.messages.pop().filter(|message| {
            matches!(message, Message::User(blocks) if blocks.iter().any(|block| matches!(block, UserContent::Runtime { .. })))
        });
        request.messages.clear();
        let meter = TokenMeter::restore(records, agent, &request);
        let mut current = request;
        let Ok(history) = crate::session::project_history(records, agent) else {
            continue;
        };
        current.messages = history.into_iter().map(|(_, message)| message).collect();
        if let Some(message) = runtime {
            current.messages.push(message);
        }
        contexts.insert(
            agent.clone(),
            ContextUsage {
                tokens: meter.estimate(&current),
                capacity,
            },
        );
    }
    contexts
}
