use chrono::{DateTime, Utc};
use libsql::Row;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::db::{Db, enum_column, params};

use super::*;

/// Declares [`EventRecord`] once with, per variant, the column that selects it and the
/// statement that records it, so writing and reading an event's record cannot disagree.
macro_rules! event_records {
    ($($variant:ident($id:ident) = $select:literal, $insert:expr;)*) => {
        /// The record an event describes, read with that record's own action.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "kind", content = "id", rename_all = "snake_case")]
        pub enum EventRecord {
            $($variant($id),)*
        }

        /// The selected record columns, one per variant in declaration order.
        macro_rules! record_columns {
            () => {
                concat!($($select, ",",)*)
            };
        }

        impl EventRecord {
            /// One constructor per selected record column, in [`record_columns!`] order.
            const DECODERS: &[fn(i64) -> Self] = &[$(|id| Self::$variant($id(id))),*];
            const COLUMNS: i32 = Self::DECODERS.len() as i32;

            fn insert(self) -> (&'static str, i64) {
                match self {
                    $(Self::$variant(id) => ($insert, id.0),)*
                }
            }

            /// The one non-null record column from `offset`.
            fn decode(row: &Row, offset: i32) -> Result<Option<Self>> {
                for (column, decode) in (offset..).zip(Self::DECODERS) {
                    if let Some(id) = row.get::<Option<i64>>(column)? {
                        return Ok(Some(decode(id)));
                    }
                }
                Ok(None)
            }
        }
    };
}

macro_rules! reference {
    ($column:literal) => {
        concat!(
            "INSERT INTO event_record(event,",
            $column,
            ") VALUES (?1,?2)"
        )
    };
}

event_records! {
    Scope(ScopeId) = "r.scope", reference!("scope");
    Principal(PrincipalId) = "r.principal", reference!("principal");
    Source(SourceId) = "r.source", reference!("source");
    Component(ComponentId) = "r.component", reference!("component");
    Path(PathId) = "r.path", reference!("path");
    Unit(UnitId) = "r.unit", reference!("unit");
    Outcome(OutcomeId) = "r.outcome", reference!("outcome");
    Guarantee(GuaranteeId) = "r.guarantee", reference!("guarantee");
    RetrySite(RetrySiteId) = "r.retry_site", reference!("retry_site");
    Deferred(DeferredId) = "r.deferred", reference!("deferred");
    Relation(RelationId) = "r.relation", reference!("relation");
    Undermining(UnderminingId) = "r.undermining", reference!("undermining");
    Premise(PremiseId) = "r.premise", reference!("premise");
    Resolution(ResolutionId) = "r.resolution", reference!("resolution");
    Refutation(RefutationId) = "r.refutation", reference!("refutation");
    Evidence(EvidenceId) = "r.evidence", reference!("evidence");
    Exposure(ExposureId) = "r.exposure", reference!("exposure");
    Assessment(AssessmentId) = "r.assessment", reference!("assessment");
    Validation(ValidationId) = "r.validation", reference!("validation");
    Poc(PocId) = "r.poc", reference!("poc");
    PocStart(PocStartId) = "r.poc_start", reference!("poc_start");
    Environment(EnvironmentId) = "r.environment", reference!("environment");
    Finding(FindingId) = "r.finding", reference!("finding");
}

/// A flaw or candidate an event concerns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "id",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Subject {
    Flaw(FlawId),
    Candidate(CandidateId),
}

/// `note` is the actor's own words, or the text of the record the event wrote.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Event {
    pub id: EventId,
    pub kind: EventKind,
    pub actor: AgentId,
    pub record: Option<EventRecord>,
    pub note: Option<String>,
    pub at: DateTime<Utc>,
}

/// What an event holds beyond its kind and subjects.
#[derive(Clone, Copy)]
pub(super) enum Detail<'a> {
    /// The actor's words, which no record holds.
    Note(&'a str),
    Record(EventRecord),
}

/// Append an audit event about `subjects`, returning its id.
pub(super) fn audit(
    db: &Db,
    actor: &AgentId,
    kind: EventKind,
    detail: Detail,
    subjects: &[Subject],
) -> Result<EventId> {
    let actor = actor_row(db, actor)?;
    let event = db.insert(
        "INSERT INTO event(kind,actor,at) VALUES (?1,?2,?3)",
        params![kind, actor, now()],
    )?;
    for subject in subjects {
        match subject {
            Subject::Flaw(flaw) => db.execute(
                "INSERT INTO flaw_event(event,flaw) VALUES (?1,?2) ON CONFLICT DO NOTHING",
                params![event, flaw.0],
            )?,
            Subject::Candidate(candidate) => db.execute(
                "INSERT INTO candidate_event(event,candidate) VALUES (?1,?2) ON CONFLICT DO NOTHING",
                params![event, candidate.0],
            )?,
        };
    }
    match detail {
        Detail::Note(note) => {
            db.execute(
                "INSERT INTO event_note(event,note) VALUES (?1,?2)",
                params![event, note],
            )?;
        }
        Detail::Record(record) => record_event(db, event, record)?,
    }
    Ok(EventId(event))
}

fn record_event(db: &Db, event: i64, record: EventRecord) -> Result<()> {
    let (sql, id) = record.insert();
    db.execute(sql, params![event, id])?;
    Ok(())
}

/// Events from `$from`, keyed by `$key`, past `?1` with limit `?2`; the filter, if
/// any, binds `?3`.
macro_rules! events_sql {
    ($from:literal, $key:literal, $filter:literal) => {
        concat!(
            "SELECT e.id,e.kind,e.at,",
            actor_columns!(),
            ",",
            record_columns!(),
            "coalesce(n.note,ph.description,pu.question,oc.note,g.statement,d.reason,rl.note,\
             pr.statement,rs.note,rf.reason,ev.note,ex.note,sa.rationale,v.evidence,p.evidence) ",
            $from,
            " JOIN actor a ON a.id=e.actor \
             LEFT JOIN event_note n ON n.event=e.id LEFT JOIN event_record r ON r.event=e.id \
             LEFT JOIN path ph ON ph.id=r.path \
             LEFT JOIN plan_unit pu ON pu.id=r.unit LEFT JOIN outcome oc ON oc.id=r.outcome \
             LEFT JOIN guarantee g ON g.id=r.guarantee LEFT JOIN deferred d ON d.id=r.deferred \
             LEFT JOIN relation rl ON rl.id=r.relation LEFT JOIN premise pr ON pr.id=r.premise \
             LEFT JOIN premise_resolution rs ON rs.id=r.resolution \
             LEFT JOIN flaw_refutation rf ON rf.id=r.refutation LEFT JOIN evidence ev ON ev.id=r.evidence \
             LEFT JOIN exposure ex ON ex.id=r.exposure LEFT JOIN assessment sa ON sa.id=r.assessment \
             LEFT JOIN validation v ON v.id=r.validation LEFT JOIN poc p ON p.id=r.poc \
             WHERE ",
            $key,
            ">?1",
            $filter,
            " ORDER BY ",
            $key,
            " LIMIT ?2"
        )
    };
}

impl Store {
    /// All audit events, or the events about one flaw or candidate.
    pub fn events(&self, subject: Option<Subject>, page: Page) -> Result<Listing<Event>> {
        self.read(|db| {
            let (sql, filter): (&'static str, _) = match subject {
                None => (events_sql!("FROM event e", "e.id", ""), Vec::new()),
                Some(Subject::Flaw(flaw)) => (
                    events_sql!(
                        "FROM flaw_event s JOIN event e ON e.id=s.event",
                        "s.event",
                        " AND s.flaw=?3"
                    ),
                    params![flaw.0],
                ),
                Some(Subject::Candidate(candidate)) => (
                    events_sql!(
                        "FROM candidate_event s JOIN event e ON e.id=s.event",
                        "s.event",
                        " AND s.candidate=?3"
                    ),
                    params![candidate.0],
                ),
            };
            let mut params = params![page.after, page.limit()];
            params.extend(filter);
            let events = rows(db, sql, params, event_row)?;
            Ok(Listing::keyset(events, page))
        })
    }
}

/// The first column of [`events_sql!`] after the actor's.
const RECORD_OFFSET: i32 = 4;

fn event_row(row: &Row) -> Result<Event> {
    Ok(Event {
        id: EventId(row.get(0)?),
        kind: enum_column(row, 1)?,
        at: time(row.get(2)?)?,
        actor: actor_column(row, 3)?,
        record: EventRecord::decode(row, RECORD_OFFSET)?,
        note: row.get(RECORD_OFFSET + EventRecord::COLUMNS)?,
    })
}
