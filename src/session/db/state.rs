//! Current-state queries over the schema's views.

use super::{Db, DbResult, corrupt};
use crate::provider::profile::ModelRef;

/// A session list row, read without decoding the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSummary {
    pub title: Option<String>,
    /// The first text the user sent the root agent.
    pub preview: Option<String>,
    pub last_millis: i64,
    pub entries: u64,
    /// The root agent's applied model.
    pub model: Option<ModelRef>,
    /// The root agent's applied mode.
    pub mode: Option<crate::tool::policy::ModeName>,
}

pub(in crate::session) fn summary(db: &Db) -> DbResult<SessionSummary> {
    db.query_row(
        "SELECT title, preview, coalesce(last_millis, 0), entries, p.provider, p.name, mode \
         FROM session_summary s LEFT JOIN model_profile p ON p.id = s.profile",
        Vec::new(),
        |row| {
            let model = match (row.get::<Option<String>>(4)?, row.get::<Option<String>>(5)?) {
                (Some(provider), Some(name)) => Some(super::decode::model_ref(&provider, &name)?),
                _ => None,
            };
            Ok(SessionSummary {
                title: row.get(0)?,
                preview: row.get(1)?,
                last_millis: row.get(2)?,
                entries: row.get(3)?,
                model,
                mode: row
                    .get::<Option<String>>(6)?
                    .map(super::decode::parsed)
                    .transpose()?,
            })
        },
    )?
    .ok_or_else(|| corrupt("session summary is missing"))
}
