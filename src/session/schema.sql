-- Skyhook session database (application_id 0x534B5948, user_version 23). Tables are STRICT;
-- subtype rows reference entry(seq), whose kind the decoder requires them to match.
-- db/mod.rs adds append-only triggers to tables outside MUTABLE_TABLES. u64 values
-- saturate to i64::MAX.
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
-- `Dictionaries` in db/mod.rs seeds them at create and checks them at open.
CREATE TABLE capability (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

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

-- Activity kinds are the session's activity; closing it, which interrupts its agents, is not.
CREATE TABLE entry_kind (
  name TEXT PRIMARY KEY,
  activity INTEGER NOT NULL CHECK (activity IN (0,1))
) STRICT, WITHOUT ROWID;

CREATE TABLE entry (
  seq INTEGER PRIMARY KEY,                       -- insert NULL ... RETURNING seq
  public_id BLOB NOT NULL UNIQUE CHECK (length(public_id) = 16),
  agent INTEGER NOT NULL REFERENCES agent(id),
  created_millis INTEGER NOT NULL,
  kind TEXT NOT NULL REFERENCES entry_kind(name)
) STRICT;
CREATE UNIQUE INDEX entry_one_agent_start ON entry(agent) WHERE kind = 'agent_started';
-- An agent's latest entry of a kind, for state read without decoding the journal.
CREATE INDEX entry_agent_kind ON entry(agent, kind);

-- Status text. agent_completed / agent_interrupted / session_started / title_cleared carry
-- no subtype row.
CREATE TABLE entry_text (
  entry INTEGER PRIMARY KEY REFERENCES entry(seq),
  text TEXT NOT NULL
) STRICT;

CREATE TABLE title_source (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

-- A title_cleared entry drops the user's earlier titles; db/state.rs TITLE applies the rule.
CREATE TABLE title (
  entry INTEGER PRIMARY KEY REFERENCES entry(seq),
  source TEXT NOT NULL REFERENCES title_source(name),
  text TEXT NOT NULL
) STRICT;

-- ───────────────────────── Targets ─────────────────────────

CREATE TABLE target_source (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE ssh_auth (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

-- via/origin NULL means the route starts at root.
CREATE TABLE target_revision (
  id INTEGER PRIMARY KEY,
  target INTEGER NOT NULL REFERENCES target(id),
  entry INTEGER NOT NULL REFERENCES entry(seq),
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
  CHECK ((ssh_auth = 'key') = (ssh_key_path IS NOT NULL))
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
CREATE TABLE agent_start (
  entry INTEGER PRIMARY KEY REFERENCES entry(seq),
  profile INTEGER NOT NULL REFERENCES model_profile(id),
  location_target INTEGER NOT NULL REFERENCES target(id),
  location_workspace BLOB NOT NULL
) STRICT;

CREATE TABLE agent_capability (
  entry INTEGER NOT NULL REFERENCES entry(seq),
  capability TEXT NOT NULL REFERENCES capability(name),
  PRIMARY KEY (entry, capability)
) STRICT, WITHOUT ROWID;

-- Each edition of a mode the session applied, recorded by the entry that first applied
-- it. The session runs the configured definitions; earlier editions are kept so every
-- entry still shows what it ran under. Versions count up from 1 per name.
CREATE TABLE mode (
  name TEXT NOT NULL,
  version INTEGER NOT NULL CHECK (version > 0),
  entry INTEGER NOT NULL UNIQUE REFERENCES entry(seq),
  instructions TEXT,
  hint TEXT,
  PRIMARY KEY (name, version)
) STRICT, WITHOUT ROWID;

-- What the edition lists, which may exceed what the live ceiling allows; agent_capability
-- rows hold what an agent was actually granted.
CREATE TABLE mode_capability (
  mode_name TEXT NOT NULL,
  mode_version INTEGER NOT NULL,
  capability TEXT NOT NULL REFERENCES capability(name),
  PRIMARY KEY (mode_name, mode_version, capability),
  FOREIGN KEY (mode_name, mode_version) REFERENCES mode(name, version)
) STRICT, WITHOUT ROWID;

-- The agent's mode edition as of this entry.
CREATE TABLE agent_mode (
  entry INTEGER PRIMARY KEY REFERENCES entry(seq),
  mode_name TEXT NOT NULL,
  mode_version INTEGER NOT NULL,
  FOREIGN KEY (mode_name, mode_version) REFERENCES mode(name, version)
) STRICT;

CREATE TABLE model_selection (                   -- ModelChanged only
  entry INTEGER PRIMARY KEY REFERENCES entry(seq),
  profile INTEGER NOT NULL REFERENCES model_profile(id)
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
  entry INTEGER PRIMARY KEY REFERENCES entry(seq),
  purpose TEXT NOT NULL REFERENCES model_purpose(name),
  profile INTEGER NOT NULL REFERENCES model_profile(id),
  system_prompt INTEGER NOT NULL REFERENCES system_prompt(id),
  response_schema_name TEXT,
  response_schema TEXT CHECK (response_schema IS NULL OR json_valid(response_schema)),
  CHECK ((response_schema_name IS NULL) = (response_schema IS NULL))
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
  role TEXT NOT NULL REFERENCES message_role(name)
) STRICT;

-- job_events parts hold their content in user_part_job_event.
CREATE TABLE user_part_kind (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE user_part (
  id INTEGER PRIMARY KEY,
  message INTEGER NOT NULL REFERENCES message(id),
  position INTEGER NOT NULL CHECK (position >= 0),
  kind TEXT NOT NULL REFERENCES user_part_kind(name),
  text TEXT,
  blob BLOB REFERENCES blob(sha256),               -- AttachmentRef
  image_format TEXT REFERENCES image_format(name),
  file TEXT,                                       -- NULL = no file name
  UNIQUE (message, position),
  CHECK ((kind = 'attachment') = (blob IS NOT NULL)),
  CHECK (kind = 'attachment' OR (image_format IS NULL AND file IS NULL))
) STRICT;

-- The agent's state as a request sent it: its date and location, live jobs and todos.
CREATE TABLE message_state (
  message INTEGER PRIMARY KEY REFERENCES message(id),
  date TEXT NOT NULL,
  location_target INTEGER NOT NULL REFERENCES target(id),
  location_workspace BLOB NOT NULL
) STRICT;

-- Jobs in pre-order; parent_position nests an agent job's active children.
-- An agent job has turns/tool_calls, its progress; any other job names its tool.
CREATE TABLE message_state_job (
  message INTEGER NOT NULL REFERENCES message_state(message),
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
  PRIMARY KEY (message, position),
  CHECK ((turns IS NULL) = (tool_calls IS NULL)),
  CHECK ((tool IS NULL) = (turns IS NOT NULL)),
  FOREIGN KEY (message, parent_position) REFERENCES message_state_job(message, position)
) STRICT, WITHOUT ROWID;

CREATE TABLE message_state_todo (
  message INTEGER NOT NULL REFERENCES message_state(message),
  position INTEGER NOT NULL CHECK (position >= 0),
  text TEXT NOT NULL,
  status TEXT NOT NULL REFERENCES todo_status(name),
  PRIMARY KEY (message, position)
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
  message INTEGER NOT NULL REFERENCES message(id),
  position INTEGER NOT NULL CHECK (position >= 0),
  provider_id TEXT NOT NULL CHECK (trim(provider_id) <> ''),
  kind TEXT NOT NULL REFERENCES item_kind(name),
  UNIQUE (message, position)
) STRICT;

-- How tightly a replay is bound to the history that produced it.
CREATE TABLE replay_binding (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

-- The native shape of a replay payload.
CREATE TABLE replay_format (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE reasoning_replay (
  item INTEGER PRIMARY KEY REFERENCES assistant_item(id),
  format TEXT NOT NULL REFERENCES replay_format(name),
  model TEXT NOT NULL,
  scope TEXT NOT NULL CHECK (trim(scope) <> ''),
  payload TEXT NOT NULL CHECK (json_valid(payload)),
  binding TEXT NOT NULL REFERENCES replay_binding(name)
) STRICT;

-- Readable blocks of text and reasoning items.
CREATE TABLE assistant_block (
  item INTEGER NOT NULL REFERENCES assistant_item(id),
  position INTEGER NOT NULL CHECK (position >= 0),
  provider_id TEXT NOT NULL CHECK (trim(provider_id) <> ''),
  text TEXT NOT NULL,
  PRIMARY KEY (item, position)
) STRICT;

CREATE TABLE tool_call (
  item INTEGER PRIMARY KEY REFERENCES assistant_item(id),
  call_id TEXT NOT NULL,
  name TEXT NOT NULL,
  arguments TEXT NOT NULL CHECK (json_valid(arguments) AND json_type(arguments) = 'object')
) STRICT;
CREATE INDEX tool_call_id ON tool_call(call_id);

-- One result per tool message; name derives from the call. History merges a turn's
-- result messages into one Message::Tool ordered by the call's item position.
CREATE TABLE tool_result (
  message INTEGER PRIMARY KEY REFERENCES message(id),
  call INTEGER NOT NULL UNIQUE REFERENCES tool_call(item),
  result TEXT NOT NULL CHECK (json_valid(result)),
  is_error INTEGER NOT NULL CHECK (is_error IN (0,1))
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
  entry INTEGER PRIMARY KEY REFERENCES entry(seq),
  message INTEGER NOT NULL UNIQUE REFERENCES message(id)
) STRICT;

CREATE TABLE todo_status (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE todo_item (
  entry INTEGER NOT NULL REFERENCES entry(seq),
  position INTEGER NOT NULL CHECK (position >= 0),
  text TEXT NOT NULL,
  status TEXT NOT NULL REFERENCES todo_status(name),
  PRIMARY KEY (entry, position)
) STRICT, WITHOUT ROWID;

-- ───────────────────────── Model requests, attempts, compaction ─────────────────────────

-- Whether later requests in a context extend a request's history.
CREATE TABLE history_lifetime (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE model_request (
  entry INTEGER PRIMARY KEY REFERENCES entry(seq),
  context INTEGER NOT NULL REFERENCES model_context(entry),
  -- History is the checkpoint, its retained sources, then the agent's commits after the
  -- checkpoint's frontier up to history_through; history_from drops those before it.
  checkpoint INTEGER REFERENCES compaction(entry),
  history_through INTEGER REFERENCES message_commit(entry),
  history_from INTEGER REFERENCES message_commit(entry),
  history_lifetime TEXT NOT NULL REFERENCES history_lifetime(name)
) STRICT;

CREATE TABLE model_request_tail (
  request INTEGER NOT NULL REFERENCES model_request(entry),
  position INTEGER NOT NULL CHECK (position >= 0),
  message INTEGER NOT NULL REFERENCES message(id),
  PRIMARY KEY (request, position)
) STRICT, WITHOUT ROWID;

CREATE TABLE model_attempt (
  entry INTEGER PRIMARY KEY REFERENCES entry(seq),
  request INTEGER NOT NULL REFERENCES model_request(entry),
  attempt INTEGER NOT NULL CHECK (attempt > 0),
  UNIQUE (request, attempt),
  UNIQUE (entry, request)
) STRICT;

-- How an attempt ended: at most one outcome each. A kind's detail row shares the entry.
CREATE TABLE attempt_outcome (
  entry INTEGER PRIMARY KEY REFERENCES entry(seq),
  attempt INTEGER NOT NULL UNIQUE REFERENCES model_attempt(entry)
) STRICT;

-- How an agent turn or a model attempt failed; detailed kinds keep their detail.
CREATE TABLE failure_kind (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE provider_error_kind (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE failure (
  entry INTEGER PRIMARY KEY REFERENCES entry(seq),
  failure TEXT NOT NULL REFERENCES failure_kind(name),
  detail TEXT,
  -- A provider failure's class; its detail is the provider's message.
  provider TEXT REFERENCES provider_error_kind(name),
  -- The backoff before a retried model failure's next attempt; only a provider failure retries.
  retry_millis INTEGER CHECK (retry_millis >= 0),
  CHECK ((failure = 'provider') = (provider IS NOT NULL)),
  CHECK (retry_millis IS NULL OR provider IS NOT NULL)
) STRICT;

-- How a completed response ended; a refusal is a model failure instead.
CREATE TABLE response_outcome (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE cut_reason (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE model_response (
  entry INTEGER PRIMARY KEY REFERENCES attempt_outcome(entry),
  message INTEGER NOT NULL UNIQUE REFERENCES message_commit(message),
  outcome TEXT NOT NULL REFERENCES response_outcome(name),
  cut_reason TEXT REFERENCES cut_reason(name),
  CHECK ((outcome = 'cut') = (cut_reason IS NOT NULL))
) STRICT;

-- A count is NULL when the provider did not report it.
CREATE TABLE usage (
  entry INTEGER PRIMARY KEY REFERENCES entry(seq),
  request INTEGER NOT NULL REFERENCES model_request(entry),
  input_tokens INTEGER CHECK (input_tokens >= 0),
  cached_input_tokens INTEGER CHECK (cached_input_tokens >= 0),
  cache_write_input_tokens INTEGER CHECK (cache_write_input_tokens >= 0),
  output_tokens INTEGER CHECK (output_tokens >= 0),
  CHECK (cache_write_input_tokens <= input_tokens)
) STRICT;

-- The summary attempt is the entry's attempt_outcome.
CREATE TABLE compaction (
  entry INTEGER PRIMARY KEY REFERENCES attempt_outcome(entry),
  frontier INTEGER NOT NULL REFERENCES entry(seq),
  message INTEGER NOT NULL REFERENCES message(id),
  -- The occupancy that triggered compaction; NULL when an overflow did.
  reported_tokens INTEGER CHECK (reported_tokens >= 0),
  CHECK (frontier < entry)
) STRICT;

CREATE TABLE compaction_retained (
  compaction INTEGER NOT NULL REFERENCES compaction(entry),
  source INTEGER NOT NULL REFERENCES message_commit(entry),
  PRIMARY KEY (compaction, source)
) STRICT, WITHOUT ROWID;

CREATE TABLE compaction_fault (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE checkpoint_error (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE compaction_failure (              -- CompactionFailed only
  entry INTEGER PRIMARY KEY REFERENCES entry(seq),
  request INTEGER REFERENCES model_request(entry),
  -- The summary attempt this round used. It may also hold a model failure, so it is a
  -- reference rather than an attempt_outcome.
  attempt INTEGER,
  fault TEXT NOT NULL REFERENCES compaction_fault(name),
  detail TEXT,
  -- A checkpoint fault's detail names the checkpoint error, a summary fault its failure.
  checkpoint TEXT GENERATED ALWAYS AS (CASE fault WHEN 'checkpoint' THEN detail END) VIRTUAL,
  summary TEXT GENERATED ALWAYS AS (CASE fault WHEN 'summary' THEN detail END) VIRTUAL,
  CHECK (attempt IS NULL OR request IS NOT NULL),
  FOREIGN KEY (attempt, request) REFERENCES model_attempt(entry, request),
  FOREIGN KEY (checkpoint) REFERENCES checkpoint_error(name),
  FOREIGN KEY (summary) REFERENCES failure_kind(name)
) STRICT;

-- ───────────────────────── Jobs and outputs ─────────────────────────

CREATE TABLE job_role (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE job_state (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE job (
  id INTEGER PRIMARY KEY CHECK (id > 0),         -- JobId
  created INTEGER NOT NULL UNIQUE REFERENCES entry(seq),
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
  location_workspace BLOB NOT NULL
) STRICT;

CREATE TABLE job_transition (
  entry INTEGER PRIMARY KEY REFERENCES entry(seq),
  job INTEGER NOT NULL REFERENCES job(id),
  state TEXT NOT NULL REFERENCES job_state(name)
) STRICT;

-- Output belongs to one run, saved once with its finish: a reset never presents an
-- earlier run's output. A generation counts the job's restarts after a finish.
CREATE TABLE job_output (
  job INTEGER NOT NULL REFERENCES job(id),
  generation INTEGER NOT NULL CHECK (generation >= 0),
  -- Every capture the producer finished was admitted into the result.
  captures_complete INTEGER NOT NULL CHECK (captures_complete IN (0,1)),
  -- The tool's result, NULL when the run produced none ('null' is a literal null);
  -- referenced captures are emptied placeholders.
  result TEXT CHECK (result IS NULL OR json_valid(result)),
  PRIMARY KEY (job, generation)
) STRICT;

CREATE TABLE capture_kind (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;
CREATE TABLE capture_detection (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

-- Streamed or offloaded bytes at one JSON Pointer. A row exists from reservation, so
-- partial output stays readable; an abandoned builtin capture deletes its row.
CREATE TABLE job_capture (
  id INTEGER PRIMARY KEY,
  job INTEGER NOT NULL REFERENCES job(id),
  generation INTEGER NOT NULL CHECK (generation >= 0),
  pointer TEXT NOT NULL CHECK (pointer = '' OR pointer LIKE '/%'),
  capture_kind TEXT NOT NULL REFERENCES capture_kind(name),
  -- What a finished text capture its schema declares as JSON holds.
  detection TEXT REFERENCES capture_detection(name),
  CHECK (detection IS NULL OR capture_kind = 'text'),
  UNIQUE (job, generation, pointer),
  UNIQUE (id, job, generation)
) STRICT;

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
  job INTEGER NOT NULL,
  generation INTEGER NOT NULL,
  capture INTEGER NOT NULL,
  PRIMARY KEY (job, generation, capture),
  FOREIGN KEY (job, generation) REFERENCES job_output(job, generation),
  FOREIGN KEY (capture, job, generation) REFERENCES job_capture(id, job, generation)
) STRICT, WITHOUT ROWID;
CREATE INDEX job_output_field_capture ON job_output_field(capture);

CREATE TABLE field_presentation (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

-- Fields of a saved result whose presentation is declared: by its schema, or
-- carried by a script's returned tool results.
CREATE TABLE job_field_presentation (
  job INTEGER NOT NULL,
  generation INTEGER NOT NULL,
  pointer TEXT NOT NULL,
  presentation TEXT NOT NULL REFERENCES field_presentation(name),
  PRIMARY KEY (job, generation, pointer),
  FOREIGN KEY (job, generation) REFERENCES job_output(job, generation)
) STRICT, WITHOUT ROWID;

CREATE TABLE job_finish (
  entry INTEGER PRIMARY KEY REFERENCES entry(seq),
  job INTEGER NOT NULL REFERENCES job(id),
  state TEXT NOT NULL REFERENCES job_state(name)
) STRICT;

-- Diagnostics retain typed terminal facts, never rendered errors.
CREATE TABLE diagnostic_operation (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE diagnostic_subject (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE diagnostic_site (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE diagnostic_effects (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE diagnostic_request_effects (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE diagnostic_cause (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE diagnostic_io_kind (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE diagnostic_path_role (name TEXT PRIMARY KEY) STRICT, WITHOUT ROWID;

CREATE TABLE job_finish_diagnostic (
  finish INTEGER PRIMARY KEY REFERENCES job_finish(entry),
  operation TEXT NOT NULL REFERENCES diagnostic_operation(name),
  subject TEXT NOT NULL REFERENCES diagnostic_subject(name),
  subject_path BLOB,
  subject_text TEXT,
  -- The subject may be an unsuccessful lookup of a job that does not exist.
  subject_job INTEGER CHECK (subject_job > 0),
  site TEXT NOT NULL REFERENCES diagnostic_site(name),
  location_target INTEGER REFERENCES target(id),
  location_workspace BLOB,
  effects TEXT NOT NULL REFERENCES diagnostic_effects(name),
  request TEXT NOT NULL REFERENCES diagnostic_request_effects(name),
  cause TEXT NOT NULL REFERENCES diagnostic_cause(name),
  io_kind TEXT REFERENCES diagnostic_io_kind(name),
  io_code INTEGER CHECK (io_code BETWEEN -2147483648 AND 2147483647),
  io_detail TEXT,
  cause_text TEXT,
  cause_job INTEGER CHECK (cause_job > 0),
  CHECK ((subject = 'job') = (subject_job IS NOT NULL)),
  CHECK ((site = 'execution') = (location_target IS NOT NULL)),
  CHECK ((site = 'execution') = (location_workspace IS NOT NULL)),
  CHECK ((cause = 'io') = (io_kind IS NOT NULL)),
  CHECK (cause = 'io' OR io_code IS NULL),
  CHECK (cause = 'io' OR io_detail IS NULL)
) STRICT;

-- Path facts retain caller order and native bytes separately from the attempted subject.
CREATE TABLE diagnostic_path (
  finish INTEGER NOT NULL REFERENCES job_finish_diagnostic(finish),
  position INTEGER NOT NULL CHECK (position >= 0),
  role TEXT NOT NULL REFERENCES diagnostic_path_role(name),
  path BLOB NOT NULL,
  PRIMARY KEY (finish, position)
) STRICT, WITHOUT ROWID;

CREATE TABLE job_finish_image (
  finish INTEGER NOT NULL REFERENCES job_finish(entry),
  position INTEGER NOT NULL CHECK (position >= 0),
  blob BLOB NOT NULL REFERENCES blob(sha256),
  format TEXT NOT NULL REFERENCES image_format(name),
  file TEXT,
  PRIMARY KEY (finish, position)
) STRICT, WITHOUT ROWID;

CREATE TABLE job_delivery (
  entry INTEGER PRIMARY KEY REFERENCES entry(seq),
  job INTEGER NOT NULL REFERENCES job(id),
  -- A delivered child reply and the runtime message that carried it.
  source INTEGER REFERENCES message_commit(entry),
  notification INTEGER REFERENCES message_commit(entry),
  CHECK ((source IS NULL) = (notification IS NULL))
) STRICT;
