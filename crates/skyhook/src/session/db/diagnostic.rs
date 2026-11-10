//! Relational encoding of terminal diagnostics.

use std::collections::HashMap;

use libsql::Row;

use super::{
    Db, DbResult, Encoder, corrupt,
    decode::{Events, Query, u64_of},
    enum_column, optional_enum_column, params,
};
use skyhook_tools::diagnostic::{
    Cause, CauseKind, ConnectionFault, Diagnostic, DiagnosticContext, FailureSite, SiteKind,
};
use skyhook_tools::target::ExecutionLocation;
use skyhook_worker::diagnostic::{PathFact, Subject, SubjectKind};
use skyhook_worker::job::JobId;
use skyhook_worker::op::{IoKind, OsError, TargetPath};

/// The columns a [`Subject`] or [`Cause`] stores its fields in.
#[derive(Default)]
struct Columns {
    path: Option<TargetPath>,
    native: Option<Vec<u8>>,
    text: Option<String>,
    job: Option<JobId>,
    connection: Option<ConnectionFault>,
    io_kind: Option<IoKind>,
    io_code: Option<i32>,
    io_detail: Option<String>,
}

impl Columns {
    fn of_subject(subject: &Subject) -> Self {
        match subject {
            Subject::None | Subject::Process => Self::default(),
            Subject::Path(path)
            | Subject::WorkingDirectory(path)
            | Subject::StagingFile(path)
            | Subject::ParentDirectory(path)
            | Subject::DirectoryEntry(path) => Self {
                path: Some(path.clone()),
                ..Self::default()
            },
            Subject::Argument(text) | Subject::Tool(text) | Subject::Label(text) => Self {
                text: Some(text.clone()),
                ..Self::default()
            },
            Subject::Job(job) => Self {
                job: Some(*job),
                ..Self::default()
            },
        }
    }

    fn of_cause(cause: &Cause) -> Self {
        match cause {
            Cause::Io(OsError { kind, code, detail }) => Self {
                io_kind: Some(*kind),
                io_code: *code,
                io_detail: detail.clone(),
                ..Self::default()
            },
            Cause::InvalidArguments(text)
            | Cause::Denied(text)
            | Cause::Message(text)
            | Cause::Unavailable { tool: text }
            | Cause::UnknownTool { tool: text }
            | Cause::ScriptUnavailable { tool: text } => Self {
                text: Some(text.clone()),
                ..Self::default()
            },
            Cause::Connection { fault, detail } => Self {
                connection: Some(*fault),
                text: Some(detail.clone()),
                ..Self::default()
            },
            Cause::UnknownJob { job } | Cause::JobAlreadyTerminal { job } => Self {
                job: Some(*job),
                ..Self::default()
            },
            Cause::Unspelled { native } => Self {
                native: Some(native.clone()),
                ..Self::default()
            },
            Cause::Cancelled | Cause::Interrupted | Cause::InputClosed | Cause::Json => {
                Self::default()
            }
        }
    }

    /// The subject of `kind`, if the column its field needs is set.
    fn subject(self, kind: SubjectKind) -> Option<Subject> {
        Some(match kind {
            SubjectKind::None => Subject::None,
            SubjectKind::Path => Subject::Path(self.path?),
            SubjectKind::WorkingDirectory => Subject::WorkingDirectory(self.path?),
            SubjectKind::StagingFile => Subject::StagingFile(self.path?),
            SubjectKind::ParentDirectory => Subject::ParentDirectory(self.path?),
            SubjectKind::DirectoryEntry => Subject::DirectoryEntry(self.path?),
            SubjectKind::Argument => Subject::Argument(self.text?),
            SubjectKind::Tool => Subject::Tool(self.text?),
            SubjectKind::Job => Subject::Job(self.job?),
            SubjectKind::Process => Subject::Process,
            SubjectKind::Label => Subject::Label(self.text?),
        })
    }

    /// The cause of `kind`, if the columns its fields need are set.
    fn cause(self, kind: CauseKind) -> Option<Cause> {
        Some(match kind {
            CauseKind::Io => Cause::Io(OsError {
                kind: self.io_kind?,
                code: self.io_code,
                detail: self.io_detail,
            }),
            CauseKind::InvalidArguments => Cause::InvalidArguments(self.text?),
            CauseKind::Denied => Cause::Denied(self.text?),
            CauseKind::Cancelled => Cause::Cancelled,
            CauseKind::Interrupted => Cause::Interrupted,
            CauseKind::InputClosed => Cause::InputClosed,
            CauseKind::Message => Cause::Message(self.text?),
            CauseKind::Connection => Cause::Connection {
                fault: self.connection?,
                detail: self.text?,
            },
            CauseKind::Json => Cause::Json,
            CauseKind::Unavailable => Cause::Unavailable { tool: self.text? },
            CauseKind::UnknownTool => Cause::UnknownTool { tool: self.text? },
            CauseKind::ScriptUnavailable => Cause::ScriptUnavailable { tool: self.text? },
            CauseKind::UnknownJob => Cause::UnknownJob { job: self.job? },
            CauseKind::JobAlreadyTerminal => Cause::JobAlreadyTerminal { job: self.job? },
            CauseKind::Unspelled => Cause::Unspelled {
                native: self.native?,
            },
        })
    }
}

impl Encoder {
    pub(super) fn diagnostic(
        &mut self,
        db: &Db,
        finish: u64,
        diagnostic: &Diagnostic,
    ) -> DbResult<()> {
        let context = &diagnostic.context;
        let subject = Columns::of_subject(&context.subject);
        let (target, workspace) = match &context.site {
            FailureSite::Invocation | FailureSite::Host => (None, None),
            FailureSite::Execution(location) => (
                Some(self.target(db, &location.target)?),
                Some(location.workspace.as_str()),
            ),
        };
        let cause = Columns::of_cause(&diagnostic.cause);
        db.execute(
            "INSERT INTO job_finish_diagnostic (finish, operation, subject, subject_path, \
             subject_text, subject_job, site, location_target, location_workspace, effects, \
             request, cause, io_kind, io_code, cause_text, io_detail, cause_job, cause_native, \
             connection_fault) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
             ?18, ?19)",
            params![
                finish,
                context.operation,
                context.subject.kind(),
                subject.path.as_ref().map(TargetPath::as_str),
                subject.text,
                subject.job.map(JobId::get),
                context.site.kind(),
                target,
                workspace,
                context.effects,
                context.request,
                diagnostic.cause.kind(),
                cause.io_kind,
                cause.io_code.map(i64::from),
                cause.text,
                cause.io_detail,
                cause.job.map(JobId::get),
                cause.native,
                cause.connection,
            ],
        )?;
        for (position, fact) in context.paths.iter().enumerate() {
            db.execute(
                "INSERT INTO diagnostic_path (finish, position, role, path) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![finish, position, fact.role, fact.path.as_str()],
            )?;
        }
        Ok(())
    }
}

fn job(row: &Row, index: i32) -> DbResult<Option<JobId>> {
    row.get::<Option<i64>>(index)?
        .map(|job| JobId::new(u64_of(job)).map_err(|error| corrupt(error.to_string())))
        .transpose()
}

fn decode(row: &Row, paths: Vec<PathFact>) -> DbResult<Diagnostic> {
    let subject = Columns {
        path: row.get::<Option<String>>(3)?.map(TargetPath::from),
        text: row.get(4)?,
        job: job(row, 5)?,
        ..Columns::default()
    }
    .subject(enum_column(row, 2)?)
    .ok_or_else(|| corrupt("diagnostic subject does not match its columns"))?;
    let site = match enum_column(row, 6)? {
        SiteKind::Invocation => FailureSite::Invocation,
        SiteKind::Host => FailureSite::Host,
        SiteKind::Execution => FailureSite::Execution(ExecutionLocation {
            target: super::decode::parsed(row.get(7)?)?,
            workspace: row.get::<String>(8)?.into(),
        }),
    };
    let cause = Columns {
        io_kind: optional_enum_column(row, 12)?,
        io_code: row.get(13)?,
        text: row.get(14)?,
        io_detail: row.get(15)?,
        job: job(row, 16)?,
        native: row.get(17)?,
        connection: optional_enum_column(row, 18)?,
        ..Columns::default()
    }
    .cause(enum_column(row, 11)?)
    .ok_or_else(|| corrupt("diagnostic cause does not match its columns"))?;
    Ok(Diagnostic {
        context: DiagnosticContext {
            operation: enum_column(row, 1)?,
            subject,
            site,
            effects: enum_column(row, 9)?,
            request: enum_column(row, 10)?,
            paths,
        },
        cause,
    })
}

/// Each job finish's diagnostic, by its entry.
pub(super) fn load(events: &Events) -> DbResult<HashMap<i64, Diagnostic>> {
    let db = events.db;
    let mut paths: HashMap<i64, Vec<PathFact>> = HashMap::new();
    let Query { sql, params } = events.scoped(
        "SELECT finish, role, path FROM diagnostic_path {scope} ORDER BY finish, position",
        "finish",
    )?;
    for (key, fact) in db.query(sql, params, |row| {
        Ok((
            row.get(0)?,
            PathFact {
                role: enum_column(row, 1)?,
                path: row.get::<String>(2)?.into(),
            },
        ))
    })? {
        paths.entry(key).or_default().push(fact);
    }
    let Query { sql, params } = events.scoped(
        "SELECT d.finish, d.operation, d.subject, d.subject_path, d.subject_text, \
         d.subject_job, d.site, t.name, d.location_workspace, d.effects, d.request, d.cause, \
         d.io_kind, d.io_code, d.cause_text, d.io_detail, d.cause_job, d.cause_native, \
         d.connection_fault \
         FROM job_finish_diagnostic d \
         LEFT JOIN target t ON t.id = d.location_target {scope}",
        "d.finish",
    )?;
    Ok(db
        .query(sql, params, |row| {
            let key = row.get(0)?;
            Ok((key, decode(row, paths.remove(&key).unwrap_or_default())?))
        })?
        .into_iter()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::JobRole;
    use crate::{
        job::JobEnd,
        session::{SessionEvent, db::tests::Fixture},
    };
    use skyhook_tools::diagnostic::{PartialContext, RequestEffects};
    use skyhook_worker::diagnostic::{Effects, Operation, PathRole};
    use skyhook_worker::op::IoKind;

    fn finish(fixture: &mut Fixture, diagnostic: Option<Diagnostic>) {
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
                location: ExecutionLocation::root("/workspace".into()),
            },
        );
        fixture.one(
            root,
            SessionEvent::JobFinished {
                job,
                state: JobEnd::Completed,
                diagnostic,
                images: Vec::new(),
            },
        );
    }

    #[test]
    fn every_variant_round_trips() {
        let mut fixture = Fixture::new();
        fixture.start("/workspace");
        let remote = TargetPath::from(r"C:\remote-é");
        let subjects = [
            Subject::None,
            Subject::Path(remote.clone()),
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
            FailureSite::Execution(ExecutionLocation::named(
                "worker".parse().unwrap(),
                remote.clone(),
            )),
        ];
        let causes = [
            Cause::Io(OsError {
                kind: IoKind::NotFound,
                code: Some(2),
                detail: None,
            }),
            Cause::Io(OsError {
                kind: IoKind::Other,
                code: None,
                detail: Some("custom capture failure".into()),
            }),
            Cause::InvalidArguments("limit must be positive".into()),
            Cause::Denied("not approved".into()),
            Cause::Cancelled,
            Cause::Interrupted,
            Cause::InputClosed,
            Cause::Message("capture failed".into()),
            Cause::Connection {
                fault: ConnectionFault::Protocol,
                detail: "shim closed before replying".into(),
            },
            Cause::Json,
            Cause::UnknownJob {
                job: JobId::new(999_998).unwrap(),
            },
            // A Windows target's UTF-16 code units, little-endian, kept as sent.
            Cause::Unspelled {
                native: vec![b'C', 0, b':', 0, 0x00, 0xd8],
            },
        ];
        for (index, operation) in Operation::ALL.iter().enumerate() {
            let diagnostic = Diagnostic {
                context: DiagnosticContext {
                    operation: *operation,
                    subject: subjects[index % subjects.len()].clone(),
                    site: sites[index % sites.len()].clone(),
                    effects: Effects::ALL[index % Effects::ALL.len()],
                    request: RequestEffects::ALL[index % RequestEffects::ALL.len()],
                    // Caller order is retained, not grouped by role.
                    paths: vec![
                        PathFact {
                            role: PathRole::Resolved,
                            path: remote.clone(),
                        },
                        PathFact {
                            role: PathRole::Requested,
                            path: "file".into(),
                        },
                    ],
                },
                cause: causes[index % causes.len()].clone(),
            };
            let io = Diagnostic::new(
                PartialContext::new(Operation::FinishCapture, Subject::Label("stdout".into()))
                    .at(FailureSite::Host)
                    .effects(Effects::OutputIncomplete)
                    .resolve(),
                Cause::Io(OsError {
                    kind: IoKind::ALL[index % IoKind::ALL.len()],
                    code: Some(-123),
                    detail: None,
                }),
            );
            finish(&mut fixture, Some(diagnostic));
            finish(&mut fixture, Some(io));
        }
        finish(&mut fixture, None);
        fixture.assert_round_trip();
    }
}
