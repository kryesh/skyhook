-- Skyhook session database (application_id 0x534B5948, user_version 18). Tables are STRICT;
-- subtype rows key (entry, kind) -> entry(seq, kind). db/mod.rs adds append-only triggers
-- to tables outside MUTABLE_TABLES. u64 values saturate to i64::MAX.
--
-- Skyhook-typed data is normalized. The only JSON text columns hold shapes a model, a
-- tool's own schema or a provider defines: tool_call.arguments, tool_result.result,
-- job.arguments, job.output_schema, tool_definition.input_schema,
-- model_context.response_schema, reasoning_replay.payload, job_output.result and
-- user_part_job_event.view.

-- ───────────────────────── Ledger, session, agents ─────────────────────────

CREATE TABLE session (
  singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
  public_id BLOB NOT NULL CHECK (length(public_id) = 16)
) STRICT;

-- Dictionaries hold the spellings of the enums they name, as JSON spells them;
-- `Dictionaries` in db/mod.rs seeds them at create and checks them at open. A subset
-- dictionary names the values a column may take. A flag marks the values that fill a
-- payload column several share; the payload's table references it through a
-- generated column.
CREATE TABLE capability (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

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

-- Subtype tables narrow kind to the kinds they hold. Activity kinds are the session's
-- activity; reopening and closing it are not.
CREATE TABLE entry_kind (
  name TEXT PRIMARY KEY,
  activity INTEGER NOT NULL CHECK (activity IN (0,1))
) STRICT, WITHOUT ROWID;

CREATE TABLE entry (
  seq INTEGER PRIMARY KEY,                       -- insert NULL ... RETURNING seq
  public_id BLOB NOT NULL UNIQUE CHECK (length(public_id) = 16),
  agent INTEGER NOT NULL REFERENCES agent(id),
  created_millis INTEGER NOT NULL,
  kind TEXT NOT NULL REFERENCES entry_kind(name),
  UNIQUE (seq, kind)
) STRICT;
CREATE UNIQUE INDEX entry_one_agent_start ON entry(agent) WHERE kind = 'agent_started';
-- An agent's latest entry of a kind, for state read without decoding the journal.
CREATE INDEX entry_agent_kind ON entry(agent, kind);

-- Status text. agent_completed / agent_interrupted / session_started / session_reopened /
-- title_cleared carry no subtype row.
CREATE TABLE entry_text (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'status' CHECK (kind = 'status'),
  text TEXT NOT NULL,
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

CREATE TABLE title_source (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

-- A title_cleared entry drops the user's earlier titles; session_title applies the rule.
CREATE TABLE title (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'title_set' CHECK (kind = 'title_set'),
  source TEXT NOT NULL REFERENCES title_source(name),
  text TEXT NOT NULL,
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

-- ───────────────────────── Targets ─────────────────────────

CREATE TABLE target_source (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE ssh_auth (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE targets_entry_kind (name TEXT PRIMARY KEY REFERENCES entry_kind(name)) STRICT, WITHOUT ROWID;

-- via/origin NULL means the route starts at root.
CREATE TABLE target_revision (
  id INTEGER PRIMARY KEY,
  target INTEGER NOT NULL REFERENCES target(id),
  entry INTEGER NOT NULL,
  kind TEXT NOT NULL REFERENCES targets_entry_kind(name),
  revision INTEGER NOT NULL CHECK (revision > 0),
  source TEXT NOT NULL REFERENCES target_source(name),
  host TEXT NOT NULL,
  workspace BLOB NOT NULL,
  ssh_user TEXT,
  ssh_port INTEGER CHECK (ssh_port BETWEEN 1 AND 65535),
  ssh_auth TEXT NOT NULL REFERENCES ssh_auth(name),
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

CREATE TABLE state_mode (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE model_profile (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  reasoning TEXT,
  max_context INTEGER NOT NULL CHECK (max_context > 0),
  max_output INTEGER NOT NULL CHECK (max_output > 0),
  supports_images INTEGER NOT NULL CHECK (supports_images IN (0,1)),
  state_mode TEXT NOT NULL REFERENCES state_mode(name),
  hint TEXT,
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

-- Entries that apply a mode and the capabilities it grants.
CREATE TABLE mode_entry_kind (name TEXT PRIMARY KEY REFERENCES entry_kind(name)) STRICT, WITHOUT ROWID;

CREATE TABLE agent_capability (
  entry INTEGER NOT NULL,
  kind TEXT NOT NULL REFERENCES mode_entry_kind(name),
  capability TEXT NOT NULL REFERENCES session_capability(capability),
  PRIMARY KEY (entry, capability),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT, WITHOUT ROWID;

-- Mode definitions pinned by the entry that first used them; the session keeps them
-- whatever the configuration later says.
CREATE TABLE mode (
  id INTEGER PRIMARY KEY,
  entry INTEGER NOT NULL,
  kind TEXT NOT NULL REFERENCES mode_entry_kind(name),
  name TEXT NOT NULL UNIQUE,
  instructions TEXT,
  hint TEXT,
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

-- What the mode lists, which may exceed the session ceiling; agent_capability rows
-- hold what an agent was actually granted.
CREATE TABLE mode_capability (
  mode INTEGER NOT NULL REFERENCES mode(id),
  capability TEXT NOT NULL REFERENCES capability(name),
  PRIMARY KEY (mode, capability)
) STRICT, WITHOUT ROWID;

-- The agent's mode as of this entry.
CREATE TABLE agent_mode (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL REFERENCES mode_entry_kind(name),
  mode INTEGER NOT NULL REFERENCES mode(id),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

CREATE TABLE model_selection (                   -- ModelChanged only
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'model_changed' CHECK (kind = 'model_changed'),
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

CREATE TABLE model_purpose (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

-- provider/model/reasoning/max_output_tokens derive from profile.
-- The agent's first purpose='agent' context is written in its agent_started transaction.
CREATE TABLE model_context (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'model_context' CHECK (kind = 'model_context'),
  purpose TEXT NOT NULL REFERENCES model_purpose(name),
  profile INTEGER NOT NULL REFERENCES model_profile(id),
  system_prompt INTEGER NOT NULL REFERENCES system_prompt(id),
  response_schema_name TEXT,
  response_schema TEXT CHECK (response_schema IS NULL OR json_valid(response_schema)),
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

CREATE TABLE message_role (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE image_format (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE message (
  id INTEGER PRIMARY KEY,
  role TEXT NOT NULL REFERENCES message_role(name),
  UNIQUE (id, role)
) STRICT;

-- state and job_events parts hold their content in the user_part_* tables.
CREATE TABLE user_part_kind (
  name TEXT PRIMARY KEY,
  text INTEGER NOT NULL CHECK (text IN (0,1)),
  UNIQUE (name, text)
) STRICT, WITHOUT ROWID;

CREATE TABLE user_part (
  id INTEGER PRIMARY KEY,
  message INTEGER NOT NULL,
  role TEXT NOT NULL DEFAULT 'user' CHECK (role = 'user'),
  position INTEGER NOT NULL CHECK (position >= 0),
  kind TEXT NOT NULL,
  text TEXT,
  blob BLOB REFERENCES blob(sha256),               -- AttachmentRef
  image_format TEXT REFERENCES image_format(name),
  file TEXT,                                       -- NULL = no file name
  has_text INTEGER GENERATED ALWAYS AS (text IS NOT NULL) VIRTUAL,
  UNIQUE (message, position),
  CHECK ((kind = 'attachment') = (blob IS NOT NULL)),
  FOREIGN KEY (kind, has_text) REFERENCES user_part_kind(name, text),
  CHECK (kind = 'attachment' OR (image_format IS NULL AND file IS NULL)),
  FOREIGN KEY (message, role) REFERENCES message(id, role)
) STRICT;

-- The agent's state as a request sent it: its date and location, live jobs and todos.
CREATE TABLE user_part_state (
  part INTEGER PRIMARY KEY REFERENCES user_part(id),
  date TEXT NOT NULL,
  location_target INTEGER NOT NULL REFERENCES target(id),
  location_workspace BLOB NOT NULL
) STRICT;

-- Jobs in pre-order; parent_position nests an agent job's active children.
-- An agent job has turns/tool_calls, its progress; any other job names its tool.
CREATE TABLE user_part_state_job (
  part INTEGER NOT NULL REFERENCES user_part_state(part),
  position INTEGER NOT NULL CHECK (position >= 0),
  parent_position INTEGER,
  job INTEGER NOT NULL REFERENCES job(id),
  tool TEXT,
  name TEXT,
  state TEXT NOT NULL REFERENCES job_state(name),
  target INTEGER REFERENCES target(id),
  workspace BLOB NOT NULL,
  age_seconds INTEGER NOT NULL CHECK (age_seconds >= 0),
  turns INTEGER CHECK (turns >= 0),
  tool_calls INTEGER CHECK (tool_calls >= 0),
  PRIMARY KEY (part, position),
  CHECK ((turns IS NULL) = (tool_calls IS NULL)),
  CHECK ((tool IS NULL) = (turns IS NOT NULL)),
  FOREIGN KEY (part, parent_position) REFERENCES user_part_state_job(part, position)
) STRICT, WITHOUT ROWID;

CREATE TABLE user_part_state_todo (
  part INTEGER NOT NULL REFERENCES user_part_state(part),
  position INTEGER NOT NULL CHECK (position >= 0),
  text TEXT NOT NULL,
  status TEXT NOT NULL REFERENCES todo_status(name),
  PRIMARY KEY (part, position)
) STRICT, WITHOUT ROWID;

CREATE TABLE job_event_kind (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

-- A job-events notification: a child's reply (message) or a job's presented view (job).
-- Like a tool result, the view is the JSON document the model received.
CREATE TABLE user_part_job_event (
  part INTEGER NOT NULL REFERENCES user_part(id),
  position INTEGER NOT NULL CHECK (position >= 0),
  kind TEXT NOT NULL REFERENCES job_event_kind(name),
  job INTEGER NOT NULL REFERENCES job(id),
  name TEXT,
  source INTEGER REFERENCES message_commit(entry),
  text TEXT,
  view TEXT CHECK (view IS NULL OR json_valid(view)),
  PRIMARY KEY (part, position),
  CHECK ((kind = 'message') = (source IS NOT NULL AND text IS NOT NULL)),
  CHECK ((kind = 'job') = (view IS NOT NULL)),
  CHECK (kind = 'message' OR name IS NULL)
) STRICT, WITHOUT ROWID;

-- Assistant items are text, reasoning (with optional replay), or one tool call each.
CREATE TABLE item_kind (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE assistant_item (
  id INTEGER PRIMARY KEY,
  message INTEGER NOT NULL,
  role TEXT NOT NULL DEFAULT 'assistant' CHECK (role = 'assistant'),
  position INTEGER NOT NULL CHECK (position >= 0),
  provider_id TEXT NOT NULL CHECK (trim(provider_id) <> ''),
  kind TEXT NOT NULL REFERENCES item_kind(name),
  UNIQUE (message, position),
  UNIQUE (id, kind),
  FOREIGN KEY (message, role) REFERENCES message(id, role)
) STRICT;

-- How tightly a replay is bound to the history that produced it.
CREATE TABLE replay_binding (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

-- The native shape of a replay payload.
CREATE TABLE replay_format (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE reasoning_replay (
  item INTEGER PRIMARY KEY,
  item_kind TEXT NOT NULL DEFAULT 'reasoning' CHECK (item_kind = 'reasoning'),
  format TEXT NOT NULL REFERENCES replay_format(name),
  model TEXT NOT NULL,
  scope TEXT NOT NULL CHECK (trim(scope) <> ''),
  payload TEXT NOT NULL CHECK (json_valid(payload)),
  binding TEXT NOT NULL REFERENCES replay_binding(name),
  FOREIGN KEY (item, item_kind) REFERENCES assistant_item(id, kind)
) STRICT;

-- Readable blocks of text and reasoning items.
CREATE TABLE block_item_kind (name TEXT PRIMARY KEY REFERENCES item_kind(name)) STRICT, WITHOUT ROWID;
CREATE TABLE assistant_block (
  id INTEGER PRIMARY KEY,
  item INTEGER NOT NULL,
  item_kind TEXT NOT NULL REFERENCES block_item_kind(name),
  position INTEGER NOT NULL CHECK (position >= 0),
  provider_id TEXT NOT NULL CHECK (trim(provider_id) <> ''),
  text TEXT NOT NULL,
  UNIQUE (item, position),
  FOREIGN KEY (item, item_kind) REFERENCES assistant_item(id, kind)
) STRICT;

CREATE TABLE tool_call (
  item INTEGER PRIMARY KEY,
  item_kind TEXT NOT NULL DEFAULT 'tool_call' CHECK (item_kind = 'tool_call'),
  call_id TEXT NOT NULL,
  name TEXT NOT NULL,
  arguments TEXT NOT NULL CHECK (json_valid(arguments) AND json_type(arguments) = 'object'),
  FOREIGN KEY (item, item_kind) REFERENCES assistant_item(id, kind)
) STRICT;
CREATE INDEX tool_call_id ON tool_call(call_id);

-- One result per tool message; name derives from the call. History merges a turn's
-- result messages into one Message::Tool ordered by the call's item position.
CREATE TABLE tool_result (
  message INTEGER PRIMARY KEY,
  role TEXT NOT NULL DEFAULT 'tool' CHECK (role = 'tool'),
  call INTEGER NOT NULL UNIQUE REFERENCES tool_call(item),
  result TEXT NOT NULL CHECK (json_valid(result)),
  is_error INTEGER NOT NULL CHECK (is_error IN (0,1)),
  FOREIGN KEY (message, role) REFERENCES message(id, role)
) STRICT;

CREATE TABLE tool_result_image (
  result INTEGER NOT NULL REFERENCES tool_result(message),
  position INTEGER NOT NULL CHECK (position >= 0),
  blob BLOB NOT NULL REFERENCES blob(sha256),
  format TEXT NOT NULL REFERENCES image_format(name),
  file TEXT,
  PRIMARY KEY (result, position)
) STRICT, WITHOUT ROWID;

CREATE TABLE message_commit (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'message_committed' CHECK (kind = 'message_committed'),
  message INTEGER NOT NULL UNIQUE REFERENCES message(id),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

CREATE TABLE todo_status (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE todos_entry_kind (name TEXT PRIMARY KEY REFERENCES entry_kind(name)) STRICT, WITHOUT ROWID;
CREATE TABLE todo_item (
  entry INTEGER NOT NULL,
  kind TEXT NOT NULL REFERENCES todos_entry_kind(name),
  position INTEGER NOT NULL CHECK (position >= 0),
  text TEXT NOT NULL,
  status TEXT NOT NULL REFERENCES todo_status(name),
  PRIMARY KEY (entry, position),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT, WITHOUT ROWID;

-- ───────────────────────── Model requests, attempts, compaction ─────────────────────────

-- Whether later requests in a context extend a request's history.
CREATE TABLE history_lifetime (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE model_request (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'model_requested' CHECK (kind = 'model_requested'),
  context INTEGER NOT NULL REFERENCES model_context(entry),
  -- History is the checkpoint, its retained sources, then the agent's commits after the
  -- checkpoint's frontier up to history_through.
  checkpoint INTEGER REFERENCES compaction(entry),
  history_through INTEGER REFERENCES message_commit(entry),
  history_lifetime TEXT NOT NULL REFERENCES history_lifetime(name),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

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
  UNIQUE (entry, request),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

-- How an attempt ended: at most one outcome each. A kind's detail row shares the entry.
CREATE TABLE attempt_outcome_kind (name TEXT PRIMARY KEY REFERENCES entry_kind(name)) STRICT, WITHOUT ROWID;
CREATE TABLE attempt_outcome (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL REFERENCES attempt_outcome_kind(name),
  attempt INTEGER NOT NULL UNIQUE REFERENCES model_attempt(entry),
  UNIQUE (entry, kind),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

-- How an agent turn or a model attempt failed; detailed kinds keep their detail.
CREATE TABLE failure_kind (
  name TEXT PRIMARY KEY,
  detailed INTEGER NOT NULL CHECK (detailed IN (0,1)),
  UNIQUE (name, detailed)
) STRICT, WITHOUT ROWID;
CREATE TABLE provider_error_kind (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE failure_entry_kind (name TEXT PRIMARY KEY REFERENCES entry_kind(name)) STRICT, WITHOUT ROWID;
CREATE TABLE failure (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL REFERENCES failure_entry_kind(name),
  failure TEXT NOT NULL,
  detailed INTEGER NOT NULL,
  detail TEXT,
  -- A provider failure's class; its detail is the provider's message.
  provider TEXT REFERENCES provider_error_kind(name),
  -- A model failure is its attempt's outcome; NULL leaves an agent failure unlinked.
  outcome TEXT GENERATED ALWAYS AS (CASE kind WHEN 'model_failed' THEN kind END) VIRTUAL,
  CHECK (detailed = (detail IS NOT NULL)),
  CHECK ((failure = 'provider') = (provider IS NOT NULL)),
  FOREIGN KEY (failure, detailed) REFERENCES failure_kind(name, detailed),
  FOREIGN KEY (entry, outcome) REFERENCES attempt_outcome(entry, kind),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

CREATE TABLE model_recovery (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'model_recovery_scheduled' CHECK (kind = 'model_recovery_scheduled'),
  failure INTEGER NOT NULL UNIQUE,
  failure_kind TEXT NOT NULL DEFAULT 'model_failed' CHECK (failure_kind = 'model_failed'),
  delay_millis INTEGER NOT NULL CHECK (delay_millis >= 0),
  FOREIGN KEY (failure, failure_kind) REFERENCES attempt_outcome(entry, kind),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

-- How a completed response ended; refusals and aborts are model failures instead.
CREATE TABLE response_outcome (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE cut_reason (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE model_response (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'response_completed' CHECK (kind = 'response_completed'),
  message INTEGER NOT NULL UNIQUE REFERENCES message_commit(message),
  outcome TEXT NOT NULL REFERENCES response_outcome(name),
  cut_reason TEXT REFERENCES cut_reason(name),
  CHECK ((outcome = 'cut') = (cut_reason IS NOT NULL)),
  FOREIGN KEY (entry, kind) REFERENCES attempt_outcome(entry, kind)
) STRICT;

CREATE TABLE usage (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'usage' CHECK (kind = 'usage'),
  request INTEGER NOT NULL REFERENCES model_request(entry),
  input_tokens INTEGER NOT NULL CHECK (input_tokens >= 0),
  cached_input_tokens INTEGER NOT NULL CHECK (cached_input_tokens >= 0),
  cache_write_input_tokens INTEGER NOT NULL CHECK (cache_write_input_tokens >= 0),
  output_tokens INTEGER NOT NULL CHECK (output_tokens >= 0),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind),
  CHECK (cache_write_input_tokens <= input_tokens)
) STRICT;

-- The summary attempt is the entry's attempt_outcome.
CREATE TABLE compaction (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'compaction' CHECK (kind = 'compaction'),
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

CREATE TABLE compaction_fault (
  name TEXT PRIMARY KEY,
  detailed INTEGER NOT NULL CHECK (detailed IN (0,1)),
  UNIQUE (name, detailed)
) STRICT, WITHOUT ROWID;
CREATE TABLE checkpoint_error (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE compaction_failure (              -- CompactionFailed only
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'compaction_failed' CHECK (kind = 'compaction_failed'),
  request INTEGER REFERENCES model_request(entry),
  -- The summary attempt this round used. It may also hold a model failure, so it is a
  -- reference rather than an attempt_outcome.
  attempt INTEGER,
  fault TEXT NOT NULL,
  detailed INTEGER NOT NULL,
  detail TEXT,
  -- A checkpoint fault's detail names the checkpoint error, a summary fault its failure.
  checkpoint TEXT GENERATED ALWAYS AS (CASE fault WHEN 'checkpoint' THEN detail END) VIRTUAL,
  summary TEXT GENERATED ALWAYS AS (CASE fault WHEN 'summary' THEN detail END) VIRTUAL,
  CHECK (attempt IS NULL OR request IS NOT NULL),
  CHECK (detailed = (detail IS NOT NULL)),
  FOREIGN KEY (attempt, request) REFERENCES model_attempt(entry, request),
  FOREIGN KEY (fault, detailed) REFERENCES compaction_fault(name, detailed),
  FOREIGN KEY (checkpoint) REFERENCES checkpoint_error(name),
  FOREIGN KEY (summary) REFERENCES failure_kind(name),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

-- ───────────────────────── Jobs and outputs ─────────────────────────

CREATE TABLE job_role (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

-- Transitions name live states and finishes terminal ones.
CREATE TABLE job_state (
  name TEXT PRIMARY KEY,
  terminal INTEGER NOT NULL CHECK (terminal IN (0,1)),
  UNIQUE (name, terminal)
) STRICT, WITHOUT ROWID;

CREATE TABLE job (
  id INTEGER PRIMARY KEY CHECK (id > 0),         -- JobId
  created INTEGER NOT NULL UNIQUE,
  kind TEXT NOT NULL DEFAULT 'job_created' CHECK (kind = 'job_created'),
  parent INTEGER REFERENCES job(id),
  origin_call INTEGER UNIQUE REFERENCES tool_call(item),
  tool TEXT NOT NULL,
  name TEXT,
  role TEXT NOT NULL REFERENCES job_role(name),
  arguments TEXT NOT NULL CHECK (json_valid(arguments)),
  output_schema TEXT CHECK (output_schema IS NULL OR json_valid(output_schema)),
  accepts_input INTEGER NOT NULL CHECK (accepts_input IN (0,1)),
  background INTEGER NOT NULL CHECK (background IN (0,1)),
  location_target INTEGER NOT NULL REFERENCES target(id),
  location_workspace BLOB NOT NULL,
  FOREIGN KEY (created, kind) REFERENCES entry(seq, kind)
) STRICT;

CREATE TABLE job_transition (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'job_state_changed' CHECK (kind = 'job_state_changed'),
  job INTEGER NOT NULL REFERENCES job(id),
  state TEXT NOT NULL,
  terminal INTEGER NOT NULL DEFAULT 0 CHECK (terminal = 0),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind),
  FOREIGN KEY (state, terminal) REFERENCES job_state(name, terminal)
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
  -- Every capture the producer finished was admitted into the result.
  captures_complete INTEGER NOT NULL CHECK (captures_complete IN (0,1)),
  -- The tool's result, NULL when the run produced none ('null' is a literal null);
  -- referenced captures are emptied placeholders.
  result TEXT CHECK (result IS NULL OR json_valid(result)),
  -- Bytes of the result's automatic presentation, measured once it is saved.
  presented_bytes INTEGER CHECK (presented_bytes >= 0),
  UNIQUE (job, generation),
  FOREIGN KEY (job, generation) REFERENCES job_run(job, generation)
) STRICT;

CREATE TABLE capture_kind (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE capture_detection (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

-- Streamed or offloaded bytes at one JSON Pointer. A row exists from reservation, so
-- partial output stays readable; an abandoned builtin capture deletes its row.
CREATE TABLE job_capture (
  id INTEGER PRIMARY KEY,
  job INTEGER NOT NULL,
  generation INTEGER NOT NULL,
  pointer TEXT NOT NULL CHECK (pointer = '' OR pointer LIKE '/%'),
  capture_kind TEXT NOT NULL REFERENCES capture_kind(name),
  final_bytes INTEGER CHECK (final_bytes >= 0),   -- set once when the writer finishes
  final_lines INTEGER CHECK (final_lines >= 0),
  -- A cached page rendering of a saved value, not producer output.
  rendered INTEGER NOT NULL DEFAULT 0 CHECK (rendered IN (0,1)),
  -- What a finished text capture its schema declares as JSON holds.
  detection TEXT REFERENCES capture_detection(name),
  CHECK (detection IS NULL OR (final_bytes IS NOT NULL AND capture_kind = 'text' AND rendered = 0)),
  -- A rendering of a saved value may share its pointer with the capture it reads.
  UNIQUE (job, generation, pointer, rendered),
  UNIQUE (id, job, generation),
  CHECK ((final_bytes IS NULL) = (final_lines IS NULL)),
  FOREIGN KEY (job, generation) REFERENCES job_run(job, generation)
) STRICT;

-- Identity is immutable; an unknown kind may resolve once, a writer finishes once, and
-- a finished capture is classified once.
CREATE TRIGGER job_capture_update_rules BEFORE UPDATE ON job_capture
WHEN NEW.id IS NOT OLD.id OR NEW.job IS NOT OLD.job OR NEW.generation IS NOT OLD.generation
  OR NEW.pointer IS NOT OLD.pointer OR NEW.rendered IS NOT OLD.rendered
  OR (NEW.capture_kind IS NOT OLD.capture_kind AND OLD.capture_kind <> 'unknown')
  OR (OLD.final_bytes IS NOT NULL
      AND (NEW.final_bytes IS NOT OLD.final_bytes OR NEW.final_lines IS NOT OLD.final_lines))
  OR (OLD.detection IS NOT NULL AND NEW.detection IS NOT OLD.detection)
BEGIN
  SELECT RAISE(ABORT, 'job_capture: identity is immutable; kind, final and detection set once');
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

-- Fields of a saved result that presentation never shortens: those its schema
-- declares and those a script's returned tool results carry.
CREATE TABLE job_complete_field (
  id INTEGER PRIMARY KEY,
  job INTEGER NOT NULL,
  generation INTEGER NOT NULL,
  pointer TEXT NOT NULL,
  UNIQUE (job, generation, pointer),
  FOREIGN KEY (job, generation) REFERENCES job_run(job, generation)
) STRICT;

CREATE TABLE job_finish (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'job_finished' CHECK (kind = 'job_finished'),
  job INTEGER NOT NULL REFERENCES job(id),
  state TEXT NOT NULL,
  terminal INTEGER NOT NULL DEFAULT 1 CHECK (terminal = 1),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind),
  FOREIGN KEY (state, terminal) REFERENCES job_state(name, terminal)
) STRICT;
CREATE INDEX job_finish_job ON job_finish(job, entry);

-- Diagnostics retain typed terminal and output-persistence facts, never rendered errors.
CREATE TABLE diagnostic_slot (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE diagnostic_operation (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE diagnostic_subject (
  name TEXT PRIMARY KEY,
  path INTEGER NOT NULL CHECK (path IN (0,1)),
  text INTEGER NOT NULL CHECK (text IN (0,1)),
  UNIQUE (name, path, text)
) STRICT, WITHOUT ROWID;

CREATE TABLE diagnostic_site (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE diagnostic_effects (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE diagnostic_cause (
  name TEXT PRIMARY KEY,
  text INTEGER NOT NULL CHECK (text IN (0,1)),
  job INTEGER NOT NULL CHECK (job IN (0,1)),
  UNIQUE (name, text, job)
) STRICT, WITHOUT ROWID;

CREATE TABLE diagnostic_io_kind (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE diagnostic_path_role (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE job_finish_diagnostic (
  finish INTEGER NOT NULL REFERENCES job_finish(entry),
  slot TEXT NOT NULL REFERENCES diagnostic_slot(name),
  operation TEXT NOT NULL REFERENCES diagnostic_operation(name),
  subject TEXT NOT NULL,
  subject_path BLOB,
  subject_text TEXT,
  -- The subject may be an unsuccessful lookup of a job that does not exist.
  subject_job INTEGER CHECK (subject_job > 0),
  site TEXT NOT NULL REFERENCES diagnostic_site(name),
  location_target INTEGER REFERENCES target(id),
  location_workspace BLOB,
  effects TEXT NOT NULL REFERENCES diagnostic_effects(name),
  cause TEXT NOT NULL,
  io_kind TEXT REFERENCES diagnostic_io_kind(name),
  io_code INTEGER CHECK (io_code BETWEEN -2147483648 AND 2147483647),
  io_detail TEXT,
  cause_text TEXT,
  cause_job INTEGER CHECK (cause_job > 0),
  has_subject_path INTEGER GENERATED ALWAYS AS (subject_path IS NOT NULL) VIRTUAL,
  has_subject_text INTEGER GENERATED ALWAYS AS (subject_text IS NOT NULL) VIRTUAL,
  has_cause_text INTEGER GENERATED ALWAYS AS (cause_text IS NOT NULL) VIRTUAL,
  has_cause_job INTEGER GENERATED ALWAYS AS (cause_job IS NOT NULL) VIRTUAL,
  PRIMARY KEY (finish, slot),
  FOREIGN KEY (subject, has_subject_path, has_subject_text)
    REFERENCES diagnostic_subject(name, path, text),
  FOREIGN KEY (cause, has_cause_text, has_cause_job) REFERENCES diagnostic_cause(name, text, job),
  CHECK ((subject = 'job') = (subject_job IS NOT NULL)),
  CHECK ((site = 'execution') = (location_target IS NOT NULL)),
  CHECK ((site = 'execution') = (location_workspace IS NOT NULL)),
  CHECK ((cause = 'io') = (io_kind IS NOT NULL)),
  CHECK (cause = 'io' OR io_code IS NULL),
  CHECK (cause = 'io' OR io_detail IS NULL)
) STRICT, WITHOUT ROWID;

-- Path facts retain caller order and native bytes separately from the attempted subject.
CREATE TABLE diagnostic_path (
  finish INTEGER NOT NULL,
  slot TEXT NOT NULL,
  position INTEGER NOT NULL CHECK (position >= 0),
  role TEXT NOT NULL REFERENCES diagnostic_path_role(name),
  path BLOB NOT NULL,
  PRIMARY KEY (finish, slot, position),
  FOREIGN KEY (finish, slot) REFERENCES job_finish_diagnostic(finish, slot)
) STRICT, WITHOUT ROWID;

CREATE TABLE job_finish_image (
  finish INTEGER NOT NULL REFERENCES job_finish(entry),
  position INTEGER NOT NULL CHECK (position >= 0),
  blob BLOB NOT NULL REFERENCES blob(sha256),
  format TEXT NOT NULL REFERENCES image_format(name),
  file TEXT,
  PRIMARY KEY (finish, position)
) STRICT, WITHOUT ROWID;

CREATE TABLE delivery_kind (name TEXT PRIMARY KEY REFERENCES entry_kind(name)) STRICT, WITHOUT ROWID;
CREATE TABLE job_delivery (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL REFERENCES delivery_kind(name),
  job INTEGER NOT NULL REFERENCES job(id),
  -- A delivered child reply and the runtime message that carried it.
  source INTEGER REFERENCES message_commit(entry),
  notification INTEGER REFERENCES message_commit(entry),
  CHECK ((kind = 'job_message_delivered') = (source IS NOT NULL)),
  CHECK ((kind = 'job_message_delivered') = (notification IS NOT NULL)),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

-- ───────────────────────── Approvals ─────────────────────────

CREATE TABLE approval_coverage (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE resource_kind (
  name TEXT PRIMARY KEY,
  target INTEGER NOT NULL CHECK (target IN (0,1)),
  UNIQUE (name, target)
) STRICT, WITHOUT ROWID;

-- One ResourceId per grant: target is the execution target of a workspace, path or
-- network resource and the destination of a route; path components and route hops
-- are the child tables.
CREATE TABLE approval_grant (
  entry INTEGER PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'approval_granted' CHECK (kind = 'approval_granted'),
  capability TEXT NOT NULL REFERENCES capability(name),
  resource_kind TEXT NOT NULL,
  target INTEGER REFERENCES target(id),
  path TEXT,
  origin TEXT,
  session_name TEXT,
  mcp_server TEXT,
  mcp_tool TEXT,
  coverage TEXT NOT NULL REFERENCES approval_coverage(name),
  has_target INTEGER GENERATED ALWAYS AS (target IS NOT NULL) VIRTUAL,
  FOREIGN KEY (resource_kind, has_target) REFERENCES resource_kind(name, target),
  CHECK ((resource_kind = 'workspace') = (path IS NOT NULL)),
  CHECK ((resource_kind = 'network') = (origin IS NOT NULL)),
  CHECK ((resource_kind = 'session') = (session_name IS NOT NULL)),
  CHECK ((resource_kind = 'mcp') = (mcp_server IS NOT NULL)),
  CHECK ((resource_kind = 'mcp') = (mcp_tool IS NOT NULL)),
  FOREIGN KEY (entry, kind) REFERENCES entry(seq, kind)
) STRICT;

CREATE TABLE approval_grant_path_component (
  grant_entry INTEGER NOT NULL REFERENCES approval_grant(entry),
  position INTEGER NOT NULL CHECK (position >= 0),
  component TEXT NOT NULL,
  PRIMARY KEY (grant_entry, position)
) STRICT, WITHOUT ROWID;

-- Hops name journaled target revisions; root is never a hop.
CREATE TABLE approval_grant_route_hop (
  grant_entry INTEGER NOT NULL REFERENCES approval_grant(entry),
  position INTEGER NOT NULL CHECK (position >= 0),
  target INTEGER NOT NULL REFERENCES target(id),
  revision INTEGER NOT NULL,
  PRIMARY KEY (grant_entry, position),
  FOREIGN KEY (target, revision) REFERENCES target_revision(target, revision)
) STRICT, WITHOUT ROWID;

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

-- The user's newest title, unless a title_cleared follows it; else the newest automatic
-- one; else the first text the user sent the root agent, as a prompt title.
CREATE VIEW session_title AS
WITH chosen(entry) AS (
  SELECT coalesce(
    (SELECT max(u.entry) FROM title u
      WHERE u.source = 'user' AND u.entry > coalesce(
        (SELECT max(e.seq) FROM entry e
          WHERE e.agent = (SELECT id FROM agent WHERE parent IS NULL) AND e.kind = 'title_cleared'),
        0)),
    (SELECT max(a.entry) FROM title a WHERE a.source <> 'user')))
SELECT t.text, t.source FROM title t WHERE t.entry = (SELECT entry FROM chosen)
UNION ALL
SELECT * FROM (
  SELECT p.text, 'prompt' FROM message_commit mc
    JOIN entry e ON e.seq = mc.entry
    JOIN agent a ON a.id = e.agent AND a.parent IS NULL
    JOIN user_part p ON p.message = mc.message AND p.kind = 'text'
   WHERE (SELECT entry FROM chosen) IS NULL
   ORDER BY mc.entry, p.position LIMIT 1);

-- last_millis is the newest activity entry's time.
CREATE VIEW session_summary AS
SELECT
  (SELECT e.created_millis FROM entry e JOIN entry_kind k ON k.name = e.kind
    WHERE k.activity ORDER BY e.seq DESC LIMIT 1) AS last_millis,
  (SELECT count(*) FROM entry) AS entries,
  coalesce(
     (SELECT ms.profile FROM model_selection ms JOIN entry x ON x.seq = ms.entry
       JOIN agent a ON a.id = x.agent AND a.parent IS NULL ORDER BY ms.entry DESC LIMIT 1),
     (SELECT s.profile FROM agent_start s JOIN entry x ON x.seq = s.entry
       JOIN agent a ON a.id = x.agent AND a.parent IS NULL)) AS profile,
  (SELECT m.name FROM agent_mode am JOIN mode m ON m.id = am.mode JOIN entry x ON x.seq = am.entry
     JOIN agent a ON a.id = x.agent AND a.parent IS NULL ORDER BY am.entry DESC LIMIT 1) AS mode;
