//! Reports: one finding per root flaw, written in place.

use skyhook::db::params;

use crate::store::*;

impl Store {
    /// Record the report of root flaw `root`, or the flaw it was merged into: one finding per
    /// root, rewritten in place.
    pub(crate) fn report(
        &self,
        actor: &AgentId,
        root: FlawId,
        title: &Text,
        body: &Text,
    ) -> Result<FindingId> {
        self.write(|db| {
            let root = live(db, root)?;
            audit(db, actor, EventKind::Reported, None, |event| {
                let existing = optional(
                    db,
                    "SELECT node FROM finding WHERE root=?1",
                    params![root],
                    |row| Ok(FindingId(row.get(0)?)),
                )?;
                let finding = match existing {
                    Some(finding) => {
                        db.execute(
                            "UPDATE node SET title=?2,body=?3 WHERE id=?1",
                            params![finding, title, body],
                        )?;
                        db.execute(
                            "UPDATE finding SET reported=?2 WHERE node=?1",
                            params![finding, event],
                        )?;
                        finding
                    }
                    None => {
                        let finding = FindingId(db.insert(
                            "INSERT INTO node(kind,title,body) VALUES (?1,?2,?3)",
                            params![NodeKind::Finding, title, body],
                        )?);
                        db.execute(
                            "INSERT INTO finding(node,root,reported) VALUES (?1,?2,?3)",
                            params![finding, root, event],
                        )?;
                        finding
                    }
                };
                crate::store::ledger::bearing::record_scenarios(db, event, root)?;
                let mut marks = Marks::of(finding, EventEffect::Changed);
                for candidate in rows(
                    db,
                    "SELECT r.candidate FROM candidate_root r JOIN node n ON n.id=r.candidate WHERE \
                     r.root=?1 AND n.merged_into IS NULL",
                    params![root],
                    |row| Ok(NodeId(row.get(0)?)),
                )? {
                    marks.mark(candidate, EventEffect::Concerns);
                }
                Ok((finding, marks))
            })
        })
    }
}
