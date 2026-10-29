//! The retry-status inventory: every site in a component's files that returns a status asking
//! its caller to retry or supply more input, or maps a producer's false or 0 to one, and what
//! that status can mean there. A sweep completes only once every site in its files is
//! classified, and a guarantee spanning the attempts such a status asks for cites every site
//! of it its component records.

use libsql::Row;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::db::{Db, optional_enum_column, params};

use super::*;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewRetrySite {
    /// The return, or the use of the mapping, in a file a component owns, in its function.
    pub location: FunctionLocation,
    /// The status as the code spells it.
    pub status: Text,
    #[serde(default)]
    pub producer: Option<Text>,
    /// What the status can mean here, once decided; restating a site without one keeps it.
    #[serde(default)]
    pub class: Option<RetryClass>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RetrySite {
    pub id: RetrySiteId,
    pub location: Location,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer: Option<String>,
    /// None until classified.
    pub class: Option<RetryClass>,
}

/// Retry sites `s` at location `l` in files of component `$component`.
macro_rules! sites_of {
    ($component:literal) => {
        concat!(
            "retry_site s JOIN location l ON l.id=s.location JOIN component_file cf ON \
             cf.file=l.file AND cf.component=",
            $component
        )
    };
}

/// Retry sites `s` at location `l` in the files of JSON list `$files`.
macro_rules! sites_in {
    ($files:literal) => {
        concat!(
            "retry_site s JOIN location l ON l.id=s.location JOIN json_each(",
            $files,
            ") f ON f.value=l.file"
        )
    };
}

/// Sites of status `$status` among `$sites` that no evidence on guarantee `$guarantee`
/// covers.
macro_rules! uncited {
    ($guarantee:literal, $status:literal, $($sites:tt)+) => {
        concat!(
            "SELECT s.id FROM ",
            $($sites)+,
            " WHERE s.status=",
            $status,
            " AND NOT EXISTS (SELECT 1 FROM evidence e JOIN location el ON el.id=e.location WHERE \
             e.guarantee=",
            $guarantee,
            " AND el.file=l.file AND el.start_line<=l.start_line AND el.end_line>=l.end_line) \
             ORDER BY s.id"
        )
    };
}

fn site_ids(db: &Db, sql: &'static str, params: Vec<libsql::Value>) -> Result<Vec<RetrySiteId>> {
    rows(db, sql, params, |row| Ok(RetrySiteId(row.get(0)?)))
}

/// Refuse guarantee `id` of `component` spanning `status` unless its evidence covers every
/// site of that status the component records, and it records one.
pub(super) fn cited(db: &Db, id: GuaranteeId, component: ComponentId, status: &str) -> Result<()> {
    if !exists(
        db,
        concat!("SELECT 1 FROM ", sites_of!("?1"), " WHERE s.status=?2"),
        params![component.0, status],
    )? {
        return Err(Error::NoRetrySites {
            status: status.to_owned(),
        });
    }
    let sites = site_ids(
        db,
        uncited!("?1", "?3", sites_of!("?2")),
        params![id.0, component.0, status],
    )?;
    if sites.is_empty() {
        Ok(())
    } else {
        Err(Error::UncitedRetrySites {
            status: status.to_owned(),
            sites,
        })
    }
}

/// Refuse completing a sweep of `component` covering `files` while a site in them is
/// unclassified, or a guarantee of the component spanning a retry status leaves a site of it
/// there uncited.
pub(super) fn swept(db: &Db, component: ComponentId, files: &[String]) -> Result<()> {
    let files = json_list(files)?;
    let sites = site_ids(
        db,
        concat!(
            "SELECT s.id FROM ",
            sites_in!("?1"),
            " WHERE s.class IS NULL ORDER BY s.id"
        ),
        params![&files],
    )?;
    if !sites.is_empty() {
        return Err(Error::UnclassifiedRetrySites { sites });
    }
    for (guarantee, status) in rows(
        db,
        "SELECT id,retry_status FROM guarantee WHERE component=?1 AND retry_status IS NOT NULL \
         ORDER BY id",
        params![component.0],
        |row| Ok((GuaranteeId(row.get(0)?), row.get::<String>(1)?)),
    )? {
        let sites = site_ids(
            db,
            uncited!("?1", "?3", sites_in!("?2")),
            params![guarantee.0, &files, &status],
        )?;
        if !sites.is_empty() {
            return Err(Error::UncitedRetrySites { status, sites });
        }
    }
    Ok(())
}

fn site_row(row: &Row) -> Result<RetrySite> {
    Ok(RetrySite {
        id: RetrySiteId(row.get(0)?),
        status: row.get(1)?,
        producer: row.get(2)?,
        class: optional_enum_column(row, 3)?,
        location: scope::location_column(row, 4)?,
    })
}

/// The retry sites in `component`'s files.
pub(super) fn of(db: &Db, component: ComponentId) -> Result<Vec<RetrySite>> {
    rows(
        db,
        concat!(
            "SELECT s.id,s.status,s.producer,s.class,",
            scope::location_columns!("l"),
            " FROM ",
            sites_of!("?1"),
            " ORDER BY l.file,l.start_line,s.id"
        ),
        params![component.0],
        site_row,
    )
}

impl Store {
    /// Records a retry site, or restates the one at its region, whatever function it names;
    /// `producer` is the function whose false or 0 the site maps to the status. A site meaning
    /// invalid input or another failure bears on its function's status contract.
    pub fn record_retry_site(&self, actor: &AgentId, site: &NewRetrySite) -> Result<RetrySiteId> {
        self.write(|db| {
            if !exists(
                db,
                "SELECT 1 FROM component_file WHERE file=?1",
                params![&site.location.file],
            )? {
                return Err(Error::Unowned {
                    file: site.location.file.to_string(),
                });
            }
            let location = scope::location_id(db, &site.location)?;
            let fields = || params![location.0, &site.status, site.producer.as_ref(), site.class];
            let restated = db.execute(
                "UPDATE retry_site SET location=?1,status=?2,producer=?3,class=coalesce(?4,class) \
                 WHERE location IN (SELECT r.id FROM location r JOIN location l ON l.id=?1 AND \
                 r.file=l.file AND r.start_line=l.start_line AND r.end_line=l.end_line)",
                fields(),
            )?;
            if restated == 0 {
                db.execute(
                    "INSERT INTO retry_site(location,status,producer,class) VALUES (?1,?2,?3,?4)",
                    fields(),
                )?;
            }
            db.execute(
                "INSERT INTO contract(component,function,kind) SELECT cf.component,l.function,\
                 'status_contract' FROM retry_site s JOIN location l ON l.id=s.location JOIN \
                 component_file cf ON cf.file=l.file WHERE s.location=?1 AND s.class IN \
                 ('invalid','other') ON CONFLICT DO NOTHING",
                params![location.0],
            )?;
            let id = stored(
                db,
                "SELECT id FROM retry_site WHERE location=?1",
                params![location.0],
                |row| Ok(RetrySiteId(row.get(0)?)),
            )?;
            audit(
                db,
                actor,
                EventKind::Recorded,
                Detail::Record(EventRecord::RetrySite(id)),
                &[],
            )?;
            Ok(id)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::tests::{
            answerer, component, evidence_at, fixture, location, new_guarantee, path, queued, text,
        },
        *,
    };

    fn site(line: u32, class: Option<RetryClass>) -> NewRetrySite {
        NewRetrySite {
            location: location("decoder/lib.c", line).in_function(text("DecodeCommands")),
            status: text("NEEDS_MORE_INPUT"),
            producer: Some(text("ReadSymbol")),
            class,
        }
    }

    /// The sweep a path reaching a new `decoder` component plans.
    fn decoder(store: &Store, actor: &AgentId) -> (ComponentId, UnitId) {
        let decoder = component(store, actor, "decoder");
        path(store, actor, "stream", &[decoder]);
        let [WorkSubject::Unit(sweep)] = queued(store, Phase::Sweep)[..] else {
            panic!("one sweep planned");
        };
        (decoder, sweep)
    }

    /// A retry site names the function returning its status.
    #[test]
    fn a_retry_site_names_its_function() {
        let site = |location| {
            serde_json::from_value::<NewRetrySite>(
                serde_json::json!({"location": location, "status": "NEEDS_MORE_INPUT"}),
            )
        };
        let region = serde_json::json!({"file": "lib.c", "start_line": 1, "end_line": 1});
        assert!(site(region).is_err());
        let named = serde_json::json!({"file": "lib.c", "start_line": 1, "end_line": 1, "function": "Decode"});
        let named = site(named).unwrap();
        assert_eq!(named.location.function, Some(text("Decode")));
    }

    /// A sweep does not complete while a retry site in its files is unclassified; restating
    /// the site's region with its class lets it, whatever function the restatement names, and
    /// restating it without one keeps the class.
    #[test]
    fn a_sweep_completes_once_every_retry_site_is_classified() {
        let (_directory, store, actor) = fixture();
        let (decoder, sweep) = decoder(&store, &actor);
        assert!(matches!(
            store.record_retry_site(
                &actor,
                &NewRetrySite {
                    location: location("elsewhere.c", 3).in_function(text("DecodeCommands")),
                    ..site(3, None)
                }
            ),
            Err(Error::Unowned { .. })
        ));
        let unclassified = store.record_retry_site(&actor, &site(10, None)).unwrap();
        let complete = || {
            store.record_outcome(
                &answerer(&actor),
                sweep,
                Outcome::Completed,
                &text("swept"),
                &[],
            )
        };
        assert!(matches!(
            complete(),
            Err(Error::UnclassifiedRetrySites { sites }) if sites == [unclassified]
        ));
        let restated = NewRetrySite {
            location: location("decoder/lib.c", 10).in_function(text("DecodeBlock")),
            ..site(10, Some(RetryClass::Invalid))
        };
        let classified = store.record_retry_site(&actor, &restated).unwrap();
        assert_eq!(classified, unclassified);
        store.record_retry_site(&actor, &site(10, None)).unwrap();
        assert_eq!(
            store.dossier(decoder).unwrap().retry_sites[0].class,
            Some(RetryClass::Invalid)
        );
        complete().unwrap();
    }

    /// A guarantee spanning a retry status needs that status's sites recorded and evidence at
    /// every one; a site recorded after it holds its component's sweep back until it is cited.
    #[test]
    fn a_guarantee_spanning_a_retry_status_cites_every_site() {
        let (_directory, store, actor) = fixture();
        let (decoder, sweep) = decoder(&store, &actor);
        let at = |line| evidence_at("decoder/lib.c", line);
        let add = |evidence| {
            let guarantee = NewGuarantee {
                retry_status: Some(text("NEEDS_MORE_INPUT")),
                ..new_guarantee(InvariantKind::Bounds, evidence)
            };
            store.add_guarantee(&actor, decoder, &guarantee)
        };
        assert!(matches!(add(vec![at(10)]), Err(Error::NoRetrySites { .. })));
        store
            .record_retry_site(&actor, &site(10, Some(RetryClass::InputExhausted)))
            .unwrap();
        let missed = store
            .record_retry_site(&actor, &site(20, Some(RetryClass::InputExhausted)))
            .unwrap();
        assert!(matches!(
            add(vec![at(10)]),
            Err(Error::UncitedRetrySites { sites, .. }) if sites == [missed]
        ));
        let bound = add(vec![at(10), at(20)]).unwrap();

        let late = store
            .record_retry_site(&actor, &site(30, Some(RetryClass::Other)))
            .unwrap();
        let complete = || {
            store.record_outcome(
                &answerer(&actor),
                sweep,
                Outcome::Completed,
                &text("swept"),
                &[],
            )
        };
        assert!(matches!(
            complete(),
            Err(Error::UncitedRetrySites { sites, .. }) if sites == [late]
        ));
        store
            .add_evidence(&actor, EvidenceSubject::Guarantee(bound), &[at(30)])
            .unwrap();
        complete().unwrap();
    }
}
