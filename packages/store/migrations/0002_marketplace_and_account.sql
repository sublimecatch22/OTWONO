-- 0002_marketplace_and_account: local records for the account link, the
-- pairing flow and the invitation-only marketplace development MVP.

CREATE TABLE relay_links (
  id             TEXT PRIMARY KEY,
  relay_base_url TEXT NOT NULL,
  account_id     TEXT,
  account_email  TEXT,
  display_name   TEXT,
  -- The token itself lives in the OS credential vault, never here.
  has_token      INTEGER NOT NULL DEFAULT 0,
  scopes         TEXT NOT NULL DEFAULT '[]',
  linked_at      TEXT,
  revoked_at     TEXT,
  created_at     TEXT NOT NULL
) STRICT;

CREATE TABLE pairing_codes (
  code        TEXT PRIMARY KEY,
  scopes      TEXT NOT NULL DEFAULT '[]',
  created_at  TEXT NOT NULL,
  expires_at  TEXT NOT NULL,
  consumed_at TEXT,
  paired_site TEXT
) STRICT;

CREATE TABLE worker_profiles (
  account_id         TEXT PRIMARY KEY,
  headline           TEXT NOT NULL DEFAULT '',
  skills             TEXT NOT NULL DEFAULT '[]',
  equipment          TEXT NOT NULL DEFAULT '[]',
  availability       TEXT NOT NULL DEFAULT '',
  location_radius_km INTEGER,
  portfolio_links    TEXT NOT NULL DEFAULT '[]',
  accepts_on_site    INTEGER NOT NULL DEFAULT 0,
  updated_at         TEXT NOT NULL
) STRICT;

CREATE TABLE listings (
  id                     TEXT PRIMARY KEY,
  creator_account_id     TEXT NOT NULL,
  source_task_id         TEXT REFERENCES tasks(id) ON DELETE SET NULL,
  title                  TEXT NOT NULL,
  description            TEXT NOT NULL DEFAULT '',
  category               TEXT NOT NULL DEFAULT 'general',
  work_mode              TEXT NOT NULL DEFAULT 'remote',
  location_hint          TEXT,
  deliverables           TEXT NOT NULL DEFAULT '[]',
  acceptance_criteria    TEXT NOT NULL DEFAULT '[]',
  evidence_required      TEXT NOT NULL DEFAULT '[]',
  deadline               TEXT,
  compensation_minor     INTEGER NOT NULL DEFAULT 0,
  expenses_minor         INTEGER NOT NULL DEFAULT 0,
  currency               TEXT NOT NULL DEFAULT 'USD',
  safety_class           TEXT NOT NULL DEFAULT 'standard',
  state                  TEXT NOT NULL DEFAULT 'draft',
  simulated_payments     INTEGER NOT NULL DEFAULT 1,
  assigned_application_id TEXT,
  moderation_findings    TEXT NOT NULL DEFAULT '[]',
  created_at             TEXT NOT NULL,
  updated_at             TEXT NOT NULL
) STRICT;
CREATE INDEX idx_listings_state ON listings(state, updated_at DESC);

CREATE TABLE applications (
  id                TEXT PRIMARY KEY,
  listing_id        TEXT NOT NULL REFERENCES listings(id) ON DELETE CASCADE,
  worker_account_id TEXT NOT NULL,
  proposal          TEXT NOT NULL DEFAULT '',
  quoted_minor      INTEGER NOT NULL DEFAULT 0,
  state             TEXT NOT NULL DEFAULT 'submitted',
  created_at        TEXT NOT NULL,
  UNIQUE (listing_id, worker_account_id)
) STRICT;

CREATE TABLE marketplace_messages (
  id                TEXT PRIMARY KEY,
  listing_id        TEXT NOT NULL REFERENCES listings(id) ON DELETE CASCADE,
  sender_account_id TEXT NOT NULL,
  body              TEXT NOT NULL,
  created_at        TEXT NOT NULL
) STRICT;
CREATE INDEX idx_market_messages ON marketplace_messages(listing_id, created_at);

CREATE TABLE submissions (
  id                TEXT PRIMARY KEY,
  listing_id        TEXT NOT NULL REFERENCES listings(id) ON DELETE CASCADE,
  worker_account_id TEXT NOT NULL,
  summary           TEXT NOT NULL DEFAULT '',
  deliverable_links TEXT NOT NULL DEFAULT '[]',
  evidence_notes    TEXT NOT NULL DEFAULT '',
  created_at        TEXT NOT NULL
) STRICT;

-- Simulated ledger. `simulated` is fixed at 1; there is no code path that
-- writes 0, and no adapter in this build can move real funds.
CREATE TABLE marketplace_ledger (
  id                TEXT PRIMARY KEY,
  listing_id        TEXT NOT NULL REFERENCES listings(id) ON DELETE CASCADE,
  entry_type        TEXT NOT NULL,
  amount_minor      INTEGER NOT NULL DEFAULT 0,
  currency          TEXT NOT NULL DEFAULT 'USD',
  account_id        TEXT NOT NULL,
  simulated         INTEGER NOT NULL DEFAULT 1 CHECK (simulated = 1),
  note              TEXT NOT NULL DEFAULT '',
  created_at        TEXT NOT NULL
) STRICT;

CREATE TABLE moderation_reports (
  id                 TEXT PRIMARY KEY,
  listing_id         TEXT NOT NULL REFERENCES listings(id) ON DELETE CASCADE,
  reporter_account_id TEXT NOT NULL,
  reason             TEXT NOT NULL,
  detail             TEXT NOT NULL DEFAULT '',
  state              TEXT NOT NULL DEFAULT 'open',
  created_at         TEXT NOT NULL
) STRICT;

-- Simple fixed-window counters backing the abuse controls.
CREATE TABLE rate_limits (
  bucket       TEXT NOT NULL,
  window_start TEXT NOT NULL,
  count        INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (bucket, window_start)
) STRICT;
