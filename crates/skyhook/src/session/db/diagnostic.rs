//! Relational encoding of terminal diagnostics.

use std::{collections::HashMap, path::PathBuf};

use libsql::Row;

use super::{
    Db, DbResult, Encoder, corrupt,
    decode::{Events, Query, u64_of},
    enum_column, optional_enum_column, params,
};
use crate::{
    execution::{ExecutionLocation, path_bytes, path_from_bytes},
    identity::JobId,
    named_enum::named_enum,
    tool::diagnostic::{
        Cause, Diagnostic, DiagnosticContext, FailureSite, IoKind, PathFact, Subject,
    },
};

named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
    pub(super) enum SiteKind {
        Invocation = "invocation",
        Host = "host",
        Execution = "execution",
    }
}

/// The columns a [`Subject`] or [`Cause`] stores its fields in.
#[derive(Default)]
struct Columns {
    path: Option<PathBuf>,
    text: Option<String>,
    job: Option<JobId>,
    io_kind: Option<IoKind>,
    io_code: Option<Option<i32>>,
    io_detail: Option<Option<String>>,
}

/// Declare `$kind`, the journal's name for each `$enum` variant, beside the
/// [`Columns`] each variant's fields fill: `split` and `join` convert between them.
macro_rules! stored_enum {
    (
        $enum:ident / $kind:ident {
            $(
                $variant:ident $(($tuple:ident))? $({ $($field:ident: $column:ident),+ })?
                    = $text:literal
            ),+ $(,)?
        }
    ) => {
        named_enum! {
            #[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
            pub(super) enum $kind { $($variant = $text),+ }
        }

        impl $enum {
            fn split(&self) -> ($kind, Columns) {
                match self {
                    $(
                        Self::$variant $(($tuple))? $({ $($field: $column),+ })? => (
                            $kind::$variant,
                            Columns {
                                $($tuple: Some($tuple.to_owned()),)?
                                $($($column: Some($column.to_owned()),)+)?
                                ..Columns::default()
                            },
                        )
                    ),+
                }
            }

            fn join(kind: $kind, columns: Columns) -> Option<Self> {
                Some(match kind {
                    $(
                        $kind::$variant => Self::$variant
                            $((columns.$tuple?))?
                            $({ $($field: columns.$column?),+ })?
                    ),+
                })
            }
        }
    };
}

stored_enum! {
    Subject / SubjectKind {
        None = "none",
        Path(path) = "path",
        WorkingDirectory(path) = "working_directory",
        StagingFile(path) = "staging_file",
        ParentDirectory(path) = "parent_directory",
        DirectoryEntry(path) = "directory_entry",
        Argument(text) = "argument",
        Tool(text) = "tool",
        Job(job) = "job",
        Process = "process",
        Label(text) = "label",
    }
}

stored_enum! {
    Cause / CauseKind {
        Io { kind: io_kind, code: io_code, detail: io_detail } = "io",
        InvalidArguments(text) = "invalid_arguments",
        Denied(text) = "denied",
        Cancelled = "cancelled",
        Interrupted = "interrupted",
        InputClosed = "input_closed",
        Message(text) = "message",
        Json = "json",
        Unavailable { tool: text } = "unavailable",
        UnknownTool { tool: text } = "unknown_tool",
        ModelHidden { tool: text } = "model_hidden",
        ScriptUnavailable { tool: text } = "script_unavailable",
        UnknownJob { job: job } = "unknown_job",
        JobAlreadyTerminal { job: job } = "job_already_terminal",
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
        let (subject, subject_columns) = context.subject.split();
        let (site, target, workspace) = match &context.site {
            FailureSite::Invocation => (SiteKind::Invocation, None, None),
            FailureSite::Host => (SiteKind::Host, None, None),
            FailureSite::Execution(location) => (
                SiteKind::Execution,
                Some(self.target(db, &location.target)?),
                Some(path_bytes(&location.workspace)),
            ),
        };
        let (cause, cause_columns) = diagnostic.cause.split();
        db.execute(
            "INSERT INTO job_finish_diagnostic (finish, operation, subject, subject_path, \
             subject_text, subject_job, site, location_target, location_workspace, effects, \
             request, cause, io_kind, io_code, cause_text, io_detail, cause_job) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                finish,
                context.operation,
                subject,
                subject_columns.path.as_deref().map(path_bytes),
                subject_columns.text,
                subject_columns.job.map(JobId::get),
                site,
                target,
                workspace,
                context.effects,
                context.request,
                cause,
                cause_columns.io_kind,
                cause_columns.io_code.flatten().map(i64::from),
                cause_columns.text,
                cause_columns.io_detail.flatten(),
                cause_columns.job.map(JobId::get),
            ],
        )?;
        for (position, fact) in context.paths.iter().enumerate() {
            db.execute(
                "INSERT INTO diagnostic_path (finish, position, role, path) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![finish, position, fact.role, path_bytes(&fact.path)],
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
    let subject = Subject::join(
        enum_column(row, 2)?,
        Columns {
            path: row.get::<Option<Vec<u8>>>(3)?.map(path_from_bytes),
            text: row.get(4)?,
            job: job(row, 5)?,
            ..Columns::default()
        },
    )
    .ok_or_else(|| corrupt("diagnostic subject does not match its columns"))?;
    let site = match enum_column(row, 6)? {
        SiteKind::Invocation => FailureSite::Invocation,
        SiteKind::Host => FailureSite::Host,
        SiteKind::Execution => FailureSite::Execution(ExecutionLocation {
            target: super::decode::parsed(row.get(7)?)?,
            workspace: path_from_bytes(row.get(8)?),
        }),
    };
    let cause = Cause::join(
        enum_column(row, 11)?,
        Columns {
            io_kind: optional_enum_column(row, 12)?,
            io_code: Some(row.get(13)?),
            text: row.get(14)?,
            io_detail: Some(row.get(15)?),
            job: job(row, 16)?,
            ..Columns::default()
        },
    )
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
                path: path_from_bytes(row.get(2)?),
            },
        ))
    })? {
        paths.entry(key).or_default().push(fact);
    }
    let Query { sql, params } = events.scoped(
        "SELECT d.finish, d.operation, d.subject, d.subject_path, d.subject_text, \
         d.subject_job, d.site, t.name, d.location_workspace, d.effects, d.request, d.cause, \
         d.io_kind, d.io_code, d.cause_text, d.io_detail, d.cause_job \
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
    use crate::{
        job::{JobEnd, JobRole},
        session::{SessionEvent, db::tests::Fixture},
        tool::diagnostic::{Effects, IoKind, Operation, PartialContext, PathRole, RequestEffects},
    };

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
    fn every_variant_and_native_path_round_trips() {
        let mut fixture = Fixture::new();
        fixture.start("/workspace");
        let native = path_from_bytes(b"/remote-\xfe".to_vec());
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
            Cause::UnknownJob {
                job: JobId::new(999_998).unwrap(),
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
            let io = Diagnostic::new(
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
            finish(&mut fixture, Some(diagnostic));
            finish(&mut fixture, Some(io));
        }
        finish(&mut fixture, None);
        fixture.assert_round_trip();
    }
}
