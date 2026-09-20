-- Skyhook session database (application_id 0x534B5948, user_version 7). Tables are STRICT;
-- subtype rows key (entry, kind) -> entry(seq, kind). db/mod.rs adds append-only triggers
-- to tables outside MUTABLE_TABLES. u64 values saturate to i64::MAX.

-- ───────────────────────── Ledger, session, agents ─────────────────────────

CREATE TABLE session (
  singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
  public_id BLOB NOT NULL CHECK (length(public_id) = 16)
) STRICT;

-- The capability names, which every capability column references.
CREATE TABLE capability (
  name TEXT PRIMARY KEY
) STRICT, WITHOUT ROWID;
INSERT INTO capability (name) VALUES
  ('read'),('write'),('exec'),('network'),('targets'),('ssh_agent'),('agents'),('interactive'),('mcp');

-- Pinned harness capability ceiling; every agent's set must be a subset (FK below).
CREATE TABLE session_capability (
  capability TEXT PRIMARY KEY REFERENCES capability(name)
) STRICT, WITHOUT ROWID;

-- 'root' is seeded at create; it has no revision rows.
CREATE TABLE target (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE
) STRICT;

CREATE TABLE agent (
  id INTEGER PRIMARY KEY,
  parent INTEGER REFERENCES agent(id),
  child_index INTEGER CHECK (child_index >= 0),
  owner_job INTEGER UNIQUE REFERENCES job(id),
  available_depth INTEGER NOT NULL CHECK (available_depth >= 0),
  UNIQUE (parent, child_index),
  CHECK ((parent IS NULL) = (child_index IS NULL)),
  CHECK (parent IS NOT NULL OR owner_job IS NULL)
) STRICT;
CREATE UNIQUE INDEX agent_single_root ON agent((parent IS NULL)) WHERE parent IS NULL;

CREATE TABLE entry (
  seq INTEGER PRIMARY KEY,                       -- insert NULL ... RETURNING seq
  public_id BLOB NOT NULL UNIQUE CHECK (length(public_id) = 16),
  agent INTEGER NOT NULL REFERENCES agent(id),
  created_millis INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN (
    'session_started','title_set','targets_upserted',
    'agent_started','agent_completed','agent_interrupted','agent_failed','model_selected',
    'mode_changed',
    'todos_replaced','message_committed','status',
    'model_context','model_requested','model_attempt_started','model_failed',
    'model_recovery_scheduled','model_attempt_interrupted','response_completed','usage',
    'compaction','compaction_skipped','compaction_failed',
    'job_created','job_state_changed','job_finished',
    'job_claimed','job_injected','job_message_delivered',
    'approval_granted','approval_revoked')),
  UNIQUE (seq, kind)
) STRICT;
CREATE UNIQUE INDEX entry_one_agent_start ON entry(agent) WHERE kind = 'agent_started';

-- Free-text payloads. agent_completed / agent_interrupted / session_started carry no
-- subtype row.
CREATE TABLE entry_text (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL CHECK (kind IN ('agent_failed','status','title_set')),
  text TEXT NOT NULL,
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

-- ───────────────────────── Targets ─────────────────────────

CREATE TABLE target_revision (
  id INTEGER PRIMARY KEY,
  target INTEGER NOT NULL REFERENCES target(id),
  entry INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('session_started','targets_upserted')),
  revision INTEGER NOT NULL CHECK (revision > 0),
  source TEXT NOT NULL CHECK (source IN ('builtin','config','session')),
  host TEXT NOT NULL,
  workspace BLOB NOT NULL,
  ssh_user TEXT,
  ssh_port INTEGER CHECK (ssh_port BETWEEN 1 AND 65535),
  ssh_auth TEXT NOT NULL CHECK (ssh_auth IN ('default','agent','key')),
  ssh_key_path BLOB,
  ssh_external_agent INTEGER NOT NULL CHECK (ssh_external_agent IN (0,1)),
  via INTEGER REFERENCES target(id),
  origin INTEGER REFERENCES target(id),
  UNIQUE (target, revision),
  CHECK ((ssh_auth = 'key') = (ssh_key_path IS NOT NULL)),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

CREATE TABLE target_ssh_option (
  revision INTEGER NOT NULL REFERENCES target_revision(id),
  key TEXT NOT NULL,
  value TEXT NOT NULL,
  PRIMARY KEY (revision, key)
) STRICT, WITHOUT ROWID;

-- ───────────────────────── Pinned contract ─────────────────────────

CREATE TABLE model_profile (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  reasoning TEXT,
  max_context INTEGER NOT NULL CHECK (max_context > 0),
  max_output INTEGER NOT NULL CHECK (max_output > 0),
  supports_images INTEGER NOT NULL CHECK (supports_images IN (0,1)),
  state_mode TEXT NOT NULL CHECK (state_mode IN ('none','dynamic','persist')),
  digest BLOB NOT NULL UNIQUE CHECK (length(digest) = 32),
  CHECK (max_output < max_context)
) STRICT;

-- An agent's settings belong to the entry that applied them; the latest one holds.
-- profile is NULL for a tool-only agent without a model, such as a remote worker.
CREATE TABLE agent_start (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'agent_started' CHECK (kind = 'agent_started'),
  profile INTEGER REFERENCES model_profile(id),
  location_target INTEGER NOT NULL REFERENCES target(id),
  location_workspace BLOB NOT NULL,
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

CREATE TABLE agent_capability (
  entry INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('agent_started','mode_changed')),
  capability TEXT NOT NULL REFERENCES session_capability(capability),
  PRIMARY KEY (entry, capability),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT, WITHOUT ROWID;

-- Mode definitions pinned by the entry that first used them; the session keeps them
-- whatever the configuration later says.
CREATE TABLE mode (
  id INTEGER PRIMARY KEY,
  entry INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('agent_started','mode_changed')),
  name TEXT NOT NULL UNIQUE,
  instructions TEXT,
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

-- What the mode lists, which may exceed the session ceiling; agent_capability rows
-- hold what an agent was actually granted.
CREATE TABLE mode_capability (
  mode INTEGER NOT NULL REFERENCES mode(id),
  capability TEXT NOT NULL REFERENCES capability(name),
  PRIMARY KEY (mode, capability)
) STRICT, WITHOUT ROWID;

-- The root agent's mode as of this entry.
CREATE TABLE agent_mode (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL CHECK (kind IN ('agent_started','mode_changed')),
  mode INTEGER NOT NULL REFERENCES mode(id),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

CREATE TABLE model_selection (                   -- ModelChanged only
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'model_selected' CHECK (kind = 'model_selected'),
  profile INTEGER NOT NULL REFERENCES model_profile(id),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

CREATE TABLE system_prompt (
  id INTEGER PRIMARY KEY,
  digest BLOB NOT NULL UNIQUE CHECK (length(digest) = 32)
) STRICT;

CREATE TABLE system_segment (
  prompt INTEGER NOT NULL REFERENCES system_prompt(id),
  position INTEGER NOT NULL CHECK (position >= 0),
  text TEXT NOT NULL,
  cache INTEGER NOT NULL CHECK (cache IN (0,1)),
  PRIMARY KEY (prompt, position)
) STRICT, WITHOUT ROWID;

CREATE TABLE tool_definition (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  description TEXT NOT NULL,
  input_schema TEXT NOT NULL CHECK (json_valid(input_schema)),
  digest BLOB NOT NULL UNIQUE CHECK (length(digest) = 32)
) STRICT;

-- provider/model/reasoning/max_output_tokens derive from profile; correlation from agent.
-- The agent's first purpose='agent' context is written in its agent_started transaction.
CREATE TABLE model_context (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'model_context' CHECK (kind = 'model_context'),
  purpose TEXT NOT NULL CHECK (purpose IN ('agent','compaction')),
  profile INTEGER NOT NULL REFERENCES model_profile(id),
  system_prompt INTEGER NOT NULL REFERENCES system_prompt(id),
  response_schema_name TEXT,
  response_schema TEXT CHECK (response_schema IS NULL OR json_valid(response_schema)),
  UNIQUE (entry, purpose),
  CHECK ((response_schema_name IS NULL) = (response_schema IS NULL)),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

CREATE TABLE model_context_tool (
  context INTEGER NOT NULL REFERENCES model_context(entry),
  position INTEGER NOT NULL CHECK (position >= 0),
  tool INTEGER NOT NULL REFERENCES tool_definition(id),
  PRIMARY KEY (context, position),
  UNIQUE (context, tool)
) STRICT, WITHOUT ROWID;

-- ───────────────────────── Blobs and messages ─────────────────────────

CREATE TABLE blob (
  sha256 BLOB PRIMARY KEY CHECK (length(sha256) = 32),
  bytes BLOB NOT NULL
) STRICT;

CREATE TABLE message (
  id INTEGER PRIMARY KEY,
  role TEXT NOT NULL CHECK (role IN ('user','assistant','tool')),
  UNIQUE (id, role)
) STRICT;

CREATE TABLE user_part (
  id INTEGER PRIMARY KEY,
  message INTEGER NOT NULL,
  role TEXT NOT NULL DEFAULT 'user' CHECK (role = 'user'),
  position INTEGER NOT NULL CHECK (position >= 0),
  kind TEXT NOT NULL CHECK (kind IN ('text','attachment','runtime','parent_input','compaction')),
  text TEXT,
  blob BLOB REFERENCES blob(sha256),               -- AttachmentRef
  image_format TEXT CHECK (image_format IN ('image/png','image/jpeg','image/gif','image/webp')),
  file TEXT,                                       -- NULL = no file name
  UNIQUE (message, position),
  CHECK ((kind = 'attachment') = (blob IS NOT NULL)),
  CHECK ((kind = 'attachment') = (text IS NULL)),
  CHECK (kind = 'attachment' OR (image_format IS NULL AND file IS NULL)),
  FOREIGN KEY (message, role) REFERENCES message(id, role)
) STRICT;

CREATE TABLE assistant_item (
  id INTEGER PRIMARY KEY,
  message INTEGER NOT NULL,
  role TEXT NOT NULL DEFAULT 'assistant' CHECK (role = 'assistant'),
  position INTEGER NOT NULL CHECK (position >= 0),
  provider_id TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('text','reasoning','tool_call')),
  UNIQUE (message, position),
  UNIQUE (id, kind),
  FOREIGN KEY (message, role) REFERENCES message(id, role)
) STRICT;

CREATE TABLE reasoning_replay (
  item INTEGER PRIMARY KEY REFERENCES assistant_item(id),
  version INTEGER NOT NULL,
  protocol TEXT NOT NULL,
  model TEXT NOT NULL,
  scope TEXT NOT NULL,
  payload TEXT NOT NULL CHECK (json_valid(payload)),
  conversation_bound INTEGER NOT NULL CHECK (conversation_bound IN (0,1))
) STRICT;

CREATE TABLE assistant_block (
  id INTEGER PRIMARY KEY,
  item INTEGER NOT NULL,
  item_kind TEXT NOT NULL CHECK (item_kind IN ('text','reasoning','tool_call')),
  position INTEGER NOT NULL CHECK (position >= 0),
  provider_id TEXT NOT NULL,
  text TEXT,
  UNIQUE (item, position),
  UNIQUE (id, item_kind),
  CHECK ((item_kind = 'tool_call') = (text IS NULL)),
  FOREIGN KEY (item, item_kind) REFERENCES assistant_item(id, kind)
) STRICT;

CREATE TABLE tool_call (
  block INTEGER PRIMARY KEY,
  item_kind TEXT NOT NULL DEFAULT 'tool_call' CHECK (item_kind = 'tool_call'),
  call_id TEXT NOT NULL,
  name TEXT NOT NULL,
  arguments TEXT NOT NULL CHECK (json_valid(arguments) AND json_type(arguments) = 'object'),
  FOREIGN KEY (block, item_kind) REFERENCES assistant_block(id, item_kind)
) STRICT;
CREATE INDEX tool_call_id ON tool_call(call_id);

-- One result per tool message; name derives from the call. History merges a turn's
-- result messages into one Message::Tool ordered by the call's item position.
CREATE TABLE tool_result (
  message INTEGER PRIMARY KEY,
  role TEXT NOT NULL DEFAULT 'tool' CHECK (role = 'tool'),
  call INTEGER NOT NULL UNIQUE REFERENCES tool_call(block),
  result TEXT NOT NULL CHECK (json_valid(result)),
  is_error INTEGER NOT NULL CHECK (is_error IN (0,1)),
  FOREIGN KEY (message, role) REFERENCES message(id, role)
) STRICT;

CREATE TABLE tool_result_image (
  result INTEGER NOT NULL REFERENCES tool_result(message),
  position INTEGER NOT NULL CHECK (position >= 0),
  blob BLOB NOT NULL REFERENCES blob(sha256),
  format TEXT NOT NULL CHECK (format IN ('image/png','image/jpeg','image/gif','image/webp')),
  file TEXT,
  PRIMARY KEY (result, position)
) STRICT, WITHOUT ROWID;

CREATE TABLE message_commit (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'message_committed' CHECK (kind = 'message_committed'),
  message INTEGER NOT NULL UNIQUE REFERENCES message(id),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

CREATE TABLE todo_item (
  entry INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('todos_replaced','compaction')),
  position INTEGER NOT NULL CHECK (position >= 0),
  text TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('pending','in_progress','completed')),
  PRIMARY KEY (entry, position),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT, WITHOUT ROWID;

-- ───────────────────────── Model requests, attempts, compaction ─────────────────────────

CREATE TABLE model_request (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'model_requested' CHECK (kind = 'model_requested'),
  context INTEGER NOT NULL,
  purpose TEXT NOT NULL CHECK (purpose IN ('agent','compaction')),
  -- History is the checkpoint, its retained sources, then the agent's commits after the
  -- checkpoint's frontier up to history_through, less model_request_omitted.
  checkpoint INTEGER REFERENCES compaction(entry),
  history_through INTEGER REFERENCES message_commit(entry),
  history_lifetime TEXT NOT NULL CHECK (history_lifetime IN ('continuing','ending','detached')),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind),
  FOREIGN KEY (context, purpose) REFERENCES model_context(entry, purpose)
) STRICT;

-- A source in that range the request left out, such as one with nothing left to send.
CREATE TABLE model_request_omitted (
  request INTEGER NOT NULL REFERENCES model_request(entry),
  source INTEGER NOT NULL REFERENCES message_commit(entry),
  PRIMARY KEY (request, source)
) STRICT, WITHOUT ROWID;

CREATE TABLE model_request_tail (
  request INTEGER NOT NULL REFERENCES model_request(entry),
  position INTEGER NOT NULL CHECK (position >= 0),
  message INTEGER NOT NULL REFERENCES message(id),
  PRIMARY KEY (request, position)
) STRICT, WITHOUT ROWID;

CREATE TABLE model_attempt (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'model_attempt_started' CHECK (kind = 'model_attempt_started'),
  request INTEGER NOT NULL REFERENCES model_request(entry),
  attempt INTEGER NOT NULL CHECK (attempt > 0),
  UNIQUE (request, attempt),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

-- How an attempt ended: at most one outcome each. A kind's detail row shares the entry.
CREATE TABLE attempt_outcome (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL CHECK (kind IN
    ('model_failed','model_attempt_interrupted','response_completed','compaction')),
  attempt INTEGER NOT NULL UNIQUE REFERENCES model_attempt(entry),
  UNIQUE (entry, kind),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

CREATE TABLE model_failure (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'model_failed' CHECK (kind = 'model_failed'),
  failure TEXT NOT NULL CHECK (failure IN ('error','refusal')),
  error TEXT NOT NULL,
  FOREIGN KEY (entry, kind) REFERENCES attempt_outcome(entry, kind)
) STRICT;

CREATE TABLE model_recovery (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'model_recovery_scheduled' CHECK (kind = 'model_recovery_scheduled'),
  failure INTEGER NOT NULL UNIQUE REFERENCES model_failure(entry),
  delay_millis INTEGER NOT NULL CHECK (delay_millis >= 0),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

CREATE TABLE model_response (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'response_completed' CHECK (kind = 'response_completed'),
  message INTEGER UNIQUE REFERENCES message_commit(message),
  stop_reason TEXT NOT NULL CHECK (stop_reason IN
    ('end_turn','tool_use','max_tokens','stop_sequence','content_filter','aborted','other')),
  stop_other TEXT,
  CHECK ((stop_reason = 'other') = (stop_other IS NOT NULL)),
  FOREIGN KEY (entry, kind) REFERENCES attempt_outcome(entry, kind)
) STRICT;

CREATE TABLE usage (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'usage' CHECK (kind = 'usage'),
  request INTEGER REFERENCES model_request(entry),
  input_tokens INTEGER NOT NULL CHECK (input_tokens >= 0),
  cached_input_tokens INTEGER NOT NULL CHECK (cached_input_tokens >= 0),
  output_tokens INTEGER NOT NULL CHECK (output_tokens >= 0),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

-- previous = latest earlier compaction of the same agent; request and attempt come from
-- the entry's attempt_outcome (both derived).
CREATE TABLE compaction (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'compaction' CHECK (kind = 'compaction'),
  schema_version INTEGER NOT NULL CHECK (schema_version > 0),
  frontier INTEGER NOT NULL REFERENCES entry(seq),
  message INTEGER NOT NULL REFERENCES message(id),
  before_tokens INTEGER NOT NULL CHECK (before_tokens >= 0),
  after_tokens INTEGER NOT NULL CHECK (after_tokens >= 0),
  CHECK (frontier < entry),
  FOREIGN KEY (entry, kind) REFERENCES attempt_outcome(entry, kind)
) STRICT;

CREATE TABLE compaction_retained (
  compaction INTEGER NOT NULL REFERENCES compaction(entry),
  source INTEGER NOT NULL REFERENCES message_commit(entry),
  PRIMARY KEY (compaction, source)
) STRICT, WITHOUT ROWID;

CREATE TABLE compaction_outcome (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL CHECK (kind IN ('compaction_skipped','compaction_failed')),
  request INTEGER REFERENCES model_request(entry),
  -- The summary attempt this round used. It may also hold a model failure, so it is a
  -- reference rather than an attempt_outcome.
  attempt INTEGER REFERENCES model_attempt(entry),
  reason TEXT NOT NULL,
  CHECK (kind = 'compaction_failed' OR attempt IS NOT NULL),
  CHECK (attempt IS NULL OR request IS NOT NULL),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

-- ───────────────────────── Jobs and outputs ─────────────────────────

CREATE TABLE job (
  id INTEGER PRIMARY KEY CHECK (id > 0),         -- JobId
  created INTEGER NOT NULL UNIQUE,
  kind TEXT NOT NULL DEFAULT 'job_created' CHECK (kind = 'job_created'),
  parent INTEGER REFERENCES job(id),
  origin_call INTEGER UNIQUE REFERENCES tool_call(block),
  tool TEXT NOT NULL,
  name TEXT,
  role TEXT NOT NULL CHECK (role IN ('tool','agent','script','question')),
  arguments TEXT NOT NULL CHECK (json_valid(arguments)),
  output_schema TEXT CHECK (output_schema IS NULL OR json_valid(output_schema)),
  accepts_input INTEGER NOT NULL CHECK (accepts_input IN (0,1)),
  background INTEGER NOT NULL CHECK (background IN (0,1)),
  authorization_scope INTEGER,
  location_target INTEGER NOT NULL REFERENCES target(id),
  location_workspace BLOB NOT NULL,
  FOREIGN KEY (created, kind) REFERENCES entry(seq, kind)
) STRICT;

CREATE TABLE job_transition (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'job_state_changed' CHECK (kind = 'job_state_changed'),
  job INTEGER NOT NULL REFERENCES job(id),
  state TEXT NOT NULL CHECK (state IN ('queued','awaiting_approval','running','waiting_input')),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;
CREATE INDEX job_transition_job ON job_transition(job, entry);

-- One row per run of a job: generation 0 at creation, the next when a 'running'
-- transition follows a finish.
CREATE TABLE job_run (
  job INTEGER NOT NULL REFERENCES job(id),
  generation INTEGER NOT NULL CHECK (generation >= 0),
  started INTEGER NOT NULL UNIQUE REFERENCES entry(seq),
  PRIMARY KEY (job, generation)
) STRICT, WITHOUT ROWID;

-- Output belongs to one run: a reset never presents an earlier run's output.
CREATE TABLE job_output (
  id INTEGER PRIMARY KEY,
  job INTEGER NOT NULL,
  generation INTEGER NOT NULL,
  -- Compact terminal document: referenced captures are emptied placeholders.
  document TEXT NOT NULL CHECK (json_valid(document)),
  UNIQUE (job, generation),
  FOREIGN KEY (job, generation) REFERENCES job_run(job, generation)
) STRICT;

-- Streamed or offloaded bytes at one JSON Pointer. A row exists from reservation, so
-- partial output stays readable; an abandoned builtin capture deletes its row.
CREATE TABLE job_capture (
  id INTEGER PRIMARY KEY,
  job INTEGER NOT NULL,
  generation INTEGER NOT NULL,
  pointer TEXT NOT NULL CHECK (pointer = '' OR pointer LIKE '/%'),
  capture_kind TEXT NOT NULL CHECK (capture_kind IN ('text','json','unknown')),
  final_bytes INTEGER CHECK (final_bytes >= 0),   -- set once when the writer finishes
  final_lines INTEGER CHECK (final_lines >= 0),
  -- A cached page rendering of a saved value, not producer output.
  rendered INTEGER NOT NULL DEFAULT 0 CHECK (rendered IN (0,1)),
  UNIQUE (job, generation, pointer),
  UNIQUE (id, job, generation),
  CHECK ((final_bytes IS NULL) = (final_lines IS NULL)),
  FOREIGN KEY (job, generation) REFERENCES job_run(job, generation)
) STRICT;

-- Identity is immutable; an unknown kind may resolve once, and a writer finishes once.
CREATE TRIGGER job_capture_update_rules BEFORE UPDATE ON job_capture
WHEN NEW.id IS NOT OLD.id OR NEW.job IS NOT OLD.job OR NEW.generation IS NOT OLD.generation
  OR NEW.pointer IS NOT OLD.pointer OR NEW.rendered IS NOT OLD.rendered
  OR (NEW.capture_kind IS NOT OLD.capture_kind AND OLD.capture_kind <> 'unknown')
  OR (OLD.final_bytes IS NOT NULL
      AND (NEW.final_bytes IS NOT OLD.final_bytes OR NEW.final_lines IS NOT OLD.final_lines))
BEGIN
  SELECT RAISE(ABORT, 'job_capture: identity is immutable; kind resolves and final sets once');
END;

-- first_line = newlines before byte_offset; chunks are contiguous from offset 0.
CREATE TABLE job_capture_chunk (
  capture INTEGER NOT NULL REFERENCES job_capture(id) ON DELETE CASCADE,
  byte_offset INTEGER NOT NULL CHECK (byte_offset >= 0),
  first_line INTEGER NOT NULL CHECK (first_line >= 0),
  data BLOB NOT NULL CHECK (length(data) BETWEEN 1 AND 1048576),
  PRIMARY KEY (capture, byte_offset)
) STRICT;
CREATE INDEX job_capture_line ON job_capture_chunk(capture, first_line);

-- Captures the terminal document references; only these are complete results.
CREATE TABLE job_output_field (
  output INTEGER NOT NULL REFERENCES job_output(id) ON DELETE CASCADE,
  capture INTEGER NOT NULL,
  job INTEGER NOT NULL,
  generation INTEGER NOT NULL,
  PRIMARY KEY (output, capture),
  FOREIGN KEY (capture, job, generation)
    REFERENCES job_capture(id, job, generation) ON DELETE CASCADE,
  FOREIGN KEY (job, generation) REFERENCES job_output(job, generation)
) STRICT, WITHOUT ROWID;
CREATE INDEX job_output_field_capture ON job_output_field(capture);

-- Script result provenance: a pointer shows a child job's output, or (child NULL) is an
-- annotated truncatable field of the script's own result.
CREATE TABLE job_presentation (
  id INTEGER PRIMARY KEY,
  job INTEGER NOT NULL,
  generation INTEGER NOT NULL,
  pointer TEXT NOT NULL,
  child INTEGER REFERENCES job(id),
  UNIQUE (job, generation, pointer, child),
  FOREIGN KEY (job, generation) REFERENCES job_run(job, generation)
) STRICT;

CREATE TABLE job_finish (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'job_finished' CHECK (kind = 'job_finished'),
  job INTEGER NOT NULL REFERENCES job(id),
  state TEXT NOT NULL CHECK (state IN ('completed','failed','cancelled','interrupted')),
  error TEXT,
  denial_code TEXT CHECK (denial_code IN ('permission_denied')),
  denial_executed INTEGER CHECK (denial_executed IN (0,1)),
  CHECK ((denial_code IS NULL) = (denial_executed IS NULL)),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;
CREATE INDEX job_finish_job ON job_finish(job, entry);

CREATE TABLE job_finish_image (
  finish INTEGER NOT NULL REFERENCES job_finish(entry),
  position INTEGER NOT NULL CHECK (position >= 0),
  blob BLOB NOT NULL REFERENCES blob(sha256),
  format TEXT NOT NULL CHECK (format IN ('image/png','image/jpeg','image/gif','image/webp')),
  file TEXT,
  PRIMARY KEY (finish, position)
) STRICT, WITHOUT ROWID;

CREATE TABLE job_delivery (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL CHECK (kind IN ('job_claimed','job_injected','job_message_delivered')),
  job INTEGER NOT NULL REFERENCES job(id),
  -- A delivered child reply and the runtime message that carried it.
  source INTEGER REFERENCES message_commit(entry),
  notification INTEGER REFERENCES message_commit(entry),
  CHECK ((kind = 'job_message_delivered') = (source IS NOT NULL)),
  CHECK ((kind = 'job_message_delivered') = (notification IS NOT NULL)),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

-- ───────────────────────── Approvals ─────────────────────────

CREATE TABLE approval_grant (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'approval_granted' CHECK (kind = 'approval_granted'),
  capability TEXT NOT NULL REFERENCES capability(name),
  resource TEXT NOT NULL CHECK (json_valid(resource)),   -- ResourceId wire form
  coverage TEXT NOT NULL CHECK (coverage IN ('exact','descendants')),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

-- Revocations are written in the same transaction as the targets_upserted that causes them.
CREATE TABLE approval_revocation (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'approval_revoked' CHECK (kind = 'approval_revoked'),
  grant_entry INTEGER NOT NULL UNIQUE REFERENCES approval_grant(entry),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

-- ───────────────────────── Views ─────────────────────────

CREATE VIEW job_generation AS
SELECT job, max(generation) AS generation FROM job_run GROUP BY job;

CREATE VIEW open_attempt AS
SELECT a.entry AS attempt FROM model_attempt a
WHERE NOT EXISTS (SELECT 1 FROM attempt_outcome o WHERE o.attempt = a.entry)
  AND NOT EXISTS (SELECT 1 FROM compaction_outcome c WHERE c.attempt = a.entry);

CREATE VIEW unanswered_call AS
SELECT c.block AS call, i.message
FROM tool_call c
JOIN assistant_block b ON b.id = c.block
JOIN assistant_item i ON i.id = b.item
JOIN message_commit mc ON mc.message = i.message
WHERE NOT EXISTS (SELECT 1 FROM tool_result r WHERE r.call = c.block);

CREATE VIEW session_summary AS
SELECT
  (SELECT t.text FROM entry_text t WHERE t.kind = 'title_set' ORDER BY t.entry DESC LIMIT 1) AS title,
  (SELECT p.text FROM message_commit mc
     JOIN entry e ON e.seq = mc.entry
     JOIN agent a ON a.id = e.agent AND a.parent IS NULL
     JOIN user_part p ON p.message = mc.message AND p.kind = 'text'
   ORDER BY mc.entry, p.position LIMIT 1) AS preview,
  (SELECT max(created_millis) FROM entry) AS last_millis,
  (SELECT count(*) FROM entry) AS entries,
  (SELECT p.name FROM model_profile p WHERE p.id = coalesce(
     (SELECT ms.profile FROM model_selection ms JOIN entry x ON x.seq = ms.entry
       JOIN agent a ON a.id = x.agent AND a.parent IS NULL ORDER BY ms.entry DESC LIMIT 1),
     (SELECT s.profile FROM agent_start s JOIN entry x ON x.seq = s.entry
       JOIN agent a ON a.id = x.agent AND a.parent IS NULL))) AS model,
  (SELECT m.name FROM agent_mode am JOIN mode m ON m.id = am.mode JOIN entry x ON x.seq = am.entry
     JOIN agent a ON a.id = x.agent AND a.parent IS NULL ORDER BY am.entry DESC LIMIT 1) AS mode;
