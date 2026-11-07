use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::db::params;

use super::*;

/// An observation about one node
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewEvidence {
    pub location: Option<Location>,
    pub environment: Option<EnvironmentId>,
    pub stance: Stance,
    pub note: Text,
}

impl Store {
    /// Record evidence about `node`, or the node it was merged into, as one event.
    pub fn evidence(
        &self,
        actor: &AgentId,
        node: NodeId,
        items: &[NewEvidence],
    ) -> Result<Vec<EvidenceId>> {
        if items.len() > MAX_ATTACHMENTS {
            return Err(Error::Limit(Bound::Attachments));
        }
        self.write(|db| {
            let (node, _) = live_node(db, node)?;
            audit(db, actor, EventKind::Evidence, None, |event| {
                let mut ids = Vec::with_capacity(items.len());
                let mut effect = EventEffect::Concerns;
                for item in items {
                    let location = (item.location.as_ref())
                        .map(|at| location_id(db, at))
                        .transpose()?;
                    if let Some(environment) = item.environment {
                        known_environment(db, environment)?;
                    }
                    ids.push(EvidenceId(db.insert(
                        "INSERT INTO evidence(node,location,environment,stance,note,event) VALUES \
                         (?1,?2,?3,?4,?5,?6)",
                        params![
                            node,
                            location,
                            item.environment,
                            item.stance,
                            &item.note,
                            event
                        ],
                    )?));
                    if item.stance == Stance::Contradicts {
                        effect = EventEffect::Changed;
                    }
                }
                Ok((ids, Marks::of(node, effect)))
            })
        })
    }
}
