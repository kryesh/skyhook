//! Relational encoding of terminal and output-persistence diagnostics.

use std::collections::HashMap;

use libsql::Row;

use super::{
    Db, DbResult, Encoder, corrupt,
    decode::{path, u64_of},
    encode::path_bytes,
    params,
};
use crate::{
    execution::ExecutionLocation,
    identity::JobId,
    tool::diagnostic::{
        Cause, Diagnostic, DiagnosticContext, Effects, FailureSite, IoKind, Operation, PathFact,
        PathRole, Subject,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Slot {
    Diagnostic,
    OutputDiagnostic,
}

impl Slot {
    fn as_str(self) -> &'static str {
        match self {
            Self::Diagnostic => "diagnostic",
            Self::OutputDiagnostic => "output_diagnostic",
        }
    }

    fn parse(value: &str) -> DbResult<Self> {
        match value {
            "diagnostic" => Ok(Self::Diagnostic),
            "output_diagnostic" => Ok(Self::OutputDiagnostic),
            _ => Err(corrupt("unknown diagnostic slot")),
        }
    }
}

impl Encoder {
    pub(super) fn diagnostic(
        &self,
        db: &Db,
        finish: u64,
        slot: Slot,
        diagnostic: &Diagnostic,
    ) -> DbResult<()> {
        let context = &diagnostic.context;
        let (subject, subject_path, subject_text, subject_job) = match &context.subject {
            Subject::None => ("none", None, None, None),
            Subject::Path(path) => ("path", Some(path_bytes(path)), None, None),
            Subject::WorkingDirectory(path) => {
                ("working_directory", Some(path_bytes(path)), None, None)
            }
            Subject::StagingFile(path) => ("staging_file", Some(path_bytes(path)), None, None),
            Subject::ParentDirectory(path) => {
                ("parent_directory", Some(path_bytes(path)), None, None)
            }
            Subject::DirectoryEntry(path) => {
                ("directory_entry", Some(path_bytes(path)), None, None)
            }
            Subject::Argument(text) => ("argument", None, Some(text), None),
            Subject::Tool(text) => ("tool", None, Some(text), None),
            Subject::Job(job) => ("job", None, None, Some(job.get())),
            Subject::Process => ("process", None, None, None),
            Subject::Label(text) => ("label", None, Some(text), None),
        };
        let (site, target, workspace) = match &context.site {
            FailureSite::Invocation => ("invocation", None, None),
            FailureSite::Host => ("host", None, None),
            FailureSite::Execution(location) => (
                "execution",
                Some(self.target(db, &location.target)?),
                Some(path_bytes(&location.workspace)),
            ),
        };
        let (cause, io_kind, io_code, cause_text, io_detail) = match &diagnostic.cause {
            Cause::Io { kind, code, detail } => (
                "io",
                Some(kind.as_str()),
                code.map(i64::from),
                None,
                detail.as_ref(),
            ),
            Cause::InvalidArguments(text) => ("invalid_arguments", None, None, Some(text), None),
            Cause::Denied(text) => ("denied", None, None, Some(text), None),
            Cause::Cancelled => ("cancelled", None, None, None, None),
            Cause::Interrupted => ("interrupted", None, None, None, None),
            Cause::InputClosed => ("input_closed", None, None, None, None),
            Cause::Message(text) => ("message", None, None, Some(text), None),
            Cause::Json => ("json", None, None, None, None),
        };
        db.execute(
            "INSERT INTO job_finish_diagnostic (finish, slot, operation, subject, subject_path, \
             subject_text, subject_job, site, location_target, location_workspace, effects, \
             cause, io_kind, io_code, cause_text, io_detail) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                finish,
                slot.as_str(),
                context.operation.as_str(),
                subject,
                subject_path,
                subject_text,
                subject_job,
                site,
                target,
                workspace,
                context.effects.as_str(),
                cause,
                io_kind,
                io_code,
                cause_text,
                io_detail,
            ],
        )?;
        for (position, fact) in context.paths.iter().enumerate() {
            db.execute(
                "INSERT INTO diagnostic_path (finish, slot, position, role, path) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    finish,
                    slot.as_str(),
                    position,
                    fact.role.as_str(),
                    path_bytes(&fact.path)
                ],
            )?;
        }
        Ok(())
    }
}

fn decode(row: &Row, paths: Vec<PathFact>) -> DbResult<Diagnostic> {
    let operation = Operation::parse(&row.get::<String>(2)?)
        .ok_or_else(|| corrupt("unknown diagnostic operation"))?;
    let subject = match row.get::<String>(3)?.as_str() {
        "none" => Subject::None,
        "path" => Subject::Path(path(row.get(4)?)),
        "working_directory" => Subject::WorkingDirectory(path(row.get(4)?)),
        "staging_file" => Subject::StagingFile(path(row.get(4)?)),
        "parent_directory" => Subject::ParentDirectory(path(row.get(4)?)),
        "directory_entry" => Subject::DirectoryEntry(path(row.get(4)?)),
        "argument" => Subject::Argument(row.get(5)?),
        "tool" => Subject::Tool(row.get(5)?),
        "job" => Subject::Job(
            JobId::new(u64_of(row.get(6)?)).map_err(|error| corrupt(error.to_string()))?,
        ),
        "process" => Subject::Process,
        "label" => Subject::Label(row.get(5)?),
        _ => return Err(corrupt("unknown diagnostic subject")),
    };
    let site = match row.get::<String>(7)?.as_str() {
        "invocation" => FailureSite::Invocation,
        "host" => FailureSite::Host,
        "execution" => FailureSite::Execution(ExecutionLocation {
            target: row.get(8)?,
            workspace: path(row.get(9)?),
        }),
        _ => return Err(corrupt("unknown diagnostic failure site")),
    };
    let effects = Effects::parse(&row.get::<String>(10)?)
        .ok_or_else(|| corrupt("unknown diagnostic effects"))?;
    let cause = match row.get::<String>(11)?.as_str() {
        "io" => Cause::Io {
            kind: IoKind::parse(&row.get::<String>(12)?)
                .ok_or_else(|| corrupt("unknown diagnostic IO kind"))?,
            code: row.get(13)?,
            detail: row.get(15)?,
        },
        "invalid_arguments" => Cause::InvalidArguments(row.get(14)?),
        "denied" => Cause::Denied(row.get(14)?),
        "cancelled" => Cause::Cancelled,
        "interrupted" => Cause::Interrupted,
        "input_closed" => Cause::InputClosed,
        "message" => Cause::Message(row.get(14)?),
        "json" => Cause::Json,
        _ => return Err(corrupt("unknown diagnostic cause")),
    };
    Ok(Diagnostic {
        context: DiagnosticContext {
            operation,
            subject,
            site,
            effects,
            paths,
        },
        cause,
    })
}

pub(super) fn load(db: &Db) -> DbResult<HashMap<(i64, Slot), Diagnostic>> {
    let mut paths: HashMap<(i64, Slot), Vec<PathFact>> = HashMap::new();
    for (key, fact) in db.query(
        "SELECT finish, slot, role, path FROM diagnostic_path ORDER BY finish, slot, position",
        Vec::new(),
        |row| {
            Ok((
                (row.get(0)?, Slot::parse(&row.get::<String>(1)?)?),
                PathFact {
                    role: PathRole::parse(&row.get::<String>(2)?)
                        .ok_or_else(|| corrupt("unknown diagnostic path role"))?,
                    path: path(row.get(3)?),
                },
            ))
        },
    )? {
        paths.entry(key).or_default().push(fact);
    }
    Ok(db
        .query(
            "SELECT d.finish, d.slot, d.operation, d.subject, d.subject_path, d.subject_text, \
             d.subject_job, d.site, t.name, d.location_workspace, d.effects, d.cause, \
             d.io_kind, d.io_code, d.cause_text, d.io_detail FROM job_finish_diagnostic d \
             LEFT JOIN target t ON t.id = d.location_target",
            Vec::new(),
            |row| {
                let key = (row.get(0)?, Slot::parse(&row.get::<String>(1)?)?);
                Ok((key, decode(row, paths.remove(&key).unwrap_or_default())?))
            },
        )?
        .into_iter()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        job::{JobRole, JobState},
        session::{SessionEvent, db::tests::Fixture},
    };

    fn finish(
        fixture: &mut Fixture,
        diagnostic: Option<Diagnostic>,
        output_diagnostic: Option<Diagnostic>,
    ) {
        let root = fixture.records[0].agent.clone();
        let job = JobId::new(fixture.records.len() as u64).unwrap();
        fixture.one(
            root.clone(),
            SessionEvent::JobCreated {
                job,
                parent: None,
                origin: None,
                tool: "read".into(),
                role: JobRole::Tool,
                name: None,
                arguments: serde_json::json!({}),
                output_schema: None,
                accepts_input: false,
                background: false,
                authorization_scope: None,
                location: ExecutionLocation::root("/workspace".into()),
            },
        );
        fixture.one(
            root,
            SessionEvent::JobFinished {
                job,
                state: JobState::Completed,
                diagnostic,
                output_diagnostic,
                images: Vec::new(),
            },
        );
    }

    #[test]
    fn dictionaries_list_every_diagnostic_vocabulary_word() {
        let fixture = Fixture::new();
        for (table, mut words) in [
            (
                "diagnostic_operation",
                Operation::ALL
                    .iter()
                    .map(|w| w.as_str())
                    .collect::<Vec<_>>(),
            ),
            (
                "diagnostic_effects",
                Effects::ALL.iter().map(|w| w.as_str()).collect(),
            ),
            (
                "diagnostic_io_kind",
                IoKind::ALL.iter().map(|w| w.as_str()).collect(),
            ),
            (
                "diagnostic_path_role",
                PathRole::ALL.iter().map(|w| w.as_str()).collect(),
            ),
        ] {
            let query = format!("SELECT name FROM {table} ORDER BY name");
            let names = fixture
                .db
                .query(&query, Vec::new(), |row| Ok(row.get::<String>(0)?))
                .unwrap();
            words.sort_unstable();
            assert_eq!(names, words, "{table}");
        }
    }

    #[test]
    fn every_variant_native_path_and_independent_slot_round_trips() {
        let mut fixture = Fixture::new();
        fixture.start("/workspace");
        #[cfg(unix)]
        let native = path(b"/remote-\xfe".to_vec());
        #[cfg(not(unix))]
        let native = path(b"/remote".to_vec());
        let subjects = [
            Subject::None,
            Subject::Path(native.clone()),
            Subject::WorkingDirectory("/workspace".into()),
            Subject::StagingFile("/workspace/.staging".into()),
            Subject::ParentDirectory("/workspace/parent".into()),
            Subject::DirectoryEntry("/workspace/entry".into()),
            Subject::Argument("path".into()),
            Subject::Tool("read".into()),
            // A failed job lookup must not require the subject job to exist.
            Subject::Job(JobId::new(999_999).unwrap()),
            Subject::Process,
            Subject::Label("output".into()),
        ];
        let sites = [
            FailureSite::Invocation,
            FailureSite::Host,
            FailureSite::Execution(ExecutionLocation::named("worker", native.clone())),
        ];
        let causes = [
            Cause::Io {
                kind: IoKind::NotFound,
                code: Some(2),
                detail: None,
            },
            Cause::Io {
                kind: IoKind::Other,
                code: None,
                detail: Some("custom capture failure".into()),
            },
            Cause::InvalidArguments("limit must be positive".into()),
            Cause::Denied("not approved".into()),
            Cause::Cancelled,
            Cause::Interrupted,
            Cause::InputClosed,
            Cause::Message("capture failed".into()),
            Cause::Json,
        ];
        for (index, operation) in Operation::ALL.iter().enumerate() {
            let diagnostic = Diagnostic {
                context: DiagnosticContext {
                    operation: *operation,
                    subject: subjects[index % subjects.len()].clone(),
                    site: sites[index % sites.len()].clone(),
                    effects: Effects::ALL[index % Effects::ALL.len()],
                    // Caller order is retained, not grouped by role.
                    paths: vec![
                        PathFact {
                            role: PathRole::Resolved,
                            path: native.clone(),
                        },
                        PathFact {
                            role: PathRole::Requested,
                            path: "file".into(),
                        },
                    ],
                },
                cause: causes[index % causes.len()].clone(),
            };
            let output_diagnostic = Diagnostic::new(
                DiagnosticContext::new(Operation::FinishCapture, Subject::Label("stdout".into()))
                    .at(FailureSite::Host)
                    .effects(Effects::OutputIncomplete),
                Cause::Io {
                    kind: IoKind::ALL[index % IoKind::ALL.len()],
                    code: Some(-123),
                    detail: None,
                },
            );
            finish(
                &mut fixture,
                Some(diagnostic.clone()),
                Some(output_diagnostic.clone()),
            );
            if index == 0 {
                finish(&mut fixture, None, Some(output_diagnostic));
                finish(&mut fixture, Some(diagnostic), None);
                finish(&mut fixture, None, None);
            }
        }
        fixture.assert_round_trip();
    }
}
