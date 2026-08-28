-- 0001_core: settings, providers, workspaces, agents, conversations, knowledge,
-- projects, permissions, activity, budgets.

CREATE TABLE settings (
  key         TEXT PRIMARY KEY,
  value       TEXT NOT NULL,
  updated_at  TEXT NOT NULL
) STRICT;

CREATE TABLE provider_connections (
  id                      TEXT PRIMARY KEY,
  kind                    TEXT NOT NULL,
  label                   TEXT NOT NULL,
  endpoint                TEXT NOT NULL,
  enabled                 INTEGER NOT NULL DEFAULT 0,
  has_credential          INTEGER NOT NULL DEFAULT 0,
  default_model           TEXT,
  default_embedding_model TEXT,
  created_at              TEXT NOT NULL,
  updated_at              TEXT NOT NULL
) STRICT;

CREATE TABLE workspaces (
  id                   TEXT PRIMARY KEY,
  kind                 TEXT NOT NULL,
  name                 TEXT NOT NULL,
  description          TEXT NOT NULL DEFAULT '',
  icon                 TEXT NOT NULL DEFAULT 'workspace',
  shared_instructions  TEXT NOT NULL DEFAULT '',
  knowledge_source_ids TEXT NOT NULL DEFAULT '[]',
  coordinator_agent_id TEXT,
  favorite             INTEGER NOT NULL DEFAULT 0,
  archived             INTEGER NOT NULL DEFAULT 0,
  ordinal              INTEGER NOT NULL DEFAULT 0,
  created_at           TEXT NOT NULL,
  updated_at           TEXT NOT NULL
) STRICT;
CREATE INDEX idx_workspaces_kind ON workspaces(kind, archived);

CREATE TABLE agents (
  id                     TEXT PRIMARY KEY,
  name                   TEXT NOT NULL,
  role                   TEXT NOT NULL DEFAULT '',
  description            TEXT NOT NULL DEFAULT '',
  icon                   TEXT NOT NULL DEFAULT 'agent',
  system_instructions    TEXT NOT NULL DEFAULT '',
  provider_connection_id TEXT REFERENCES provider_connections(id) ON DELETE SET NULL,
  model                  TEXT,
  parameters             TEXT NOT NULL DEFAULT '{}',
  capabilities           TEXT NOT NULL DEFAULT '[]',
  knowledge_source_ids   TEXT NOT NULL DEFAULT '[]',
  memory_scope           TEXT NOT NULL DEFAULT 'conversation',
  approval_policy        TEXT NOT NULL DEFAULT 'off_device_only',
  max_steps              INTEGER NOT NULL DEFAULT 12,
  timeout_seconds        INTEGER NOT NULL DEFAULT 120,
  workspace_id           TEXT REFERENCES workspaces(id) ON DELETE SET NULL,
  version                INTEGER NOT NULL DEFAULT 1,
  is_template            INTEGER NOT NULL DEFAULT 0,
  template_key           TEXT,
  archived               INTEGER NOT NULL DEFAULT 0,
  created_at             TEXT NOT NULL,
  updated_at             TEXT NOT NULL
) STRICT;
CREATE INDEX idx_agents_workspace ON agents(workspace_id);
CREATE UNIQUE INDEX idx_agents_template_key ON agents(template_key) WHERE template_key IS NOT NULL;

-- Every save writes a snapshot so a configuration change can be inspected and
-- rolled back.
CREATE TABLE agent_versions (
  id         TEXT PRIMARY KEY,
  agent_id   TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  version    INTEGER NOT NULL,
  snapshot   TEXT NOT NULL,
  note       TEXT,
  created_at TEXT NOT NULL,
  UNIQUE (agent_id, version)
) STRICT;

CREATE TABLE workspace_members (
  workspace_id   TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  agent_id       TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
  job_role       TEXT NOT NULL DEFAULT '',
  is_coordinator INTEGER NOT NULL DEFAULT 0,
  ordinal        INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (workspace_id, agent_id)
) STRICT;

CREATE TABLE conversations (
  id                     TEXT PRIMARY KEY,
  title                  TEXT NOT NULL,
  workspace_id           TEXT REFERENCES workspaces(id) ON DELETE SET NULL,
  agent_id               TEXT REFERENCES agents(id) ON DELETE SET NULL,
  provider_connection_id TEXT REFERENCES provider_connections(id) ON DELETE SET NULL,
  model                  TEXT,
  knowledge_source_ids   TEXT NOT NULL DEFAULT '[]',
  pinned                 INTEGER NOT NULL DEFAULT 0,
  archived               INTEGER NOT NULL DEFAULT 0,
  created_at             TEXT NOT NULL,
  updated_at             TEXT NOT NULL
) STRICT;
CREATE INDEX idx_conversations_updated ON conversations(archived, updated_at DESC);

CREATE TABLE messages (
  id                     TEXT PRIMARY KEY,
  conversation_id        TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
  role                   TEXT NOT NULL,
  content                TEXT NOT NULL,
  citations              TEXT NOT NULL DEFAULT '[]',
  attachments            TEXT NOT NULL DEFAULT '[]',
  model                  TEXT,
  provider_connection_id TEXT,
  token_estimate         INTEGER,
  stopped_reason         TEXT,
  ordinal                INTEGER NOT NULL,
  created_at             TEXT NOT NULL
) STRICT;
CREATE INDEX idx_messages_conversation ON messages(conversation_id, ordinal);

CREATE TABLE knowledge_sources (
  id              TEXT PRIMARY KEY,
  label           TEXT NOT NULL,
  root_path       TEXT NOT NULL UNIQUE,
  is_directory    INTEGER NOT NULL DEFAULT 1,
  authorised      INTEGER NOT NULL DEFAULT 1,
  include_globs   TEXT NOT NULL DEFAULT '[]',
  exclude_globs   TEXT NOT NULL DEFAULT '[]',
  embedding_model TEXT NOT NULL DEFAULT 'lexical-fallback',
  last_indexed_at TEXT,
  created_at      TEXT NOT NULL
) STRICT;

CREATE TABLE documents (
  id           TEXT PRIMARY KEY,
  source_id    TEXT NOT NULL REFERENCES knowledge_sources(id) ON DELETE CASCADE,
  path         TEXT NOT NULL,
  file_name    TEXT NOT NULL,
  format       TEXT NOT NULL,
  byte_size    INTEGER NOT NULL DEFAULT 0,
  content_hash TEXT NOT NULL DEFAULT '',
  modified_at  TEXT,
  state        TEXT NOT NULL DEFAULT 'pending',
  error        TEXT,
  chunk_count  INTEGER NOT NULL DEFAULT 0,
  indexed_at   TEXT,
  UNIQUE (source_id, path)
) STRICT;
CREATE INDEX idx_documents_state ON documents(source_id, state);

CREATE TABLE chunks (
  id             TEXT PRIMARY KEY,
  document_id    TEXT NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  source_id      TEXT NOT NULL REFERENCES knowledge_sources(id) ON DELETE CASCADE,
  chunk_index    INTEGER NOT NULL,
  text           TEXT NOT NULL,
  locator        TEXT,
  token_estimate INTEGER NOT NULL DEFAULT 0,
  UNIQUE (document_id, chunk_index)
) STRICT;
CREATE INDEX idx_chunks_source ON chunks(source_id);

CREATE TABLE chunk_vectors (
  chunk_id  TEXT PRIMARY KEY REFERENCES chunks(id) ON DELETE CASCADE,
  source_id TEXT NOT NULL,
  model     TEXT NOT NULL,
  dim       INTEGER NOT NULL,
  vector    BLOB NOT NULL
) STRICT;
CREATE INDEX idx_chunk_vectors_source ON chunk_vectors(source_id);

CREATE TABLE projects (
  id                     TEXT PRIMARY KEY,
  title                  TEXT NOT NULL,
  objective              TEXT NOT NULL DEFAULT '',
  acceptance_criteria    TEXT NOT NULL DEFAULT '[]',
  state                  TEXT NOT NULL DEFAULT 'draft',
  workspace_id           TEXT REFERENCES workspaces(id) ON DELETE SET NULL,
  orchestrator_agent_id  TEXT REFERENCES agents(id) ON DELETE SET NULL,
  verifier_agent_id      TEXT REFERENCES agents(id) ON DELETE SET NULL,
  max_steps              INTEGER NOT NULL DEFAULT 40,
  max_task_retries       INTEGER NOT NULL DEFAULT 2,
  budget_id              TEXT,
  sync_enabled           INTEGER NOT NULL DEFAULT 0,
  created_at             TEXT NOT NULL,
  updated_at             TEXT NOT NULL
) STRICT;
CREATE INDEX idx_projects_state ON projects(state, updated_at DESC);

CREATE TABLE tasks (
  id                  TEXT PRIMARY KEY,
  project_id          TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  ordinal             INTEGER NOT NULL,
  title               TEXT NOT NULL,
  instructions        TEXT NOT NULL DEFAULT '',
  acceptance_criteria TEXT NOT NULL DEFAULT '[]',
  state               TEXT NOT NULL DEFAULT 'queued',
  assigned_agent_id   TEXT REFERENCES agents(id) ON DELETE SET NULL,
  requires_approval   INTEGER NOT NULL DEFAULT 0,
  attempt             INTEGER NOT NULL DEFAULT 0,
  max_attempts        INTEGER NOT NULL DEFAULT 3,
  output              TEXT,
  failure_reason      TEXT,
  verification_notes  TEXT,
  created_at          TEXT NOT NULL,
  updated_at          TEXT NOT NULL
) STRICT;
CREATE INDEX idx_tasks_project ON tasks(project_id, ordinal);
CREATE INDEX idx_tasks_state ON tasks(state);

CREATE TABLE task_dependencies (
  task_id            TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  depends_on_task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  PRIMARY KEY (task_id, depends_on_task_id)
) STRICT;

CREATE TABLE artifacts (
  id         TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  task_id    TEXT REFERENCES tasks(id) ON DELETE SET NULL,
  name       TEXT NOT NULL,
  media_type TEXT NOT NULL DEFAULT 'text/markdown',
  path       TEXT NOT NULL,
  byte_size  INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL
) STRICT;

CREATE TABLE permission_grants (
  id                TEXT PRIMARY KEY,
  capability        TEXT NOT NULL,
  scopes            TEXT NOT NULL DEFAULT '[]',
  decision          TEXT NOT NULL,
  spend_limit_minor INTEGER,
  spend_category    TEXT,
  expires_at        TEXT,
  revoked_at        TEXT,
  created_at        TEXT NOT NULL,
  created_by        TEXT NOT NULL DEFAULT 'user',
  note              TEXT
) STRICT;
CREATE INDEX idx_grants_capability ON permission_grants(capability, revoked_at);

CREATE TABLE permission_requests (
  id                     TEXT PRIMARY KEY,
  capability             TEXT NOT NULL,
  scopes                 TEXT NOT NULL DEFAULT '[]',
  summary                TEXT NOT NULL,
  requested_by_agent_id  TEXT,
  project_id             TEXT,
  task_id                TEXT,
  created_at             TEXT NOT NULL,
  resolved_at            TEXT,
  resolution             TEXT
) STRICT;
CREATE INDEX idx_requests_open ON permission_requests(resolved_at);

CREATE TABLE activity_log (
  id          TEXT PRIMARY KEY,
  created_at  TEXT NOT NULL,
  actor_type  TEXT NOT NULL,
  actor_id    TEXT,
  actor_name  TEXT,
  action      TEXT NOT NULL,
  target_type TEXT,
  target_id   TEXT,
  project_id  TEXT,
  task_id     TEXT,
  outcome     TEXT NOT NULL DEFAULT 'ok',
  detail      TEXT NOT NULL DEFAULT '{}'
) STRICT;
CREATE INDEX idx_activity_created ON activity_log(created_at DESC);
CREATE INDEX idx_activity_project ON activity_log(project_id, created_at DESC);

CREATE TABLE budgets (
  id                        TEXT PRIMARY KEY,
  project_id                TEXT REFERENCES projects(id) ON DELETE CASCADE,
  name                      TEXT NOT NULL,
  currency                  TEXT NOT NULL DEFAULT 'USD',
  total_minor               INTEGER NOT NULL DEFAULT 0,
  approval_threshold_minor  INTEGER NOT NULL DEFAULT 0,
  simulated                 INTEGER NOT NULL DEFAULT 1,
  created_at                TEXT NOT NULL
) STRICT;

CREATE TABLE expenses (
  id            TEXT PRIMARY KEY,
  budget_id     TEXT NOT NULL REFERENCES budgets(id) ON DELETE CASCADE,
  task_id       TEXT REFERENCES tasks(id) ON DELETE SET NULL,
  category      TEXT NOT NULL DEFAULT 'general',
  description   TEXT NOT NULL DEFAULT '',
  amount_minor  INTEGER NOT NULL DEFAULT 0,
  state         TEXT NOT NULL DEFAULT 'estimated',
  receipt_path  TEXT,
  approved_by   TEXT,
  approved_at   TEXT,
  simulated     INTEGER NOT NULL DEFAULT 1,
  created_at    TEXT NOT NULL
) STRICT;
CREATE INDEX idx_expenses_budget ON expenses(budget_id);

CREATE TABLE sessions (
  id                   TEXT PRIMARY KEY,
  workspace_id         TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  question             TEXT NOT NULL,
  stage                TEXT NOT NULL DEFAULT 'positions',
  chair_agent_id       TEXT REFERENCES agents(id) ON DELETE SET NULL,
  synthesis            TEXT,
  dissent_summary      TEXT,
  unresolved_questions TEXT NOT NULL DEFAULT '[]',
  recommended_decision TEXT,
  created_at           TEXT NOT NULL,
  updated_at           TEXT NOT NULL
) STRICT;

CREATE TABLE session_contributions (
  id         TEXT PRIMARY KEY,
  session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  agent_id   TEXT NOT NULL,
  agent_name TEXT NOT NULL,
  stage      TEXT NOT NULL,
  content    TEXT NOT NULL,
  claim_kind TEXT NOT NULL DEFAULT 'speculation',
  citations  TEXT NOT NULL DEFAULT '[]',
  ordinal    INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL
) STRICT;
CREATE INDEX idx_contributions_session ON session_contributions(session_id, ordinal);

-- Lab experiments: a run of one prompt against several configurations.
CREATE TABLE lab_experiments (
  id           TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  name         TEXT NOT NULL,
  prompt       TEXT NOT NULL,
  variants     TEXT NOT NULL DEFAULT '[]',
  results      TEXT NOT NULL DEFAULT '[]',
  promoted_variant TEXT,
  created_at   TEXT NOT NULL,
  updated_at   TEXT NOT NULL
) STRICT;
