-- Dictionaries are filled from the Rust enums when the database is created.
CREATE TABLE node_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
-- The kind of node each phase works, and its place in the pipeline (`Phase::stage`).
CREATE TABLE task_phase(
    name TEXT PRIMARY KEY,
    subject TEXT NOT NULL REFERENCES node_kind(name),
    stage INTEGER NOT NULL,
    UNIQUE(name,subject)
) STRICT, WITHOUT ROWID;
CREATE TABLE location_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE trust_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE premise_scope(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE premise_status(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE flaw_status(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE path_status(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE inventory_status(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE severity_kind(name TEXT PRIMARY KEY, rank INTEGER NOT NULL UNIQUE) STRICT, WITHOUT ROWID;
CREATE TABLE verdict_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE stance_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE test_result(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE artifact_role(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE event_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE event_effect(name TEXT PRIMARY KEY, rank INTEGER NOT NULL UNIQUE) STRICT, WITHOUT ROWID;
CREATE TABLE live_policy_kind(name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

-- The session is the run: one row, written as the session opens, with the live policy it keeps.
CREATE TABLE run(
    id INTEGER PRIMARY KEY CHECK(id=1),
    created INTEGER NOT NULL,
    threshold TEXT NOT NULL REFERENCES severity_kind(name),
    workers INTEGER NOT NULL CHECK(workers BETWEEN 1 AND 65535),
    live TEXT NOT NULL REFERENCES live_policy_kind(name)
) STRICT;

-- A site in the target; per-kind detail tables share its id.
CREATE TABLE location(
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL REFERENCES location_kind(name),
    UNIQUE(id,kind)
) STRICT;
-- A file region, in the function containing it when it lies in one.
CREATE TABLE code_location(
    location INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'code' CHECK(kind='code'),
    file TEXT NOT NULL,
    start_line INTEGER NOT NULL CHECK(start_line BETWEEN 1 AND 4294967295),
    end_line INTEGER NOT NULL CHECK(end_line BETWEEN start_line AND 4294967295),
    function TEXT,
    UNIQUE(file,start_line,end_line,function),
    FOREIGN KEY(location,kind) REFERENCES location(id,kind)
) STRICT;
CREATE UNIQUE INDEX code_location_unnamed ON code_location(file,start_line,end_line) WHERE function IS NULL;
-- An address range of a binary image at its link base, each address by its u64 bit pattern, so
-- the span is checked before it is stored; in the symbol containing it when one is known.
CREATE TABLE binary_location(
    location INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'binary' CHECK(kind='binary'),
    image TEXT NOT NULL,
    start_address INTEGER NOT NULL,
    end_address INTEGER NOT NULL,
    symbol TEXT,
    UNIQUE(image,start_address,end_address,symbol),
    FOREIGN KEY(location,kind) REFERENCES location(id,kind)
) STRICT;
CREATE UNIQUE INDEX binary_location_unnamed ON binary_location(image,start_address,end_address) WHERE symbol IS NULL;
-- An operation of an interface, at one of its fields when the site is narrower.
CREATE TABLE interface_location(
    location INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'interface' CHECK(kind='interface'),
    interface TEXT NOT NULL REFERENCES component_interface(interface),
    operation TEXT NOT NULL,
    field TEXT,
    UNIQUE(interface,operation,field),
    FOREIGN KEY(location,kind) REFERENCES location(id,kind)
) STRICT;
CREATE UNIQUE INDEX interface_location_whole ON interface_location(interface,operation) WHERE field IS NULL;

-- One audit log. `note` is the actor's own words. Ids are monotonic in commit order.
CREATE TABLE event(
    id INTEGER PRIMARY KEY,
    agent INTEGER NOT NULL REFERENCES agent(id),
    at INTEGER NOT NULL,
    kind TEXT NOT NULL REFERENCES event_kind(name),
    note TEXT,
    UNIQUE(id,kind)
) STRICT;
CREATE INDEX event_agent ON event(agent,id);

-- Every graph object; per-kind detail tables share its id. A merged node points at the live
-- one it was merged into, one hop.
CREATE TABLE node(
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL REFERENCES node_kind(name),
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    merged_into INTEGER,
    UNIQUE(id,kind),
    CHECK(merged_into!=id),
    FOREIGN KEY(merged_into,kind) REFERENCES node(id,kind)
) STRICT;
CREATE INDEX node_live ON node(kind,id) WHERE merged_into IS NULL;
CREATE INDEX node_merged ON node(merged_into) WHERE merged_into IS NOT NULL;

-- The nodes an event of `kind` names, each with the strongest way it bears on it, directly or by
-- propagation.
CREATE TABLE event_node(
    event INTEGER NOT NULL,
    node INTEGER NOT NULL REFERENCES node(id),
    kind TEXT NOT NULL,
    effect TEXT NOT NULL REFERENCES event_effect(name),
    PRIMARY KEY(node,event),
    FOREIGN KEY(event,kind) REFERENCES event(id,kind)
) STRICT, WITHOUT ROWID;
-- A node's effects but its test records', which bear on their candidate through `test`: a walk
-- down a node's effects never visits its tests. It holds the kind so that it alone answers one.
CREATE INDEX event_node_effect ON event_node(node,effect,event,kind) WHERE kind!='tested';
CREATE INDEX event_node_event ON event_node(event,node);

-- Sources, paths and components recorded in error are retracted, and stand in nothing derived.
CREATE TABLE component(
    node INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'component' CHECK(kind='component'),
    status TEXT NOT NULL DEFAULT 'recorded' REFERENCES inventory_status(name),
    FOREIGN KEY(node,kind) REFERENCES node(id,kind)
) STRICT;
CREATE TABLE component_file(
    file TEXT PRIMARY KEY,
    component INTEGER NOT NULL REFERENCES component(node)
) STRICT, WITHOUT ROWID;
CREATE INDEX component_file_component ON component_file(component,file);
-- The interfaces a component exposes, by name; an environment gives each one an address.
CREATE TABLE component_interface(
    interface TEXT PRIMARY KEY,
    component INTEGER NOT NULL REFERENCES component(node)
) STRICT, WITHOUT ROWID;
CREATE INDEX component_interface_component ON component_interface(component,interface);

-- One source per location. A merged source keeps its node but not this row, so its location is
-- free for a live source.
CREATE TABLE source(
    node INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'source' CHECK(kind='source'),
    location INTEGER NOT NULL UNIQUE REFERENCES location(id),
    trust TEXT NOT NULL REFERENCES trust_kind(name),
    status TEXT NOT NULL DEFAULT 'recorded' REFERENCES inventory_status(name),
    FOREIGN KEY(node,kind) REFERENCES node(id,kind)
) STRICT;

-- The swept unit.
CREATE TABLE path(
    node INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'path' CHECK(kind='path'),
    status TEXT NOT NULL DEFAULT 'unswept' REFERENCES path_status(name),
    FOREIGN KEY(node,kind) REFERENCES node(id,kind)
) STRICT;
-- Unswept paths first, as the Research page lists them.
CREATE INDEX path_unswept_first ON path(status!='unswept',node);
-- Positions run 0..n-1; the writes that move steps keep them so.
CREATE TABLE path_step(
    path INTEGER NOT NULL REFERENCES path(node),
    position INTEGER NOT NULL CHECK(position>=0),
    component INTEGER NOT NULL REFERENCES component(node),
    location INTEGER REFERENCES location(id),
    note TEXT NOT NULL,
    PRIMARY KEY(path,position)
) STRICT, WITHOUT ROWID;
CREATE INDEX path_step_component ON path_step(component,path);
CREATE TABLE path_source(
    path INTEGER NOT NULL REFERENCES path(node),
    source INTEGER NOT NULL REFERENCES source(node),
    PRIMARY KEY(path,source)
) STRICT, WITHOUT ROWID;
CREATE INDEX path_source_source ON path_source(source,path);
-- The logic a path's flow relies on.
CREATE TABLE path_premise(
    path INTEGER NOT NULL REFERENCES path(node),
    premise INTEGER NOT NULL REFERENCES premise(node),
    PRIMARY KEY(path,premise)
) STRICT, WITHOUT ROWID;
CREATE INDEX path_premise_premise ON path_premise(premise,path);

-- `decided` is the latest event setting its status, a restatement included.
CREATE TABLE premise(
    node INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'premise' CHECK(kind='premise'),
    scope TEXT NOT NULL REFERENCES premise_scope(name),
    location INTEGER REFERENCES location(id),
    status TEXT NOT NULL DEFAULT 'undecided' REFERENCES premise_status(name),
    decided INTEGER REFERENCES event(id),
    FOREIGN KEY(node,kind) REFERENCES node(id,kind)
) STRICT;

-- `judged` is the latest event setting its status, a restatement included.
CREATE TABLE flaw(
    node INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'flaw' CHECK(kind='flaw'),
    component INTEGER NOT NULL REFERENCES component(node),
    location INTEGER NOT NULL REFERENCES location(id),
    cwe INTEGER CHECK(cwe BETWEEN 1 AND 4294967295),
    status TEXT NOT NULL DEFAULT 'suspected' REFERENCES flaw_status(name),
    judged INTEGER REFERENCES event(id),
    FOREIGN KEY(node,kind) REFERENCES node(id,kind)
) STRICT;
CREATE INDEX flaw_component ON flaw(component,node);
CREATE TABLE flaw_premise(
    flaw INTEGER NOT NULL REFERENCES flaw(node),
    premise INTEGER NOT NULL REFERENCES premise(node),
    PRIMARY KEY(flaw,premise)
) STRICT, WITHOUT ROWID;
CREATE INDEX flaw_premise_premise ON flaw_premise(premise,flaw);

-- An ordered flaw chain through the actions of one scenario, with its assessment and verdict in
-- place. Each result names the event that recorded it; its rationale is that event's note.
CREATE TABLE candidate(
    node INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'candidate' CHECK(kind='candidate'),
    vector TEXT,
    severity TEXT REFERENCES severity_kind(name),
    assessed INTEGER REFERENCES event(id),
    verdict TEXT REFERENCES verdict_kind(name),
    reviewed INTEGER REFERENCES event(id),
    CHECK((vector IS NULL)=(severity IS NULL) AND (vector IS NULL)=(assessed IS NULL)),
    CHECK((verdict IS NULL)=(reviewed IS NULL)),
    FOREIGN KEY(node,kind) REFERENCES node(id,kind)
) STRICT;
CREATE INDEX candidate_validated ON candidate(node) WHERE verdict='validated';
-- The paths a candidate's scenario performs, in order, at least one: the first is where the
-- attacker starts. A path may repeat, as in a race with itself.
CREATE TABLE candidate_action(
    candidate INTEGER NOT NULL REFERENCES candidate(node),
    position INTEGER NOT NULL CHECK(position>=0),
    path INTEGER NOT NULL REFERENCES path(node),
    PRIMARY KEY(candidate,position)
) STRICT, WITHOUT ROWID;
CREATE INDEX candidate_action_path ON candidate_action(path,candidate);
-- The flaw at position 0 is the candidate's root.
CREATE TABLE candidate_flaw(
    candidate INTEGER NOT NULL REFERENCES candidate(node),
    position INTEGER NOT NULL CHECK(position>=0),
    flaw INTEGER NOT NULL REFERENCES flaw(node),
    PRIMARY KEY(candidate,position),
    UNIQUE(candidate,flaw)
) STRICT, WITHOUT ROWID;
CREATE INDEX candidate_flaw_flaw ON candidate_flaw(flaw,position,candidate);

-- One report per root flaw. The node's title and body are the report, last written by
-- `reported`.
CREATE TABLE finding(
    node INTEGER PRIMARY KEY,
    kind TEXT NOT NULL DEFAULT 'finding' CHECK(kind='finding'),
    root INTEGER NOT NULL UNIQUE REFERENCES flaw(node),
    reported INTEGER NOT NULL REFERENCES event(id),
    FOREIGN KEY(node,kind) REFERENCES node(id,kind)
) STRICT;

-- Evidence naming an environment is an observation made there.
CREATE TABLE evidence(
    id INTEGER PRIMARY KEY,
    node INTEGER NOT NULL REFERENCES node(id),
    location INTEGER REFERENCES location(id),
    environment INTEGER REFERENCES environment(id),
    stance TEXT NOT NULL REFERENCES stance_kind(name),
    note TEXT NOT NULL,
    event INTEGER NOT NULL REFERENCES event(id)
) STRICT;
CREATE INDEX evidence_node ON evidence(node,id);
CREATE INDEX evidence_disputes ON evidence(node,event) WHERE stance='contradicts';

-- Prepared for one candidate or for any.
CREATE TABLE environment(
    id INTEGER PRIMARY KEY,
    instructions TEXT NOT NULL,
    candidate INTEGER REFERENCES candidate(node),
    event INTEGER NOT NULL UNIQUE REFERENCES event(id)
) STRICT;
CREATE INDEX environment_candidate ON environment(candidate,event) WHERE candidate IS NOT NULL;
CREATE INDEX environment_unscoped ON environment(event) WHERE candidate IS NULL;
-- The interfaces an environment serves, each at its address there.
CREATE TABLE environment_interface(
    environment INTEGER NOT NULL REFERENCES environment(id),
    interface TEXT NOT NULL REFERENCES component_interface(interface),
    address TEXT NOT NULL,
    PRIMARY KEY(environment,interface)
) STRICT, WITHOUT ROWID;
CREATE INDEX environment_interface_interface ON environment_interface(interface,environment);

-- A test's account is its event's note. `verdict` is the candidate's verdict it tested: the
-- test stands only while that verdict is the candidate's latest.
CREATE TABLE test(
    id INTEGER PRIMARY KEY,
    candidate INTEGER NOT NULL REFERENCES candidate(node),
    environment INTEGER REFERENCES environment(id),
    result TEXT NOT NULL REFERENCES test_result(name),
    verdict INTEGER REFERENCES event(id),
    event INTEGER NOT NULL UNIQUE REFERENCES event(id)
) STRICT;
CREATE INDEX test_candidate ON test(candidate,id);
-- The tests that settle or decline, newest per candidate and per verdict tested.
CREATE INDEX test_settled ON test(candidate,id) WHERE result!='pending';
CREATE INDEX test_verdict ON test(candidate,verdict,id) WHERE result!='pending';
-- A file below the run's artifacts directory, its digest taken when the test was recorded.
CREATE TABLE test_artifact(
    test INTEGER NOT NULL REFERENCES test(id),
    path TEXT NOT NULL,
    role TEXT NOT NULL REFERENCES artifact_role(name),
    digest BLOB NOT NULL CHECK(length(digest)=32),
    PRIMARY KEY(test,path)
) STRICT, WITHOUT ROWID;

-- The scheduling log. A task has no end of its own: it is live while its job is.
CREATE TABLE task(
    id INTEGER PRIMARY KEY,
    job INTEGER NOT NULL REFERENCES job(id),
    phase TEXT NOT NULL REFERENCES task_phase(name),
    began INTEGER NOT NULL UNIQUE REFERENCES event(id),
    UNIQUE(id,phase)
) STRICT;
CREATE INDEX task_job ON task(job,id);
-- Each subject a task works, with the basis it began on: the latest event of its phase's
-- `bearing`. The keys pin the subject's kind to the phase's.
CREATE TABLE task_node(
    task INTEGER NOT NULL,
    phase TEXT NOT NULL,
    node INTEGER NOT NULL,
    kind TEXT NOT NULL,
    basis INTEGER NOT NULL REFERENCES event(id),
    PRIMARY KEY(task,node),
    FOREIGN KEY(task,phase) REFERENCES task(id,phase),
    FOREIGN KEY(node,kind) REFERENCES node(id,kind),
    FOREIGN KEY(phase,kind) REFERENCES task_phase(name,subject)
) STRICT, WITHOUT ROWID;
CREATE INDEX task_node_subject ON task_node(phase,node,basis);
CREATE INDEX task_node_node ON task_node(node,task);
-- The tasks an event's agent and the agents above it held when it was recorded: the work it is
-- part of.
CREATE TABLE event_task(
    event INTEGER NOT NULL REFERENCES event(id),
    task INTEGER NOT NULL REFERENCES task(id),
    PRIMARY KEY(event,task)
) STRICT, WITHOUT ROWID;

CREATE VIEW candidate_root AS
SELECT candidate,flaw AS root FROM candidate_flaw WHERE position=0;
