//! What a reporter writes around a finding's facts, kept with the finding so its report can
//! be rendered again from the store alone, and the artifact copies the report states.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook::{
    db::{Db, enum_column, params},
    media::BlobDigest,
};

use super::*;

/// Reporter-authored prose for one root flaw's finding; every fact around it comes from the
/// store. Use credential references/placeholders, never secrets.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReportInput {
    pub root: FlawId,
    pub title: Text,
    pub summary: Text,
    pub analysis: Text,
    pub impact: Text,
    pub remediation: Text,
    pub reproduction: Reproduction,
    /// At most one per scenario the report states: why it matters and what it needs. Read
    /// back by candidate.
    #[serde(default)]
    pub notes: Vec<ScenarioNote>,
    /// At most one per test the report states: what the run showed. Read back by PoC.
    #[serde(default)]
    pub narratives: Vec<PocNarrative>,
    /// Sections beyond the fixed outline, after the remediation, in order.
    #[serde(default)]
    pub sections: Vec<Section>,
}

impl ReportInput {
    /// The write-up as the store reads it back: notes by candidate, narratives by PoC, and
    /// each step's citations without repeats, first citation first.
    pub fn canonical(mut self) -> WriteUp {
        self.notes.sort_by_key(|note| note.candidate);
        self.narratives.sort_by_key(|narrative| narrative.poc);
        for step in &mut self.reproduction.steps {
            let mut cited = std::collections::HashSet::new();
            step.artifacts
                .retain(|citation| cited.insert(citation.clone()));
        }
        WriteUp(self)
    }
}

/// A write-up in the form the store reads back, so one that was stored compares equal.
#[derive(Clone, Debug, PartialEq)]
pub struct WriteUp(ReportInput);

impl std::ops::Deref for WriteUp {
    type Target = ReportInput;

    fn deref(&self) -> &ReportInput {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScenarioNote {
    pub candidate: CandidateId,
    pub note: Text,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PocNarrative {
    pub poc: PocId,
    pub narrative: Text,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Section {
    /// One line.
    pub heading: Text,
    pub body: Text,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Reproduction {
    /// Setup instructions and credential reference names, not credential values.
    pub prerequisites: Vec<Text>,
    #[schemars(length(min = 1))]
    pub steps: Vec<ReproductionStep>,
    pub outcome: ReproductionOutcome,
    /// Instructions only. The generator never performs cleanup or executes commands.
    pub cleanup: Vec<Text>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReproductionStep {
    pub instruction: Text,
    pub expected_result: Text,
    /// The recorded artifacts the step uses, each of a test the report states.
    #[serde(default)]
    pub artifacts: Vec<ArtifactCitation>,
}

/// A recorded artifact of a test: the PoC and the artifact's reference as `record_poc` took it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactCitation {
    pub poc: PocId,
    pub reference: ArtifactReference,
}

/// Observed results require the headline demonstrated by a confirmed PoC in the current store
/// snapshot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReproductionOutcome {
    Unobserved {
        expected_result: Text,
        limitations: Text,
    },
    Observed {
        observed_result: Text,
    },
}

/// One artifact of a stated test as a finding copies it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactCopy {
    pub poc: PocId,
    /// Its place among the test's artifacts.
    pub position: u32,
    pub reference: ArtifactReference,
    pub role: ArtifactRole,
    /// Below the test's folder: the role's folder and the artifact's own name.
    pub path: String,
    pub bytes: u64,
    pub sha256: BlobDigest,
}

fn instructions(db: &Db, finding: FindingId, part: InstructionPart) -> Result<Vec<Text>> {
    rows(
        db,
        "SELECT text FROM finding_instruction WHERE finding=?1 AND part=?2 ORDER BY position",
        params![finding.0, part],
        |row| Ok(row.get::<String>(0)?.parse()?),
    )
}

fn insert_instructions(
    db: &Db,
    finding: FindingId,
    part: InstructionPart,
    texts: &[Text],
) -> Result<()> {
    for (index, text) in texts.iter().enumerate() {
        db.execute(
            "INSERT INTO finding_instruction(finding,part,position,text) VALUES (?1,?2,?3,?4)",
            params![finding.0, part, index, text],
        )?;
    }
    Ok(())
}

/// Store `copies` with new finding `id` of `scenarios`, refusing a copy of a test it does not
/// state.
pub(super) fn insert_copies(
    db: &Db,
    id: FindingId,
    scenarios: &[ScenarioKey],
    copies: &[ArtifactCopy],
) -> Result<()> {
    for copy in copies {
        if !scenarios
            .iter()
            .any(|scenario| scenario.poc == Some(copy.poc))
        {
            return Err(Error::UnstatedTest { poc: copy.poc });
        }
        db.execute(
            "INSERT INTO finding_artifact(finding,poc,position,path,bytes,sha256) VALUES (?1,?2,?3,?4,?5,?6)",
            params![id.0, copy.poc.0, copy.position, &copy.path, copy.bytes as i64, copy.sha256],
        )?;
    }
    Ok(())
}

/// Remove finding `id`'s write-up, keeping its artifact copies.
pub(super) fn clear(db: &Db, id: FindingId) -> Result<()> {
    for table in [
        "DELETE FROM finding_step_artifact WHERE finding=?1",
        "DELETE FROM finding_step WHERE finding=?1",
        "DELETE FROM finding_instruction WHERE finding=?1",
        "DELETE FROM finding_section WHERE finding=?1",
        "DELETE FROM finding_note WHERE finding=?1",
        "DELETE FROM finding_narrative WHERE finding=?1",
    ] {
        db.execute(table, params![id.0])?;
    }
    Ok(())
}

/// Store `input` as finding `id`'s write-up of `scenarios`, its steps citing `copies`,
/// refusing a note on a scenario it does not report, a narrative of a test it does not
/// state, and a second note or narrative of one.
pub(super) fn insert(
    db: &Db,
    id: FindingId,
    scenarios: &[ScenarioKey],
    input: &WriteUp,
    copies: &[ArtifactCopy],
) -> Result<()> {
    let stated = |poc: PocId| scenarios.iter().any(|scenario| scenario.poc == Some(poc));
    insert_instructions(
        db,
        id,
        InstructionPart::Prerequisite,
        &input.reproduction.prerequisites,
    )?;
    for (index, step) in input.reproduction.steps.iter().enumerate() {
        db.execute(
            "INSERT INTO finding_step(finding,position,instruction,expected_result) VALUES (?1,?2,?3,?4)",
            params![id.0, index, &step.instruction, &step.expected_result],
        )?;
        for cited in &step.artifacts {
            let copy = (copies.iter())
                .find(|copy| copy.poc == cited.poc && copy.reference == cited.reference)
                .ok_or_else(|| Error::UnstatedArtifact {
                    poc: cited.poc,
                    reference: cited.reference.clone(),
                })?;
            db.execute(
                "INSERT INTO finding_step_artifact(finding,step,poc,position) VALUES (?1,?2,?3,?4)",
                params![id.0, index, copy.poc.0, copy.position],
            )?;
        }
    }
    insert_instructions(
        db,
        id,
        InstructionPart::Cleanup,
        &input.reproduction.cleanup,
    )?;
    for (index, section) in input.sections.iter().enumerate() {
        db.execute(
            "INSERT INTO finding_section(finding,position,heading,body) VALUES (?1,?2,?3,?4)",
            params![id.0, index, &section.heading, &section.body],
        )?;
    }
    for ScenarioNote { candidate, note } in &input.notes {
        if !(scenarios.iter()).any(|scenario| scenario.candidate == *candidate) {
            return Err(Error::UnreportedScenario {
                candidate: *candidate,
            });
        }
        let inserted = db.execute(
            "INSERT INTO finding_note(finding,candidate,note) VALUES (?1,?2,?3) ON CONFLICT DO NOTHING",
            params![id.0, candidate.0, note],
        )?;
        if inserted == 0 {
            return Err(Error::RepeatedProse {
                record: Record::Candidate,
                id: candidate.0,
            });
        }
    }
    for PocNarrative { poc, narrative } in &input.narratives {
        if !stated(*poc) {
            return Err(Error::UnstatedTest { poc: *poc });
        }
        let inserted = db.execute(
            "INSERT INTO finding_narrative(finding,poc,narrative) VALUES (?1,?2,?3) ON CONFLICT DO NOTHING",
            params![id.0, poc.0, narrative],
        )?;
        if inserted == 0 {
            return Err(Error::RepeatedProse {
                record: Record::Poc,
                id: poc.0,
            });
        }
    }
    Ok(())
}

/// The prose finding `id` was recorded with.
pub(super) fn input(db: &Db, id: FindingId) -> Result<ReportInput> {
    let text =
        |row: &libsql::Row, index| -> Result<Text> { Ok(row.get::<String>(index)?.parse()?) };
    let optional_text = |row: &libsql::Row, index| -> Result<Option<Text>> {
        row.get::<Option<String>>(index)?
            .map(|value| value.parse().map_err(Error::from))
            .transpose()
    };
    let mut input = one(
        db,
        "SELECT root,title,summary,analysis,impact,remediation,observed_result,expected_result,\
         limitations FROM finding WHERE id=?1",
        id,
        |row| {
            let outcome = match (
                optional_text(row, 6)?,
                optional_text(row, 7)?,
                optional_text(row, 8)?,
            ) {
                (Some(observed_result), None, None) => {
                    ReproductionOutcome::Observed { observed_result }
                }
                (None, Some(expected_result), Some(limitations)) => {
                    ReproductionOutcome::Unobserved {
                        expected_result,
                        limitations,
                    }
                }
                _ => return Err(skyhook::db::corrupt("finding outcome columns disagree").into()),
            };
            Ok(ReportInput {
                root: FlawId(row.get(0)?),
                title: text(row, 1)?,
                summary: text(row, 2)?,
                analysis: text(row, 3)?,
                impact: text(row, 4)?,
                remediation: text(row, 5)?,
                reproduction: Reproduction {
                    prerequisites: Vec::new(),
                    steps: Vec::new(),
                    outcome,
                    cleanup: Vec::new(),
                },
                notes: Vec::new(),
                narratives: Vec::new(),
                sections: Vec::new(),
            })
        },
    )?;
    input.reproduction.prerequisites = instructions(db, id, InstructionPart::Prerequisite)?;
    input.reproduction.cleanup = instructions(db, id, InstructionPart::Cleanup)?;
    input.reproduction.steps = rows(
        db,
        "SELECT instruction,expected_result FROM finding_step WHERE finding=?1 ORDER BY position",
        params![id.0],
        |row| {
            Ok(ReproductionStep {
                instruction: text(row, 0)?,
                expected_result: text(row, 1)?,
                artifacts: Vec::new(),
            })
        },
    )?;
    for (step, citation) in rows(
        db,
        "SELECT s.step,s.poc,a.reference FROM finding_step_artifact s JOIN poc_artifact a ON \
         a.poc=s.poc AND a.position=s.position WHERE s.finding=?1 ORDER BY s.rowid",
        params![id.0],
        |row| {
            Ok((
                row.get::<i64>(0)? as usize,
                ArtifactCitation {
                    poc: PocId(row.get(1)?),
                    reference: row.get::<String>(2)?.parse()?,
                },
            ))
        },
    )? {
        (input.reproduction.steps.get_mut(step))
            .ok_or_else(|| skyhook::db::corrupt("a cited step is not stored"))?
            .artifacts
            .push(citation);
    }
    input.sections = rows(
        db,
        "SELECT heading,body FROM finding_section WHERE finding=?1 ORDER BY position",
        params![id.0],
        |row| {
            Ok(Section {
                heading: text(row, 0)?,
                body: text(row, 1)?,
            })
        },
    )?;
    input.notes = rows(
        db,
        "SELECT candidate,note FROM finding_note WHERE finding=?1 ORDER BY candidate",
        params![id.0],
        |row| {
            Ok(ScenarioNote {
                candidate: CandidateId(row.get(0)?),
                note: text(row, 1)?,
            })
        },
    )?;
    input.narratives = rows(
        db,
        "SELECT poc,narrative FROM finding_narrative WHERE finding=?1 ORDER BY poc",
        params![id.0],
        |row| {
            Ok(PocNarrative {
                poc: PocId(row.get(0)?),
                narrative: text(row, 1)?,
            })
        },
    )?;
    Ok(input)
}

/// The artifact copies finding `id` states, by test and position.
pub(super) fn copies(db: &Db, id: FindingId) -> Result<Vec<ArtifactCopy>> {
    rows(
        db,
        "SELECT c.poc,c.position,a.reference,a.role,c.path,c.bytes,c.sha256 FROM finding_artifact c \
         JOIN poc_artifact a ON a.poc=c.poc AND a.position=c.position WHERE c.finding=?1 ORDER BY \
         c.poc,c.position",
        params![id.0],
        |row| {
            let digest = bytes(row.get(6)?)?;
            Ok(ArtifactCopy {
                poc: PocId(row.get(0)?),
                position: row.get::<i64>(1)? as u32,
                reference: row.get::<String>(2)?.parse()?,
                role: enum_column(row, 3)?,
                path: row.get(4)?,
                bytes: row.get::<i64>(5)?.max(0) as u64,
                sha256: BlobDigest::from_bytes(digest),
            })
        },
    )
}
