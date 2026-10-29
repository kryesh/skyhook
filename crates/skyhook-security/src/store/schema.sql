-- Dictionaries are filled from the Rust enums when the database is created.
CREATE TABLE poc_policy_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE severity_kind(name TEXT PRIMARY KEY, rank INTEGER NOT NULL UNIQUE) STRICT, WITHOUT ROWID;
CREATE TABLE confidence_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE evidence_role(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE evidence_stance(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE relation_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE verdict_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE rejection_reason(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE poc_result_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE artifact_role(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE event_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE credential_reference_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE source_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE instruction_part(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE trust_kind(
    name TEXT PRIMARY KEY,
    untrusted INTEGER NOT NULL CHECK(untrusted IN (0,1))
) STRICT, WITHOUT ROWID;
CREATE TABLE invariant_kind(
    name TEXT PRIMARY KEY,
    load_bearing INTEGER NOT NULL CHECK(load_bearing IN (0,1))
) STRICT, WITHOUT ROWID;
CREATE TABLE misreport_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE retry_class(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE unit_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE outcome_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE operation_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE guard_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE deficiency_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE property_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE resolution_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE exposure_outcome(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE route_basis(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE gate_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE cvss_metric(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE deployment_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE deployment_stance(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE tree_state(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

-- The session is the run: one row, written as the session opens, with the revision it started
-- at and the PoC policy of the launch working it now.
CREATE TABLE run(
    id INTEGER PRIMARY KEY CHECK(id=1),
    created INTEGER NOT NULL,
    threshold TEXT NOT NULL REFERENCES severity_kind(name),
    workers INTEGER NOT NULL CHECK(workers BETWEEN 1 AND 65535),
    revision TEXT,
    tree TEXT NOT NULL REFERENCES tree_state(name),
    poc TEXT NOT NULL REFERENCES poc_policy_kind(name)
) STRICT;
-- An agent of the session.
CREATE TABLE actor(
    id INTEGER PRIMARY KEY,
    agent TEXT NOT NULL UNIQUE,
    -- The journal's row for the agent; none only for an agent the journal never started.
    session_agent INTEGER REFERENCES agent(id)
) STRICT;

-- A code region, in the function containing it when it lies in one: evidence, routes, entry
-- points, retry sites and deferred regions share it.
CREATE TABLE location(
    id INTEGER PRIMARY KEY,
    file TEXT NOT NULL,
    start_line INTEGER NOT NULL CHECK(start_line BETWEEN 1 AND 4294967295),
    end_line INTEGER NOT NULL CHECK(end_line BETWEEN start_line AND 4294967295),
    function TEXT,
    UNIQUE(file,start_line,end_line,function)
) STRICT;
CREATE UNIQUE INDEX location_unnamed ON location(file,start_line,end_line) WHERE function IS NULL;

-- The latest scope is the run's policy; a verdict excluding by policy records it.
CREATE TABLE scope(id INTEGER PRIMARY KEY, description TEXT NOT NULL) STRICT;
-- A deployment fact a scope states: a build flag, platform, host behaviour or input bound
-- the deployment in scope admits, or one it excludes. A condition resolved as holding may
-- cite what the current scope admits, and a policy exclusion what it excludes.
CREATE TABLE deployment(
    id INTEGER PRIMARY KEY,
    scope INTEGER NOT NULL REFERENCES scope(id),
    kind TEXT NOT NULL REFERENCES deployment_kind(name),
    stance TEXT NOT NULL REFERENCES deployment_stance(name),
    statement TEXT NOT NULL
) STRICT;
CREATE INDEX deployment_scope ON deployment(scope,id);
-- An actor that can drive input or for whom an operation is intended, at one trust level.
CREATE TABLE principal(
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    trust TEXT NOT NULL REFERENCES trust_kind(name),
    description TEXT NOT NULL
) STRICT;
CREATE TABLE source(
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL REFERENCES source_kind(name),
    trust TEXT NOT NULL REFERENCES trust_kind(name),
    entry INTEGER NOT NULL REFERENCES location(id),
    description TEXT NOT NULL
) STRICT;
-- Where a source's input is handed into implementation: the first call beyond parsing and
-- dispatch, calling `calls`.
CREATE TABLE source_handoff(
    source INTEGER PRIMARY KEY REFERENCES source(id),
    location INTEGER NOT NULL REFERENCES location(id),
    calls TEXT NOT NULL
) STRICT;
CREATE TABLE source_principal(
    source INTEGER NOT NULL REFERENCES source(id),
    principal INTEGER NOT NULL REFERENCES principal(id),
    PRIMARY KEY(source,principal)
) STRICT, WITHOUT ROWID;
CREATE INDEX source_principal_principal ON source_principal(principal,source);

-- Implementation code swept once, whatever reaches it. A file has one owner, and moves to
-- another component only until a completed sweep covers it.
CREATE TABLE component(
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL
) STRICT;
-- `joined` is the journal's latest entry when the file joined its component, so a sweep
-- covers only the files its component held when the sweep's agent started.
CREATE TABLE component_file(
    file TEXT PRIMARY KEY,
    component INTEGER NOT NULL REFERENCES component(id),
    joined INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
CREATE INDEX component_file_component ON component_file(component,file);
-- Sources sharing one interpretation up to their handoff into implementation, reviewed
-- once. A membership change bumps its revision, which every decision about it records.
CREATE TABLE path(
    id INTEGER PRIMARY KEY,
    description TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1 CHECK(revision>0)
) STRICT;
CREATE TABLE path_source(
    path INTEGER NOT NULL REFERENCES path(id),
    source INTEGER NOT NULL REFERENCES source(id),
    PRIMARY KEY(path,source)
) STRICT, WITHOUT ROWID;
CREATE INDEX path_source_source ON path_source(source,path);
CREATE TABLE path_reach(
    path INTEGER NOT NULL REFERENCES path(id),
    component INTEGER NOT NULL REFERENCES component(id),
    note TEXT NOT NULL,
    PRIMARY KEY(path,component)
) STRICT, WITHOUT ROWID;
CREATE INDEX path_reach_component ON path_reach(component,path);

-- The review plan: one component's sweep, one path's review, a question (a contract
-- check among them) or a follow-up. A unit is open until its one
-- outcome is recorded, and nothing reopens it.
CREATE TABLE plan_unit(
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL REFERENCES unit_kind(name),
    component INTEGER REFERENCES component(id),
    path INTEGER REFERENCES path(id),
    question TEXT,
    stopping TEXT,
    -- The candidate a follow-up concerns; its tests wait while the follow-up is open.
    candidate INTEGER REFERENCES candidate(id),
    -- The flaw a follow-up traces, asking whether it breaks the unit's guarantees and premises.
    flaw INTEGER REFERENCES flaw(id),
    actor INTEGER NOT NULL REFERENCES actor(id),
    at INTEGER NOT NULL,
    CHECK(CASE kind
        WHEN 'sweep' THEN component IS NOT NULL AND path IS NULL AND question IS NULL
        WHEN 'path_review' THEN path IS NOT NULL AND component IS NULL AND question IS NULL
        ELSE component IS NULL AND path IS NULL AND question IS NOT NULL END),
    CHECK((question IS NULL)=(stopping IS NULL)),
    CHECK(candidate IS NULL OR kind='follow_up'),
    CHECK(flaw IS NULL OR kind='follow_up')
) STRICT;
CREATE INDEX plan_unit_by_kind ON plan_unit(kind,id);
CREATE INDEX plan_unit_component ON plan_unit(component,id) WHERE component IS NOT NULL;
CREATE INDEX plan_unit_path ON plan_unit(path,id) WHERE path IS NOT NULL;
CREATE INDEX plan_unit_candidate ON plan_unit(candidate,id) WHERE candidate IS NOT NULL;
CREATE INDEX plan_unit_flaw ON plan_unit(flaw,id) WHERE flaw IS NOT NULL;
-- The guarantees and premises a question or follow-up asks about: with its flaw, whether the
-- flaw breaks them; without one, the premises of its candidate it settles. A premise is named
-- at its revision when the unit was planned: a follow-up settling it that ended at its current
-- revision exhausts it, so nothing plans another until a change reopens it.
CREATE TABLE unit_invariant(
    unit INTEGER NOT NULL REFERENCES plan_unit(id),
    guarantee INTEGER REFERENCES guarantee(id),
    premise INTEGER REFERENCES premise(id),
    revision INTEGER,
    CHECK((guarantee IS NULL)!=(premise IS NULL)),
    CHECK((premise IS NULL)=(revision IS NULL)),
    UNIQUE(unit,guarantee),
    UNIQUE(unit,premise)
) STRICT;
CREATE INDEX unit_invariant_guarantee ON unit_invariant(guarantee,unit) WHERE guarantee IS NOT NULL;
CREATE INDEX unit_invariant_premise ON unit_invariant(premise,unit) WHERE premise IS NOT NULL;
-- What each unit is about, one record a row in `rank` then `position` order: the flaw and the
-- candidate it names, the guarantees and premises it asks about, then the function a check
-- asks about.
CREATE VIEW unit_about AS
SELECT id AS unit,0 AS rank,0 AS position,flaw,NULL AS candidate,NULL AS guarantee,
    NULL AS premise,NULL AS component,NULL AS function FROM plan_unit WHERE flaw IS NOT NULL
UNION ALL SELECT id,1,0,NULL,candidate,NULL,NULL,NULL,NULL FROM plan_unit
    WHERE candidate IS NOT NULL
UNION ALL SELECT unit,2,rowid,NULL,NULL,guarantee,premise,NULL,NULL FROM unit_invariant
UNION ALL SELECT k.unit,3,0,NULL,NULL,NULL,NULL,c.component,c.function FROM unit_check k
    JOIN contract c ON c.id=k.contract;
CREATE TABLE outcome(
    id INTEGER PRIMARY KEY,
    unit INTEGER NOT NULL UNIQUE REFERENCES plan_unit(id),
    kind TEXT NOT NULL REFERENCES outcome_kind(name),
    note TEXT NOT NULL,
    actor INTEGER NOT NULL REFERENCES actor(id),
    at INTEGER NOT NULL
) STRICT;
-- The premises an inconclusive question or follow-up left open, and for each the sites it
-- rests on that the unit did not trace.
CREATE TABLE outcome_premise(
    outcome INTEGER NOT NULL REFERENCES outcome(id),
    premise INTEGER NOT NULL REFERENCES premise(id),
    PRIMARY KEY(outcome,premise)
) STRICT, WITHOUT ROWID;
CREATE TABLE outcome_untraced(
    outcome INTEGER NOT NULL,
    premise INTEGER NOT NULL,
    location INTEGER NOT NULL REFERENCES location(id),
    PRIMARY KEY(outcome,premise,location),
    FOREIGN KEY(outcome,premise) REFERENCES outcome_premise(outcome,premise)
) STRICT, WITHOUT ROWID;
-- A function of a component and the kind of contract it keeps across calls, which question units
-- check (`unit_check`), one open at a time, with the function producing what it is about and
-- the input it consumes when known.
CREATE TABLE contract(
    id INTEGER PRIMARY KEY,
    component INTEGER NOT NULL REFERENCES component(id),
    function TEXT NOT NULL,
    kind TEXT NOT NULL REFERENCES invariant_kind(name),
    UNIQUE(component,function,kind)
) STRICT;
CREATE TABLE unit_check(
    unit INTEGER PRIMARY KEY REFERENCES plan_unit(id),
    contract INTEGER NOT NULL REFERENCES contract(id),
    producer TEXT,
    input TEXT
) STRICT;
CREATE INDEX unit_check_contract ON unit_check(contract,unit);
-- Each guarantee a check of a contract names.
CREATE VIEW named_guarantee AS
SELECT DISTINCT k.contract,i.guarantee FROM unit_check k JOIN unit_invariant i ON i.unit=k.unit
WHERE i.guarantee IS NOT NULL;
-- What bears on a contract, by whom and at which event: each record or restatement of a retry
-- site in its function, for a status contract, meaning invalid input or another failure, and
-- each undermining of or contradicting evidence on a guarantee one of its checks names.
CREATE VIEW contract_trigger AS
SELECT c.id AS contract,e.actor,e.id AS event,s.id AS retry_site,NULL AS undermining,
    NULL AS evidence
FROM contract c JOIN component_file cf ON cf.component=c.component JOIN location l ON
    l.file=cf.file AND l.function=c.function JOIN retry_site s ON s.location=l.id JOIN
    event_record r ON r.retry_site=s.id JOIN event e ON e.id=r.event
WHERE c.kind='status_contract' AND s.class IN ('invalid','other')
UNION ALL SELECT n.contract,e.actor,e.id,NULL,u.id,NULL FROM named_guarantee n JOIN undermining
    u ON u.guarantee=n.guarantee JOIN event_record r ON r.undermining=u.id JOIN event e ON
    e.id=r.event
UNION ALL SELECT n.contract,e.actor,e.id,NULL,NULL,v.id FROM named_guarantee n JOIN evidence v
    ON v.guarantee=n.guarantee AND v.stance='contradicts' JOIN event_record r ON
    r.evidence=v.id JOIN event e ON e.id=r.event;
-- Each contract by its latest check: broken while a live, unrefuted flaw undermines a guarantee
-- it names; supported once it completed naming guarantees that are each of its kind, of its
-- component and argued in its function with nothing against them, and for a status contract
-- cite every site of each status the function returns; open once it ended inconclusive;
-- otherwise unsettled.
CREATE VIEW contract_standing AS
SELECT c.*,k.unit,(SELECT event FROM event_record WHERE outcome=o.id) AS ended,CASE
    WHEN EXISTS (SELECT 1 FROM unit_invariant i JOIN undermining u ON u.guarantee=i.guarantee
        JOIN flaw_state f ON f.id=u.flaw WHERE i.unit=k.unit AND f.current) THEN 'broken'
    WHEN o.kind='completed' AND EXISTS (SELECT 1 FROM unit_invariant WHERE unit=k.unit
        AND guarantee IS NOT NULL) AND NOT EXISTS (SELECT 1 FROM unit_invariant i JOIN guarantee
        g ON g.id=i.guarantee WHERE i.unit=k.unit AND (g.component!=c.component OR
        g.kind!=c.kind OR EXISTS (SELECT 1 FROM evidence e WHERE e.guarantee=g.id AND
        e.stance='contradicts') OR NOT EXISTS (SELECT 1 FROM evidence e JOIN location l ON
        l.id=e.location WHERE e.guarantee=g.id AND e.stance='supports' AND
        l.function=c.function))) AND NOT EXISTS (SELECT 1 FROM component_file cf JOIN location
        l ON l.file=cf.file JOIN retry_site s ON s.location=l.id WHERE c.kind='status_contract'
        AND cf.component=c.component AND s.status IN (SELECT r.status FROM retry_site r JOIN
        location rl ON rl.id=r.location JOIN component_file rf ON rf.file=rl.file WHERE
        rf.component=c.component AND rl.function=c.function) AND NOT EXISTS (SELECT 1 FROM
        unit_invariant i JOIN evidence e ON e.guarantee=i.guarantee JOIN location el ON
        el.id=e.location WHERE i.unit=k.unit AND el.file=l.file AND el.start_line<=l.start_line
        AND el.end_line>=l.end_line)) THEN 'supported'
    WHEN o.kind='inconclusive' THEN 'open' ELSE 'unsettled' END AS state
FROM contract c LEFT JOIN unit_check k ON k.unit=(SELECT max(unit) FROM unit_check WHERE
    contract=c.id) LEFT JOIN outcome o ON o.unit=k.unit;
-- The files a completed sweep covered: those its component held when the sweep began.
CREATE TABLE swept_file(
    outcome INTEGER NOT NULL REFERENCES outcome(id),
    file TEXT NOT NULL REFERENCES component_file(file),
    PRIMARY KEY(outcome,file)
) STRICT, WITHOUT ROWID;
CREATE INDEX swept_file_file ON swept_file(file,outcome);

-- A site in a component's file returning a status that asks its caller to retry or supply
-- more input, or mapping `producer`'s false or 0 to one, with what the status can mean there;
-- none until classified. A sweep does not complete while a site in its files is unclassified.
-- A region holds one site, whatever function its location names.
CREATE TABLE retry_site(
    id INTEGER PRIMARY KEY,
    location INTEGER NOT NULL UNIQUE REFERENCES location(id),
    status TEXT NOT NULL,
    producer TEXT,
    class TEXT REFERENCES retry_class(name)
) STRICT;
-- An argued invariant of a component, under the preconditions it names, in the function it is
-- argued in; not a certificate. One spanning the attempts a retry status asks for cites every
-- site of that status its component records.
CREATE TABLE guarantee(
    id INTEGER PRIMARY KEY,
    component INTEGER NOT NULL REFERENCES component(id),
    function TEXT,
    kind TEXT NOT NULL REFERENCES invariant_kind(name),
    statement TEXT NOT NULL,
    preconditions TEXT NOT NULL,
    retry_status TEXT
) STRICT;
CREATE INDEX guarantee_component ON guarantee(component,id);
CREATE INDEX guarantee_kind ON guarantee(kind,id);
-- A region of a component or a stretch of a path left unexamined, and why.
CREATE TABLE deferred(
    id INTEGER PRIMARY KEY,
    component INTEGER REFERENCES component(id),
    path INTEGER REFERENCES path(id),
    location INTEGER REFERENCES location(id),
    reason TEXT NOT NULL,
    CHECK((component IS NULL)!=(path IS NULL))
) STRICT;
CREATE INDEX deferred_component ON deferred(component,id) WHERE component IS NOT NULL;
CREATE INDEX deferred_path ON deferred(path,id) WHERE path IS NOT NULL;
-- A deferred region a later examination covered.
CREATE TABLE deferred_settlement(
    deferred INTEGER PRIMARY KEY REFERENCES deferred(id),
    note TEXT NOT NULL,
    actor INTEGER NOT NULL REFERENCES actor(id),
    at INTEGER NOT NULL
) STRICT;
-- The guarantees a settlement rests on, each with evidence inside the region.
CREATE TABLE settlement_guarantee(
    deferred INTEGER NOT NULL REFERENCES deferred_settlement(deferred),
    guarantee INTEGER NOT NULL REFERENCES guarantee(id),
    PRIMARY KEY(deferred,guarantee)
) STRICT, WITHOUT ROWID;

-- A local defect or violated contract. Material changes bump its revision, which every
-- decision about it records; merged flaws keep only their history.
CREATE TABLE flaw(
    id INTEGER PRIMARY KEY,
    component INTEGER NOT NULL REFERENCES component(id),
    title TEXT NOT NULL,
    statement TEXT NOT NULL,
    operation TEXT NOT NULL REFERENCES operation_kind(name),
    guard TEXT REFERENCES guard_kind(name),
    deficiency TEXT REFERENCES deficiency_kind(name),
    -- What the flaw makes its code report for the wrong reason, when it does.
    misreports TEXT REFERENCES misreport_kind(name),
    revision INTEGER NOT NULL DEFAULT 1 CHECK(revision>0),
    merged_into INTEGER REFERENCES flaw(id),
    CHECK((guard IS NULL)=(deficiency IS NULL)),
    CHECK(id!=merged_into)
) STRICT;
CREATE INDEX flaw_component ON flaw(component,id);
CREATE INDEX flaw_merged_into ON flaw(merged_into) WHERE merged_into IS NOT NULL;
CREATE TABLE flaw_effect(
    flaw INTEGER NOT NULL REFERENCES flaw(id),
    property TEXT NOT NULL REFERENCES property_kind(name),
    note TEXT NOT NULL,
    PRIMARY KEY(flaw,property)
) STRICT, WITHOUT ROWID;
-- A flaw is refuted while one of its refutations stands: only evidence contradicting the
-- refutation itself lifts it.
CREATE TABLE flaw_refutation(
    id INTEGER PRIMARY KEY,
    flaw INTEGER NOT NULL REFERENCES flaw(id),
    reason TEXT NOT NULL,
    actor INTEGER NOT NULL REFERENCES actor(id),
    at INTEGER NOT NULL
) STRICT;
CREATE INDEX flaw_refutation_flaw ON flaw_refutation(flaw,id DESC);
-- Each flaw with whether it is refuted, and current: neither merged nor refuted.
CREATE VIEW flaw_state AS
SELECT *,merged_into IS NULL AND NOT refuted AS current FROM (SELECT f.*,EXISTS (SELECT 1 FROM
    flaw_refutation x WHERE x.flaw=f.id AND NOT EXISTS (SELECT 1 FROM evidence v WHERE
    v.refutation=x.id AND v.stance='contradicts')) AS refuted FROM flaw f);
-- Symmetric kinds are stored with a<b; `enables` keeps its direction.
CREATE TABLE relation(
    id INTEGER PRIMARY KEY,
    a INTEGER NOT NULL REFERENCES flaw(id),
    b INTEGER NOT NULL REFERENCES flaw(id),
    kind TEXT NOT NULL REFERENCES relation_kind(name),
    note TEXT NOT NULL,
    CHECK(a!=b),
    UNIQUE(a,b,kind)
) STRICT;
CREATE INDEX relation_b ON relation(b,a);
-- A flaw that breaks a guarantee or premise: it counts as contradicting it.
CREATE TABLE undermining(
    id INTEGER PRIMARY KEY,
    flaw INTEGER NOT NULL REFERENCES flaw(id),
    guarantee INTEGER REFERENCES guarantee(id),
    premise INTEGER REFERENCES premise(id),
    note TEXT NOT NULL,
    actor INTEGER NOT NULL REFERENCES actor(id),
    at INTEGER NOT NULL,
    CHECK((guarantee IS NULL)!=(premise IS NULL)),
    UNIQUE(flaw,guarantee),
    UNIQUE(flaw,premise)
) STRICT;
CREATE INDEX undermining_guarantee ON undermining(guarantee,flaw) WHERE guarantee IS NOT NULL;
CREATE INDEX undermining_premise ON undermining(premise,flaw) WHERE premise IS NOT NULL;

-- A scenario over one or more flaws. Material changes bump its revision, which its
-- assessments record; only a contradiction of the scenario (a member refuted, a scenario
-- condition failing or undermined) bumps its contradictions, which its tests record.
CREATE TABLE candidate(
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    description TEXT NOT NULL,
    cwe INTEGER CHECK(cwe>0),
    revision INTEGER NOT NULL DEFAULT 1 CHECK(revision>0),
    contradictions INTEGER NOT NULL DEFAULT 0 CHECK(contradictions>=0),
    merged_into INTEGER REFERENCES candidate(id),
    CHECK(id!=merged_into)
) STRICT;
CREATE INDEX candidate_merged_into ON candidate(merged_into) WHERE merged_into IS NOT NULL;
CREATE TABLE candidate_member(
    candidate INTEGER NOT NULL REFERENCES candidate(id),
    flaw INTEGER NOT NULL REFERENCES flaw(id),
    step INTEGER CHECK(step>=0),
    PRIMARY KEY(candidate,flaw)
) STRICT, WITHOUT ROWID;
CREATE INDEX candidate_member_flaw ON candidate_member(flaw,candidate);
CREATE TABLE candidate_path(
    candidate INTEGER NOT NULL REFERENCES candidate(id),
    path INTEGER NOT NULL REFERENCES path(id),
    PRIMARY KEY(candidate,path)
) STRICT, WITHOUT ROWID;
CREATE INDEX candidate_path_path ON candidate_path(path,candidate);

-- A condition not yet established, owned by exactly one of the deployment in scope (a
-- run-level condition of one kind), a flaw (a fact about the code reaching or triggering it
-- needs) or a candidate (one of its own scenario). Contradicting evidence bumps its revision.
CREATE TABLE premise(
    id INTEGER PRIMARY KEY,
    deployment INTEGER REFERENCES deployment(id),
    flaw INTEGER REFERENCES flaw(id),
    candidate INTEGER REFERENCES candidate(id),
    kind TEXT NOT NULL REFERENCES invariant_kind(name),
    statement TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1 CHECK(revision>0),
    CHECK((deployment IS NOT NULL)+(flaw IS NOT NULL)+(candidate IS NOT NULL)=1)
) STRICT;
CREATE INDEX premise_flaw ON premise(flaw,id) WHERE flaw IS NOT NULL;
CREATE INDEX premise_candidate ON premise(candidate,id) WHERE candidate IS NOT NULL;
-- A candidate relying on a condition, gating its scenario or only how one CVSS base metric
-- scores: each of its own conditions, and the deployment and flaw conditions it is linked to.
CREATE TABLE candidate_premise(
    candidate INTEGER NOT NULL REFERENCES candidate(id),
    premise INTEGER NOT NULL REFERENCES premise(id),
    gate TEXT NOT NULL REFERENCES gate_kind(name),
    metric TEXT REFERENCES cvss_metric(name),
    CHECK((gate='metric')=(metric IS NOT NULL)),
    PRIMARY KEY(candidate,premise)
) STRICT, WITHOUT ROWID;
CREATE INDEX candidate_premise_premise ON candidate_premise(premise,candidate);
-- The conditions each candidate relies on: those linked to it, and its member flaws'
-- conditions, which gate its scenario unless a link says otherwise.
CREATE VIEW candidate_condition AS
SELECT candidate,premise,gate,metric FROM candidate_premise
UNION ALL SELECT m.candidate,p.id,'scenario',NULL FROM candidate_member m
    JOIN premise p ON p.flaw=m.flaw
WHERE NOT EXISTS (SELECT 1 FROM candidate_premise l WHERE l.candidate=m.candidate
    AND l.premise=p.id);
-- A condition's resolution at the premise's revision; the latest one at the current revision
-- decides.
CREATE TABLE premise_resolution(
    id INTEGER PRIMARY KEY,
    premise INTEGER NOT NULL REFERENCES premise(id),
    revision INTEGER NOT NULL,
    resolution TEXT NOT NULL REFERENCES resolution_kind(name),
    flaw INTEGER REFERENCES flaw(id),
    -- What the scope admits that the condition holds by.
    deployment INTEGER REFERENCES deployment(id),
    -- The scope a condition was asked under; a later scope answers it.
    scope INTEGER REFERENCES scope(id),
    note TEXT NOT NULL,
    actor INTEGER NOT NULL REFERENCES actor(id),
    at INTEGER NOT NULL,
    CHECK(resolution='fails' OR flaw IS NULL),
    CHECK(resolution='holds' OR deployment IS NULL),
    CHECK((resolution='asked')=(scope IS NOT NULL))
) STRICT;
CREATE INDEX premise_resolution_premise ON premise_resolution(premise,id DESC);

-- One flaw as one path drives it. The latest decision per pair stands while its revisions
-- are the flaw's and the path's and no evidence contradicts it.
CREATE TABLE exposure(
    id INTEGER PRIMARY KEY,
    flaw INTEGER NOT NULL REFERENCES flaw(id),
    path INTEGER NOT NULL REFERENCES path(id),
    revision INTEGER NOT NULL,
    path_revision INTEGER NOT NULL,
    outcome TEXT NOT NULL REFERENCES exposure_outcome(name),
    conditions TEXT NOT NULL,
    note TEXT NOT NULL,
    actor INTEGER NOT NULL REFERENCES actor(id),
    at INTEGER NOT NULL
) STRICT;
CREATE INDEX exposure_pair ON exposure(flaw,path,id DESC);
CREATE INDEX exposure_path ON exposure(path,flaw);
CREATE TABLE exposure_step(
    exposure INTEGER NOT NULL REFERENCES exposure(id),
    position INTEGER NOT NULL CHECK(position>=0),
    location INTEGER NOT NULL REFERENCES location(id),
    basis TEXT NOT NULL REFERENCES route_basis(name),
    note TEXT NOT NULL,
    PRIMARY KEY(exposure,position)
) STRICT, WITHOUT ROWID;

-- A located observation about exactly one record.
CREATE TABLE evidence(
    id INTEGER PRIMARY KEY,
    flaw INTEGER REFERENCES flaw(id),
    candidate INTEGER REFERENCES candidate(id),
    exposure INTEGER REFERENCES exposure(id),
    premise INTEGER REFERENCES premise(id),
    guarantee INTEGER REFERENCES guarantee(id),
    refutation INTEGER REFERENCES flaw_refutation(id),
    location INTEGER NOT NULL REFERENCES location(id),
    role TEXT NOT NULL REFERENCES evidence_role(name),
    stance TEXT NOT NULL REFERENCES evidence_stance(name),
    note TEXT NOT NULL,
    actor INTEGER NOT NULL REFERENCES actor(id),
    at INTEGER NOT NULL,
    CHECK((flaw IS NOT NULL)+(candidate IS NOT NULL)+(exposure IS NOT NULL)
        +(premise IS NOT NULL)+(guarantee IS NOT NULL)+(refutation IS NOT NULL)=1)
) STRICT;
CREATE INDEX evidence_flaw ON evidence(flaw,id) WHERE flaw IS NOT NULL;
CREATE INDEX evidence_candidate ON evidence(candidate,id) WHERE candidate IS NOT NULL;
CREATE INDEX evidence_exposure ON evidence(exposure,id) WHERE exposure IS NOT NULL;
CREATE INDEX evidence_premise ON evidence(premise,id) WHERE premise IS NOT NULL;
CREATE INDEX evidence_guarantee ON evidence(guarantee,id) WHERE guarantee IS NOT NULL;
CREATE INDEX evidence_refutation ON evidence(refutation,id) WHERE refutation IS NOT NULL;
CREATE INDEX evidence_location ON evidence(location);

-- Conditional severity: the impact if the candidate's premises hold. The run's threshold
-- never changes, so whether the severity meets it is fixed when it is recorded.
CREATE TABLE assessment(
    id INTEGER PRIMARY KEY,
    candidate INTEGER NOT NULL REFERENCES candidate(id),
    revision INTEGER NOT NULL,
    vector TEXT NOT NULL,
    severity TEXT NOT NULL REFERENCES severity_kind(name),
    shortlisted INTEGER NOT NULL CHECK(shortlisted IN (0,1)),
    confidence TEXT NOT NULL REFERENCES confidence_kind(name),
    rationale TEXT NOT NULL,
    actor INTEGER NOT NULL REFERENCES actor(id),
    at INTEGER NOT NULL
) STRICT;
CREATE UNIQUE INDEX assessment_identity ON assessment(id,candidate);
CREATE INDEX assessment_candidate ON assessment(candidate,revision,id DESC);
CREATE INDEX assessment_vector ON assessment(candidate,vector);
-- The candidate's conditions when it was assessed. An assessment with the vector and the
-- conditions of an earlier one is unchanged, and the verdict on that one carries to it.
CREATE TABLE assessment_premise(
    assessment INTEGER NOT NULL REFERENCES assessment(id),
    premise INTEGER NOT NULL REFERENCES premise(id),
    PRIMARY KEY(assessment,premise)
) STRICT, WITHOUT ROWID;
-- The base metrics an assessment scores at their least severe value.
CREATE TABLE assessment_floor(
    assessment INTEGER NOT NULL REFERENCES assessment(id),
    metric TEXT NOT NULL REFERENCES cvss_metric(name),
    PRIMARY KEY(assessment,metric)
) STRICT, WITHOUT ROWID;
-- A verdict on one assessment. A policy exclusion records the scope it applied.
CREATE TABLE validation(
    id INTEGER PRIMARY KEY,
    candidate INTEGER NOT NULL,
    assessment INTEGER NOT NULL,
    verdict TEXT NOT NULL REFERENCES verdict_kind(name),
    reason TEXT REFERENCES rejection_reason(name),
    scope INTEGER REFERENCES scope(id),
    -- What the scope excludes that a policy exclusion cites.
    deployment INTEGER REFERENCES deployment(id),
    evidence TEXT NOT NULL,
    actor INTEGER NOT NULL REFERENCES actor(id),
    at INTEGER NOT NULL,
    CHECK((verdict='rejected')=(reason IS NOT NULL)),
    CHECK(reason IS 'policy_excluded' OR deployment IS NULL),
    FOREIGN KEY(assessment,candidate) REFERENCES assessment(id,candidate)
) STRICT;
CREATE UNIQUE INDEX validation_identity ON validation(id,candidate);
CREATE INDEX validation_assessment ON validation(assessment,id DESC);
CREATE INDEX validation_candidate ON validation(candidate,id);
-- A test opened against a verdict by an agent's job, before any work, after the candidate's
-- latest test as it was then (`since`); the test that settles it references it. The test is
-- open while the candidate has no test past `since`, and settles only while no contradiction
-- since its opening moved the candidate past `contradictions`.
CREATE TABLE poc_start(
    id INTEGER PRIMARY KEY,
    candidate INTEGER NOT NULL,
    validation INTEGER NOT NULL,
    since INTEGER REFERENCES poc(id),
    contradictions INTEGER NOT NULL,
    actor INTEGER NOT NULL REFERENCES actor(id),
    job INTEGER NOT NULL REFERENCES job(id),
    at INTEGER NOT NULL,
    FOREIGN KEY(validation,candidate) REFERENCES validation(id,candidate)
) STRICT;
CREATE INDEX poc_start_candidate ON poc_start(candidate,id);
-- A test of the candidate against the verdict it named; it stands until a contradiction
-- moves the candidate past `contradictions`.
CREATE TABLE poc(
    id INTEGER PRIMARY KEY,
    candidate INTEGER NOT NULL,
    contradictions INTEGER NOT NULL,
    validation INTEGER NOT NULL,
    start INTEGER UNIQUE REFERENCES poc_start(id),
    result TEXT NOT NULL REFERENCES poc_result_kind(name),
    evidence TEXT NOT NULL,
    actor INTEGER NOT NULL REFERENCES actor(id),
    at INTEGER NOT NULL,
    FOREIGN KEY(validation,candidate) REFERENCES validation(id,candidate)
) STRICT;
CREATE UNIQUE INDEX poc_identity ON poc(id,candidate);
CREATE INDEX poc_candidate ON poc(candidate,contradictions,id);
CREATE INDEX poc_validation ON poc(validation,id DESC);
CREATE TABLE poc_artifact(
    poc INTEGER NOT NULL REFERENCES poc(id),
    position INTEGER NOT NULL CHECK(position>=0),
    reference TEXT NOT NULL,
    role TEXT NOT NULL REFERENCES artifact_role(name),
    PRIMARY KEY(poc,position)
) STRICT, WITHOUT ROWID;

-- Each (flaw, path) pair's latest decision, and whether it still stands: made at its flaw's
-- current revision and its path's, and contradicted by no evidence since.
CREATE VIEW exposure_decision AS
SELECT e.id,e.flaw,e.path,e.revision,e.path_revision,e.outcome,e.conditions,e.note,e.actor,e.at,
    e.revision=(SELECT revision FROM flaw WHERE id=e.flaw)
    AND e.path_revision=(SELECT revision FROM path WHERE id=e.path)
    AND NOT EXISTS (SELECT 1 FROM evidence v WHERE v.exposure=e.id AND v.stance='contradicts')
    AS standing
FROM exposure e WHERE e.id=(SELECT max(id) FROM exposure WHERE flaw=e.flaw AND path=e.path);
-- Each premise's latest resolution, and whether it is current: made at the premise's current
-- revision. A premise without a current one is open.
CREATE VIEW premise_decision AS
SELECT x.*,x.revision=(SELECT revision FROM premise WHERE id=x.premise) AS current
FROM premise_resolution x WHERE x.id=(SELECT max(id) FROM premise_resolution WHERE
    premise=x.premise);

-- The root flaws of every candidate member: what `enables` leads back to through live
-- flaws, where nothing outside it enables it. A flaw nothing live enables (`top`) is a root.
-- Any other ancestor every one of whose own ancestors it enables in turn heads a cycle
-- nothing else feeds, rooted at the cycle's lowest flaw, so every flaw descending from a
-- cycle shares its root. Only an ancestor no top feeds can head one, so only those are traced
-- pairwise. Every flaw on a way to an ancestor is one, so once the ancestors are known each
-- trace follows only the live `enables` edges into them (`edge`, gathered from the ancestors'
-- side; what a top enables elsewhere is never visited). Candidates sharing a root are one
-- root group.
CREATE VIEW flaw_root AS
WITH RECURSIVE up(flaw,ancestor) AS (
    SELECT DISTINCT flaw,flaw FROM candidate_member
    UNION SELECT up.flaw,r.a FROM up JOIN relation r ON r.b=up.ancestor AND r.kind='enables'
        JOIN flaw f ON f.id=r.a WHERE f.merged_into IS NULL),
edge(a,b) AS MATERIALIZED (SELECT r.a,u.ancestor FROM (SELECT DISTINCT ancestor FROM up) u
    CROSS JOIN relation r ON r.b=u.ancestor AND r.kind='enables' JOIN flaw f ON f.id=r.a
    WHERE f.merged_into IS NULL),
top(flaw) AS (SELECT DISTINCT ancestor FROM up WHERE ancestor NOT IN (SELECT b FROM edge)),
fed(flaw) AS (SELECT flaw FROM top UNION SELECT e.b FROM fed JOIN edge e ON e.a=fed.flaw),
reach(flaw,ancestor) AS (
    SELECT DISTINCT ancestor,ancestor FROM up WHERE ancestor NOT IN fed
    UNION SELECT reach.flaw,e.a FROM reach JOIN edge e ON e.b=reach.ancestor)
SELECT up.flaw,up.ancestor AS root FROM up JOIN top ON top.flaw=up.ancestor
UNION SELECT up.flaw,(SELECT min(ancestor) FROM reach WHERE flaw=up.ancestor) FROM up
WHERE up.ancestor IN (SELECT flaw FROM reach) AND NOT EXISTS (SELECT 1 FROM reach x
    WHERE x.flaw=up.ancestor AND NOT EXISTS
    (SELECT 1 FROM reach y WHERE y.flaw=x.ancestor AND y.ancestor=up.ancestor));
CREATE VIEW candidate_root AS
SELECT DISTINCT m.candidate,r.root FROM candidate_member m JOIN flaw_root r ON r.flaw=m.flaw;

-- What each candidate stands at: its assessment at the current revision, the latest verdict on
-- that assessment or on an earlier one it leaves unchanged (the same vector under the same
-- conditions), so re-review is owed only when the assessment changed (a policy exclusion
-- stands only under the scope it applied, and a positive verdict only while the current
-- assessment lacks nothing a fresh one needs: `verdict_refusal` lists nothing for it), its
-- test, its confidence, and whether that state is published. Tests belong to the candidate's
-- contradictions, so a material change, a renewed verdict or a re-scoring keeps them: the test
-- is the latest failed one no later confirmation resolved, otherwise the latest that settled
-- something (a pending one settles nothing). Every positive verdict is reportable, at
-- `confirmed`, or `demonstrated` once its test is confirmed; below a verdict the confidence is
-- the assessment's. A positive verdict not demonstrated itself is noted when, in every root
-- group it belongs to, a demonstrated scenario the group's report states (an unmerged one)
-- stands at its severity or above. A finding stands for the assessment, verdict and test
-- it reported (report::as_published spells the same tuple).
CREATE VIEW candidate_assessment AS
SELECT c.id AS candidate, c.merged_into, c.revision, c.contradictions, (SELECT s.id FROM assessment s
    WHERE s.candidate=c.id AND s.revision=c.revision ORDER BY s.id DESC LIMIT 1) AS assessment
FROM candidate c;
-- What each candidate's current assessment lacks for a positive verdict, the most basic
-- first: a path, every member flaw contributing along one of the candidate's paths by a
-- standing decision and not refuted, every condition gating the scenario resolved as
-- holding, and each metric whose condition does not hold scored at its least severe value.
-- A verdict is recorded, and stands, only while it lists nothing for the candidate.
CREATE VIEW verdict_refusal AS
SELECT c.id AS candidate,0 AS rank,'no_path' AS refusal,NULL AS flaw,NULL AS premise,NULL AS metric
FROM candidate c WHERE NOT EXISTS (SELECT 1 FROM candidate_path cp WHERE cp.candidate=c.id)
UNION ALL SELECT m.candidate,1,CASE WHEN f.refuted THEN 'refuted' ELSE 'no_contribution' END,
    m.flaw,NULL,NULL
FROM candidate_member m JOIN flaw_state f ON f.id=m.flaw
WHERE f.refuted OR NOT EXISTS (SELECT 1 FROM candidate_path cp JOIN exposure_decision e ON
    e.path=cp.path AND e.flaw=m.flaw WHERE cp.candidate=m.candidate AND e.outcome='contributes'
    AND e.standing)
UNION ALL SELECT p.candidate,2,CASE WHEN p.gate='metric' THEN 'metric_above_floor'
    WHEN p.resolution IS 'fails' THEN 'failed_premise' ELSE 'open_premise' END,NULL,p.premise,
    p.metric
FROM (SELECT cc.*,(SELECT resolution FROM premise_decision WHERE premise=cc.premise AND
    current) AS resolution FROM candidate_condition cc) p
JOIN candidate_assessment ca ON ca.candidate=p.candidate
WHERE p.resolution IS NOT 'holds' AND NOT (p.gate='metric' AND EXISTS (SELECT 1 FROM
    assessment_floor af WHERE af.assessment=ca.assessment AND af.metric=p.metric));
CREATE VIEW candidate_review AS
SELECT ca.*, (SELECT CASE
    WHEN v.reason='policy_excluded' AND v.scope IS NOT (SELECT max(id) FROM scope) THEN NULL
    WHEN v.verdict='validated' AND EXISTS (SELECT 1 FROM verdict_refusal r
        WHERE r.candidate=ca.candidate) THEN NULL
    ELSE v.id END FROM validation v
    WHERE v.assessment IN (SELECT o.id FROM assessment o JOIN assessment s ON s.id=ca.assessment
        WHERE o.candidate=ca.candidate AND o.vector=s.vector AND NOT EXISTS (SELECT premise FROM
        assessment_premise WHERE assessment=o.id EXCEPT SELECT premise FROM assessment_premise
        WHERE assessment=s.id) AND NOT EXISTS (SELECT premise FROM assessment_premise WHERE
        assessment=s.id EXCEPT SELECT premise FROM assessment_premise WHERE assessment=o.id))
    ORDER BY v.id DESC LIMIT 1) AS validation
FROM candidate_assessment ca;
CREATE VIEW candidate_test AS
SELECT r.*, coalesce((SELECT p.id FROM poc p WHERE p.candidate=r.candidate
    AND p.contradictions=r.contradictions AND p.result='failed' AND NOT EXISTS
    (SELECT 1 FROM poc q WHERE q.candidate=r.candidate AND q.contradictions=r.contradictions
    AND q.result='confirmed' AND q.id>p.id) ORDER BY p.id DESC LIMIT 1),
    (SELECT p.id FROM poc p WHERE p.candidate=r.candidate AND p.contradictions=r.contradictions
    AND p.result!='pending' ORDER BY p.id DESC LIMIT 1)) AS poc
FROM candidate_review r;
CREATE VIEW candidate_state AS
SELECT t.*, v.verdict, v.reason, p.result, s.severity, s.confidence AS assessed,
    (SELECT rank FROM severity_kind WHERE name=s.severity) AS rank,
    CASE
        WHEN t.merged_into IS NOT NULL THEN 'merged'
        WHEN t.assessment IS NULL THEN 'identified'
        WHEN t.validation IS NULL THEN CASE s.shortlisted WHEN 1 THEN 'shortlisted' ELSE 'below_threshold' END
        WHEN v.verdict!='validated' THEN v.verdict
        WHEN p.result='confirmed' THEN 'validated'
        WHEN NOT EXISTS (SELECT 1 FROM candidate_root g WHERE g.candidate=t.candidate AND NOT EXISTS
            (SELECT 1 FROM candidate_root o JOIN candidate_test ot ON ot.candidate=o.candidate
            JOIN poc op ON op.id=ot.poc JOIN validation ov ON ov.id=ot.validation
            JOIN assessment oa ON oa.id=ot.assessment
            JOIN severity_kind ok ON ok.name=oa.severity JOIN severity_kind sk ON sk.name=s.severity
            WHERE o.root=g.root AND ot.merged_into IS NULL AND op.result='confirmed'
            AND ov.verdict='validated' AND ok.rank>=sk.rank)) THEN 'noted'
        ELSE 'validated'
    END AS state
FROM candidate_test t LEFT JOIN assessment s ON s.id=t.assessment
LEFT JOIN validation v ON v.id=t.validation LEFT JOIN poc p ON p.id=t.poc;
CREATE VIEW candidate_standing AS
SELECT st.*, CASE WHEN st.state IN ('validated','noted') AND EXISTS (SELECT 1
    FROM finding_candidate fc WHERE fc.candidate=st.candidate AND fc.assessment=st.assessment
    AND fc.validation=st.validation AND fc.poc IS st.poc) THEN 'reported' ELSE st.state END AS status,
    CASE WHEN st.state='merged' THEN NULL WHEN st.state NOT IN ('validated','noted') THEN st.assessed
        WHEN st.result='confirmed' THEN 'demonstrated' ELSE 'confirmed' END AS confidence
FROM candidate_state st;
-- The scenarios each root group reports: its candidates holding a positive verdict.
CREATE VIEW group_scenario AS
SELECT g.root,st.* FROM candidate_root g JOIN candidate_standing st ON st.candidate=g.candidate
WHERE st.state IN ('validated','noted');

CREATE TABLE environment(
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    instructions TEXT NOT NULL,
    target TEXT,
    actor INTEGER NOT NULL REFERENCES actor(id),
    at INTEGER NOT NULL
) STRICT;
CREATE TABLE environment_endpoint(
    environment INTEGER NOT NULL REFERENCES environment(id),
    position INTEGER NOT NULL CHECK(position>=0),
    address TEXT NOT NULL,
    PRIMARY KEY(environment,position)
) STRICT, WITHOUT ROWID;
CREATE TABLE environment_credential(
    environment INTEGER NOT NULL REFERENCES environment(id),
    position INTEGER NOT NULL CHECK(position>=0),
    kind TEXT NOT NULL REFERENCES credential_reference_kind(name),
    reference TEXT NOT NULL,
    PRIMARY KEY(environment,position)
) STRICT, WITHOUT ROWID;

-- One root flaw's report, leading with `headline`: a new one each time the root group's
-- reportable scenarios differ from its latest one's, keyed by that set, with the prose its
-- reporter wrote around the facts the store holds. Only a root's current finding is on disk,
-- rendered from these rows. A reproduction's outcome was observed, or is expected with its
-- limitations.
CREATE TABLE finding(
    id INTEGER PRIMARY KEY,
    root INTEGER NOT NULL REFERENCES flaw(id),
    headline INTEGER NOT NULL REFERENCES candidate(id),
    title TEXT NOT NULL,
    summary TEXT NOT NULL,
    analysis TEXT NOT NULL,
    impact TEXT NOT NULL,
    remediation TEXT NOT NULL,
    observed_result TEXT,
    expected_result TEXT,
    limitations TEXT,
    actor INTEGER NOT NULL REFERENCES actor(id),
    at INTEGER NOT NULL,
    CHECK((expected_result IS NULL)=(limitations IS NULL)),
    CHECK((observed_result IS NULL)!=(expected_result IS NULL))
) STRICT;
CREATE INDEX finding_root ON finding(root,id);
-- A finding's reproduction, in order: prerequisites, steps with what each should show, and
-- cleanup instructions.
CREATE TABLE finding_instruction(
    finding INTEGER NOT NULL REFERENCES finding(id),
    part TEXT NOT NULL REFERENCES instruction_part(name),
    position INTEGER NOT NULL CHECK(position>=0),
    text TEXT NOT NULL,
    PRIMARY KEY(finding,part,position)
) STRICT, WITHOUT ROWID;
CREATE TABLE finding_step(
    finding INTEGER NOT NULL REFERENCES finding(id),
    position INTEGER NOT NULL CHECK(position>=0),
    instruction TEXT NOT NULL,
    expected_result TEXT NOT NULL,
    PRIMARY KEY(finding,position)
) STRICT, WITHOUT ROWID;
-- The recorded artifacts a reproduction step uses, as the finding copies them.
CREATE TABLE finding_step_artifact(
    finding INTEGER NOT NULL,
    step INTEGER NOT NULL,
    poc INTEGER NOT NULL,
    position INTEGER NOT NULL,
    UNIQUE(finding,step,poc,position),
    FOREIGN KEY(finding,step) REFERENCES finding_step(finding,position),
    FOREIGN KEY(finding,poc,position) REFERENCES finding_artifact(finding,poc,position)
) STRICT;
-- The reporter's sections beyond the fixed outline, in order.
CREATE TABLE finding_section(
    finding INTEGER NOT NULL REFERENCES finding(id),
    position INTEGER NOT NULL CHECK(position>=0),
    heading TEXT NOT NULL,
    body TEXT NOT NULL,
    PRIMARY KEY(finding,position)
) STRICT, WITHOUT ROWID;
-- Each scenario a finding reports, as it stood when reported.
CREATE TABLE finding_candidate(
    finding INTEGER NOT NULL REFERENCES finding(id),
    candidate INTEGER NOT NULL REFERENCES candidate(id),
    assessment INTEGER NOT NULL,
    validation INTEGER NOT NULL,
    poc INTEGER,
    PRIMARY KEY(finding,candidate),
    FOREIGN KEY(assessment,candidate) REFERENCES assessment(id,candidate),
    FOREIGN KEY(validation,candidate) REFERENCES validation(id,candidate),
    FOREIGN KEY(poc,candidate) REFERENCES poc(id,candidate)
) STRICT, WITHOUT ROWID;
CREATE INDEX finding_candidate_candidate ON finding_candidate(candidate,finding);
-- The reporter's note on one scenario the finding reports: why it matters, what it needs.
CREATE TABLE finding_note(
    finding INTEGER NOT NULL,
    candidate INTEGER NOT NULL,
    note TEXT NOT NULL,
    PRIMARY KEY(finding,candidate),
    FOREIGN KEY(finding,candidate) REFERENCES finding_candidate(finding,candidate)
) STRICT, WITHOUT ROWID;
-- The reporter's account of one test the finding states: what the run showed.
CREATE TABLE finding_narrative(
    finding INTEGER NOT NULL REFERENCES finding(id),
    poc INTEGER NOT NULL REFERENCES poc(id),
    narrative TEXT NOT NULL,
    PRIMARY KEY(finding,poc)
) STRICT, WITHOUT ROWID;
-- Each artifact of a stated test as the finding copies it: its file below the test's folder
-- and the size and digest it had when the finding was recorded.
CREATE TABLE finding_artifact(
    finding INTEGER NOT NULL REFERENCES finding(id),
    poc INTEGER NOT NULL,
    position INTEGER NOT NULL,
    path TEXT NOT NULL,
    bytes INTEGER NOT NULL CHECK(bytes>=0),
    sha256 BLOB NOT NULL CHECK(length(sha256)=32),
    PRIMARY KEY(finding,poc,position),
    UNIQUE(finding,poc,path),
    FOREIGN KEY(poc,position) REFERENCES poc_artifact(poc,position)
) STRICT, WITHOUT ROWID;
CREATE TABLE event(
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL REFERENCES event_kind(name),
    actor INTEGER NOT NULL REFERENCES actor(id),
    at INTEGER NOT NULL
) STRICT;
-- The actor's own words for an event, where no record it references holds them.
CREATE TABLE event_note(
    event INTEGER PRIMARY KEY REFERENCES event(id),
    note TEXT NOT NULL
) STRICT;
-- The one record an event describes.
CREATE TABLE event_record(
    event INTEGER PRIMARY KEY REFERENCES event(id),
    scope INTEGER REFERENCES scope(id),
    principal INTEGER REFERENCES principal(id),
    source INTEGER REFERENCES source(id),
    component INTEGER REFERENCES component(id),
    path INTEGER REFERENCES path(id),
    unit INTEGER REFERENCES plan_unit(id),
    outcome INTEGER REFERENCES outcome(id),
    guarantee INTEGER REFERENCES guarantee(id),
    retry_site INTEGER REFERENCES retry_site(id),
    deferred INTEGER REFERENCES deferred(id),
    relation INTEGER REFERENCES relation(id),
    undermining INTEGER REFERENCES undermining(id),
    premise INTEGER REFERENCES premise(id),
    resolution INTEGER REFERENCES premise_resolution(id),
    refutation INTEGER REFERENCES flaw_refutation(id),
    evidence INTEGER REFERENCES evidence(id),
    exposure INTEGER REFERENCES exposure(id),
    assessment INTEGER REFERENCES assessment(id),
    validation INTEGER REFERENCES validation(id),
    poc INTEGER REFERENCES poc(id),
    poc_start INTEGER REFERENCES poc_start(id),
    environment INTEGER REFERENCES environment(id),
    finding INTEGER REFERENCES finding(id),
    CHECK((scope IS NOT NULL)+(principal IS NOT NULL)+(source IS NOT NULL)
        +(component IS NOT NULL)+(path IS NOT NULL)+(unit IS NOT NULL)
        +(outcome IS NOT NULL)+(guarantee IS NOT NULL)+(retry_site IS NOT NULL)
        +(deferred IS NOT NULL)+(relation IS NOT NULL)+(undermining IS NOT NULL)
        +(premise IS NOT NULL)
        +(resolution IS NOT NULL)+(refutation IS NOT NULL)+(evidence IS NOT NULL)
        +(exposure IS NOT NULL)+(assessment IS NOT NULL)+(validation IS NOT NULL)
        +(poc IS NOT NULL)+(poc_start IS NOT NULL)+(environment IS NOT NULL)
        +(finding IS NOT NULL)=1)
) STRICT;
CREATE INDEX event_record_unit ON event_record(unit) WHERE unit IS NOT NULL;
CREATE INDEX event_record_outcome ON event_record(outcome) WHERE outcome IS NOT NULL;
CREATE INDEX event_record_retry_site ON event_record(retry_site) WHERE retry_site IS NOT NULL;
CREATE INDEX event_record_undermining ON event_record(undermining) WHERE undermining IS NOT NULL;
CREATE INDEX event_record_evidence ON event_record(evidence) WHERE evidence IS NOT NULL;
CREATE INDEX event_record_assessment ON event_record(assessment) WHERE assessment IS NOT NULL;
CREATE INDEX event_record_poc ON event_record(poc) WHERE poc IS NOT NULL;
CREATE INDEX event_record_environment ON event_record(environment) WHERE environment IS NOT NULL;
-- The flaws and candidates an event concerns.
CREATE TABLE flaw_event(
    event INTEGER NOT NULL REFERENCES event(id),
    flaw INTEGER NOT NULL REFERENCES flaw(id),
    PRIMARY KEY(flaw,event)
) STRICT, WITHOUT ROWID;
CREATE TABLE candidate_event(
    event INTEGER NOT NULL REFERENCES event(id),
    candidate INTEGER NOT NULL REFERENCES candidate(id),
    PRIMARY KEY(candidate,event)
) STRICT, WITHOUT ROWID;
