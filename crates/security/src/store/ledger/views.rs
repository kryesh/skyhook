//! The views the research tables derive standings with.

/// The views deriving each node's standing from its results and what they rest on.
macro_rules! derived_views {
    () => {
        concat!(
            "
-- A premise's or flaw's standing is its latest status set by the work owning it: a code
-- premise's settle, a deployment premise's answer outside every task, a flaw's chain. It reads
-- undecided or suspected while none was, or after a material edit since. A later status set
-- elsewhere that differs from it is a proposal, which disputes it, unless its work already
-- answered that status proposed before.
CREATE VIEW premise_standing AS
SELECT s.*,",
            $crate::store::ledger::standing::proposed!(
                "premise_decision",
                "premise",
                "s.node",
                "s.decided",
                "status",
                "s.status",
                "settle"
            ),
            " AS proposed FROM (SELECT p.node,p.scope,p.location,d.event AS decided,CASE WHEN \
             d.event>=ifnull(",
            $crate::store::ledger::standing::edited!("settle", "p.node"),
            ",0) THEN d.status ELSE 'undecided' END AS status FROM premise p LEFT JOIN \
             premise_decision d ON d.event=",
            $crate::store::ledger::standing::owned!("premise_decision", "premise", "p.node"),
            ") s;
CREATE VIEW flaw_standing AS
SELECT s.*,",
            $crate::store::ledger::standing::proposed!(
                "flaw_judgement",
                "flaw",
                "s.node",
                "s.judged",
                "status",
                "s.status",
                "chain"
            ),
            " AS proposed FROM (SELECT f.node,f.component,f.location,f.cwe,d.event AS judged,CASE \
             WHEN d.event>=ifnull(",
            $crate::store::ledger::standing::edited!("chain", "f.node"),
            ",0) THEN d.status ELSE 'suspected' END AS status FROM flaw f LEFT JOIN flaw_judgement \
             d ON d.event=",
            $crate::store::ledger::standing::owned!("flaw_judgement", "flaw", "f.node"),
            ") s;

-- Each candidate's standing assessment and verdict, the latest recorded within review work on
-- it, and the latest of each recorded since elsewhere that differs and that work has not
-- answered: a proposal its review decides.
CREATE VIEW candidate_result AS
SELECT c.node AS candidate,a.event AS assessed,a.vector,a.severity,",
            $crate::store::ledger::standing::proposed!(
                "candidate_assessment",
                "candidate",
                "c.node",
                "a.event",
                "vector",
                "a.vector",
                "review"
            ),
            " AS assessment_proposed,v.event AS reviewed,v.verdict,",
            $crate::store::ledger::standing::proposed!(
                "candidate_verdict",
                "candidate",
                "c.node",
                "v.event",
                "verdict",
                "v.verdict",
                "review"
            ),
            " AS verdict_proposed FROM candidate c LEFT JOIN candidate_assessment a ON a.event=",
            $crate::store::ledger::standing::owned!("candidate_assessment", "candidate", "c.node"),
            " LEFT JOIN candidate_verdict v ON v.event=",
            $crate::store::ledger::standing::owned!("candidate_verdict", "candidate", "c.node"),
            ";

-- Each candidate's results judged against what they rest on. An assessment is current while no
-- edit or merge of the candidate itself came after what it stands on: what it says, not what it
-- rests on. A verdict is current while no event of its review's bearing but a proposal
-- disputing it did, and only while the standings it claims hold: validated, no premise of its
-- chain undecided or failing and no chain flaw refuted; disproved, a code premise failing or a
-- chain flaw refuted; excluded, a deployment premise failing; unproven, a premise undecided.
CREATE VIEW candidate_core AS
SELECT b.*,b.assessed_current AND b.reviewed_current AND ifnull(b.verdict='validated',0) AS upheld
FROM (SELECT g.*,g.reviewed_at_basis AND CASE g.verdict WHEN 'validated' THEN NOT (g.undecided OR
    g.contradicted) WHEN 'disproved' THEN g.refuted OR g.code_fails WHEN 'excluded' THEN
    g.deployment_fails ELSE g.undecided END AS reviewed_current,
    g.confirmed AND NOT g.contradicted AS demonstrated
FROM (SELECT f.*,f.refuted OR f.code_fails OR f.deployment_fails AS contradicted FROM (
SELECT c.candidate,n.merged_into,r.root,c.severity,k.rank,
    k.rank>=(SELECT t.rank FROM run JOIN severity_kind t ON t.name=run.threshold) AS shortlisted,
    c.vector,c.assessed,c.assessment_proposed,
    ifnull(",
            $crate::store::ledger::standing::stamp!("review", "c.assessed", "c.candidate"),
            ">=ifnull(",
            $crate::store::ledger::standing::edited!(own "review", "c.candidate"),
            ",0),0) AS assessed_current,
    c.verdict,c.reviewed,c.verdict_proposed,
    ifnull(",
            $crate::store::ledger::standing::stamp!("review", "c.reviewed", "c.candidate"),
            ">=",
            $crate::store::ledger::bearing::basis!(rested "review", "c.candidate"),
            ",0) AS reviewed_at_basis,
    EXISTS (SELECT 1 FROM candidate_flaw rf JOIN flaw_standing fs ON fs.node=rf.flaw
        WHERE rf.candidate=c.candidate AND fs.status='refuted') AS refuted,
    EXISTS (SELECT 1 FROM (",
            $crate::store::ledger::facts::chain_premises!("c.candidate"),
            ") cp JOIN premise_standing p ON p.node=cp.premise WHERE p.status='fails' AND
        p.scope='code') AS code_fails,
    EXISTS (SELECT 1 FROM (",
            $crate::store::ledger::facts::chain_premises!("c.candidate"),
            ") cp JOIN premise_standing p ON p.node=cp.premise WHERE p.status='fails' AND
        p.scope='deployment') AS deployment_fails,
    EXISTS (SELECT 1 FROM (",
            $crate::store::ledger::facts::chain_premises!("c.candidate"),
            ") cp JOIN premise_standing p ON p.node=cp.premise WHERE p.status='undecided') AS undecided,
    ifnull((SELECT result='confirmed' FROM test WHERE id=",
            $crate::store::ledger::facts::settling_test!("c.candidate"),
            "),0) AS confirmed
FROM candidate_result c JOIN node n ON n.id=c.candidate LEFT JOIN candidate_root r ON
    r.candidate=c.candidate LEFT JOIN severity_kind k ON k.name=c.severity) f) g) b;

-- Each candidate's standing, derived. A validated candidate is noted while another live
-- member of its root group stands validated and demonstrated at its severity or above. For
-- display, its standing as its results last left it, and whether one of them no longer stands.
CREATE VIEW candidate_state AS
SELECT s.*,",
            $crate::store::ledger::facts::state!($crate::store::ledger::facts::noted!()),
            " AS state,",
            $crate::store::ledger::facts::standing_state!($crate::store::ledger::facts::noted!()),
            " AS standing,
    s.merged_into IS NULL AND s.severity IS NOT NULL AND (NOT s.assessed_current OR
        (s.verdict IS NOT NULL AND NOT s.reviewed_current)) AS stale,
    CASE WHEN s.merged_into IS NOT NULL OR NOT s.assessed_current THEN NULL
        WHEN s.upheld THEN
            CASE WHEN s.demonstrated THEN 'demonstrated' ELSE 'confirmed' END
        WHEN s.undecided OR s.contradicted THEN 'conjectured'
        ELSE 'established' END AS confidence
FROM candidate_core s;

-- The scenarios a root's finding reports.
CREATE VIEW finding_scenario AS
SELECT r.root,s.candidate,s.state,s.confidence,s.severity,s.rank
FROM candidate_root r JOIN candidate_state s ON s.candidate=r.candidate
WHERE s.state IN ('validated','noted');
"
        )
    };
}
pub(in crate::store) use derived_views;
