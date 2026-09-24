//! Relational encoding of terminal and output-persistence diagnostics.

use std::collections::HashMap;

use libsql::Row;

use super::{
    Db, DbResult, Encoder,
    decode::{path, u64_of},
    encode::path_bytes,
    enum_column, params,
};
use crate::{
    execution::ExecutionLocation,
    identity::JobId,
    named_enum::named_enum,
    tool::diagnostic::{Cause, Diagnostic, DiagnosticContext, FailureSite, PathFact, Subject},
};

named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
    pub(super) enum Slot {
        Diagnostic = "diagnostic",
        OutputDiagnostic = "output_diagnostic",
    }
}

named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub(super) enum SubjectKind {
        None = "none",
        Path = "path",
        WorkingDirectory = "working_directory",
        StagingFile = "staging_file",
        ParentDirectory = "parent_directory",
        DirectoryEntry = "directory_entry",
        Argument = "argument",
        Tool = "tool",
        Job = "job",
        Process = "process",
        Label = "label",
    }
}

named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub(super) enum SiteKind {
        Invocation = "invocation",
        Host = "host",
        Execution = "execution",
    }
}

named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub(super) enum CauseKind {
        Io = "io",
        InvalidArguments = "invalid_arguments",
        Denied = "denied",
        Cancelled = "cancelled",
        Interrupted = "interrupted",
        InputClosed = "input_closed",
        Message = "message",
        Json = "json",
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
            Subject::None => (SubjectKind::None, None, None, None),
            Subject::Path(path) => (SubjectKind::Path, Some(path_bytes(path)), None, None),
            Subject::WorkingDirectory(path) => (
                SubjectKind::WorkingDirectory,
                Some(path_bytes(path)),
                None,
                None,
            ),
            Subject::StagingFile(path) => {
                (SubjectKind::StagingFile, Some(path_bytes(path)), None, None)
            }
            Subject::ParentDirectory(path) => (
                SubjectKind::ParentDirectory,
                Some(path_bytes(path)),
                None,
                None,
            ),
            Subject::DirectoryEntry(path) => (
                SubjectKind::DirectoryEntry,
                Some(path_bytes(path)),
                None,
                None,
            ),
            Subject::Argument(text) => (SubjectKind::Argument, None, Some(text), None),
            Subject::Tool(text) => (SubjectKind::Tool, None, Some(text), None),
            Subject::Job(job) => (SubjectKind::Job, None, None, Some(job.get())),
            Subject::Process => (SubjectKind::Process, None, None, None),
            Subject::Label(text) => (SubjectKind::Label, None, Some(text), None),
        };
        let (site, target, workspace) = match &context.site {
            FailureSite::Invocation => (SiteKind::Invocation, None, None),
            FailureSite::Host => (SiteKind::Host, None, None),
            FailureSite::Execution(location) => (
                SiteKind::Execution,
                Some(self.target(db, location.target.as_str())?),
                Some(path_bytes(&location.workspace)),
            ),
        };
        let (cause, io_kind, io_code, cause_text, io_detail) = match &diagnostic.cause {
            Cause::Io { kind, code, detail } => (
                CauseKind::Io,
                Some(*kind),
                code.map(i64::from),
                None,
                detail.as_ref(),
            ),
            Cause::InvalidArguments(text) => {
                (CauseKind::InvalidArguments, None, None, Some(text), None)
            }
            Cause::Denied(text) => (CauseKind::Denied, None, None, Some(text), None),
            Cause::Cancelled => (CauseKind::Cancelled, None, None, None, None),
            Cause::Interrupted => (CauseKind::Interrupted, None, None, None, None),
            Cause::InputClosed => (CauseKind::InputClosed, None, None, None, None),
            Cause::Message(text) => (CauseKind::Message, None, None, Some(text), None),
            Cause::Json => (CauseKind::Json, None, None, None, None),
        };
        db.execute(
            "INSERT INTO job_finish_diagnostic (finish, slot, operation, subject, subject_path, \
             subject_text, subject_job, site, location_target, location_workspace, effects, \
             cause, io_kind, io_code, cause_text, io_detail) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                finish,
                slot,
                context.operation,
                subject,
                subject_path,
                subject_text,
                subject_job,
                site,
                target,
                workspace,
                context.effects,
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
                params![finish, slot, position, fact.role, path_bytes(&fact.path)],
            )?;
        }
        Ok(())
    }
}

fn decode(row: &Row, paths: Vec<PathFact>) -> DbResult<Diagnostic> {
    let subject = match enum_column(row, 3)? {
        SubjectKind::None => Subject::None,
        SubjectKind::Path => Subject::Path(path(row.get(4)?)),
        SubjectKind::WorkingDirectory => Subject::WorkingDirectory(path(row.get(4)?)),
        SubjectKind::StagingFile => Subject::StagingFile(path(row.get(4)?)),
        SubjectKind::ParentDirectory => Subject::ParentDirectory(path(row.get(4)?)),
        SubjectKind::DirectoryEntry => Subject::DirectoryEntry(path(row.get(4)?)),
        SubjectKind::Argument => Subject::Argument(row.get(5)?),
        SubjectKind::Tool => Subject::Tool(row.get(5)?),
        SubjectKind::Job => Subject::Job(
            JobId::new(u64_of(row.get(6)?)).map_err(|error| super::corrupt(error.to_string()))?,
        ),
        SubjectKind::Process => Subject::Process,
        SubjectKind::Label => Subject::Label(row.get(5)?),
    };
    let site = match enum_column(row, 7)? {
        SiteKind::Invocation => FailureSite::Invocation,
        SiteKind::Host => FailureSite::Host,
        SiteKind::Execution => FailureSite::Execution(ExecutionLocation {
            target: super::decode::target_ref(row.get(8)?)?,
            workspace: path(row.get(9)?),
        }),
    };
    let cause = match enum_column(row, 11)? {
        CauseKind::Io => Cause::Io {
            kind: enum_column(row, 12)?,
            code: row.get(13)?,
            detail: row.get(15)?,
        },
        CauseKind::InvalidArguments => Cause::InvalidArguments(row.get(14)?),
        CauseKind::Denied => Cause::Denied(row.get(14)?),
        CauseKind::Cancelled => Cause::Cancelled,
        CauseKind::Interrupted => Cause::Interrupted,
        CauseKind::InputClosed => Cause::InputClosed,
        CauseKind::Message => Cause::Message(row.get(14)?),
        CauseKind::Json => Cause::Json,
    };
    Ok(Diagnostic {
        context: DiagnosticContext {
            operation: enum_column(row, 2)?,
            subject,
            site,
            effects: enum_column(row, 10)?,
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
                (row.get(0)?, enum_column(row, 1)?),
                PathFact {
                    role: enum_column(row, 2)?,
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
                let key = (row.get(0)?, enum_column(row, 1)?);
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
        job::{JobEnd, JobRole},
        session::{SessionEvent, db::tests::Fixture},
        tool::diagnostic::{Effects, IoKind, Operation, PartialContext, PathRole},
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
                location: ExecutionLocation::root("/workspace".into()),
            },
        );
        fixture.one(
            root,
            SessionEvent::JobFinished {
                job,
                state: JobEnd::Completed,
                diagnostic,
                output_diagnostic,
                images: Vec::new(),
            },
        );
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
            FailureSite::Execution(ExecutionLocation::named(
                "worker".parse().unwrap(),
                native.clone(),
            )),
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
                PartialContext::new(Operation::FinishCapture, Subject::Label("stdout".into()))
                    .at(FailureSite::Host)
                    .effects(Effects::OutputIncomplete)
                    .resolve(),
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
