/**
 * Shapes returned by the local service.
 *
 * These mirror the Rust types in `otwono-types` and the route response structs.
 * They are hand-written rather than generated so that the client compiles
 * without a build step against the service.
 */

import type { Preferences } from '@otwono/ui';
import type { Citation } from './client';

export type { Preferences, Citation };

export interface SystemStatus {
  version: string;
  schema_version: number;
  started_at: string;
  data_directory: string;
  secret_backend: 'operating_system' | 'encrypted_file' | 'ephemeral';
  secret_backend_detail: string;
  emergency_stop: boolean;
  open_permission_requests: number;
  telemetry_opt_in: boolean;
  onboarding_complete: boolean;
}

export interface PreferenceOptions {
  themes: string[];
  accents: string[];
  backgrounds: string[];
  fonts: string[];
  densities: string[];
  tabs: string[];
  required_tabs: string[];
  widgets: string[];
  font_size_range: [number, number];
  sidebar_width_range: [number, number];
  chat_width_range: [number, number];
}

export interface PreferencesResponse {
  preferences: Preferences;
  options: PreferenceOptions;
}

export type ProviderKind = 'ollama' | 'lmstudio' | 'openai_compatible';

export interface Capabilities {
  chat: boolean;
  streaming: boolean;
  tool_calling: boolean;
  structured_output: boolean;
  vision: boolean;
  embeddings: boolean;
  context_length: number | null;
}

export interface ModelInfo {
  id: string;
  display_name: string;
  capabilities: Capabilities;
  capability_source: 'reported' | 'probed' | 'inferred';
  parameter_size: string | null;
  quantization: string | null;
}

export interface ProviderConnection {
  id: string;
  kind: ProviderKind;
  label: string;
  endpoint: string;
  has_credential: boolean;
  enabled: boolean;
  default_model: string | null;
  default_embedding_model: string | null;
}

export interface ConnectionsResponse {
  connections: ProviderConnection[];
  ready_for_chat: boolean;
  guidance: string | null;
}

export interface ConnectionTest {
  health: 'unknown' | 'reachable' | 'unreachable' | 'authentication_required';
  detail: string;
  models: ModelInfo[];
  latency_ms: number | null;
}

export interface DetectedRuntime {
  kind: ProviderKind;
  display_name: string;
  endpoint: string;
  test: ConnectionTest;
  usable: boolean;
  existing_connection_id: string | null;
}

export interface DetectionResponse {
  found: DetectedRuntime[];
  guidance: string;
}

export type Capability =
  | 'file_read'
  | 'file_write'
  | 'knowledge_search'
  | 'http_fetch'
  | 'artifact_create'
  | 'budget_record'
  | 'marketplace_publish'
  | 'relay_sync';

export interface Agent {
  id: string;
  name: string;
  role: string;
  description: string;
  icon: string;
  system_instructions: string;
  provider_connection_id: string | null;
  model: string | null;
  parameters: {
    temperature: number | null;
    top_p: number | null;
    max_output_tokens: number | null;
    stop: string[];
    extra: Record<string, unknown>;
  };
  capabilities: Capability[];
  knowledge_source_ids: string[];
  memory_scope: 'none' | 'conversation' | 'project' | 'workspace';
  approval_policy: 'always' | 'off_device_only' | 'standing';
  max_steps: number;
  timeout_seconds: number;
  workspace_id: string | null;
  version: number;
  is_template: boolean;
  template_key: string | null;
  created_at: string;
  updated_at: string;
}

export interface AgentTemplateSummary {
  key: string;
  name: string;
  role: string;
  description: string;
  icon: string;
  capabilities: string[];
  agent_id: string | null;
}

export interface Conversation {
  id: string;
  title: string;
  workspace_id: string | null;
  agent_id: string | null;
  provider_connection_id: string | null;
  model: string | null;
  knowledge_source_ids: string[];
  pinned: boolean;
  archived: boolean;
  created_at: string;
  updated_at: string;
}

export interface Message {
  id: string;
  conversation_id: string;
  role: 'system' | 'user' | 'assistant' | 'tool';
  content: string;
  citations: Citation[];
  attachments: unknown[];
  model: string | null;
  provider_connection_id: string | null;
  token_estimate: number | null;
  stopped_reason: string | null;
  created_at: string;
}

export interface ConversationDetail extends Conversation {
  messages: Message[];
}

export type WorkspaceKind = 'chat' | 'office' | 'lab' | 'boardroom' | 'think_tank';

export interface Workspace {
  id: string;
  kind: WorkspaceKind;
  name: string;
  description: string;
  icon: string;
  shared_instructions: string;
  knowledge_source_ids: string[];
  coordinator_agent_id: string | null;
  favorite: boolean;
  archived: boolean;
  created_at: string;
  updated_at: string;
}

export interface WorkspaceSummary extends Workspace {
  member_count: number;
  purpose: string;
  runs_sessions: boolean;
}

export interface WorkspaceKindDescription {
  kind: WorkspaceKind;
  display_name: string;
  purpose: string;
  runs_sessions: boolean;
}

export interface SessionContribution {
  id: string;
  session_id: string;
  agent_id: string;
  agent_name: string;
  stage: 'positions' | 'critique' | 'synthesis' | 'completed' | 'failed';
  content: string;
  claim_kind: 'sourced' | 'speculation';
  citations: Citation[];
  created_at: string;
}

export interface Session {
  id: string;
  workspace_id: string;
  question: string;
  stage: 'positions' | 'critique' | 'synthesis' | 'completed' | 'failed';
  chair_agent_id: string | null;
  synthesis: string | null;
  dissent_summary: string | null;
  unresolved_questions: string[];
  recommended_decision: string | null;
  created_at: string;
  updated_at: string;
}

export interface SessionDetail extends Session {
  contributions: SessionContribution[];
}

export interface LabVariant {
  id: string;
  label: string;
  agent_id: string | null;
  provider_connection_id: string | null;
  model: string | null;
  system_instructions: string | null;
  temperature: number | null;
}

export interface LabResult {
  variant_id: string;
  output: string;
  error: string | null;
  latency_ms: number;
  token_estimate: number | null;
  ran_at: string;
}

export interface LabExperiment {
  id: string;
  workspace_id: string;
  name: string;
  prompt: string;
  variants: LabVariant[];
  results: LabResult[];
  promoted_variant: string | null;
  created_at: string;
  updated_at: string;
}

export interface WorkspaceDetail extends WorkspaceSummary {
  members: { agent: Agent; job_role: string; is_coordinator: boolean }[];
  sessions: Session[];
  experiments: LabExperiment[];
}

export type ProjectState =
  | 'draft'
  | 'planned'
  | 'awaiting_approval'
  | 'running'
  | 'blocked'
  | 'verifying'
  | 'completed'
  | 'failed'
  | 'cancelled'
  | 'archived';

export type TaskState =
  | 'queued'
  | 'ready'
  | 'running'
  | 'awaiting_approval'
  | 'blocked'
  | 'verifying'
  | 'completed'
  | 'failed'
  | 'cancelled';

export interface Task {
  id: string;
  project_id: string;
  ordinal: number;
  title: string;
  instructions: string;
  acceptance_criteria: string[];
  state: TaskState;
  assigned_agent_id: string | null;
  depends_on: string[];
  requires_approval: boolean;
  attempt: number;
  max_attempts: number;
  output: string | null;
  failure_reason: string | null;
  verification_notes: string | null;
  created_at: string;
  updated_at: string;
}

export interface Project {
  id: string;
  title: string;
  objective: string;
  acceptance_criteria: string[];
  state: ProjectState;
  workspace_id: string | null;
  orchestrator_agent_id: string | null;
  verifier_agent_id: string | null;
  max_steps: number;
  max_task_retries: number;
  budget_id: string | null;
  sync_enabled: boolean;
  created_at: string;
  updated_at: string;
}

export interface ProjectSummary extends Project {
  task_count: number;
  completed_tasks: number;
  awaiting_approval: number;
}

export interface Artifact {
  id: string;
  project_id: string;
  task_id: string | null;
  name: string;
  media_type: string;
  path: string;
  byte_size: number;
  created_at: string;
}

export interface ProjectDetail extends Project {
  tasks: Task[];
  artifacts: Artifact[];
}

export interface RunReport {
  project_id: string;
  steps_used: number;
  tasks_completed: number;
  tasks_failed: number;
  tasks_reworked: number;
  awaiting_approval: string[];
  final_state: string;
  stopped_because: string;
}

export interface KnowledgeSource {
  id: string;
  label: string;
  root_path: string;
  is_directory: boolean;
  authorised: boolean;
  include_globs: string[];
  exclude_globs: string[];
  embedding_model: string;
  document_count: number;
  chunk_count: number;
  last_indexed_at: string | null;
  created_at: string;
  embedding_is_fallback: boolean;
  embedding_detail: string;
  exists_on_disk: boolean;
}

export interface SourcesResponse {
  sources: KnowledgeSource[];
  retrieval_notice: string | null;
}

export interface KnowledgeDocument {
  id: string;
  source_id: string;
  path: string;
  file_name: string;
  format: string;
  byte_size: number;
  content_hash: string;
  modified_at: string | null;
  state: 'pending' | 'parsing' | 'indexing' | 'indexed' | 'failed' | 'removed' | 'skipped';
  error: string | null;
  chunk_count: number;
  indexed_at: string | null;
}

export interface IndexResponse {
  scanned: number;
  indexed: number;
  unchanged: number;
  skipped: number;
  failed: number;
  removed: number;
  chunks: number;
  failures: string[];
  truncated: boolean;
  embedding_model: string;
  used_fallback_embeddings: boolean;
  message: string;
}

export interface BrowseEntry {
  name: string;
  path: string;
  is_directory: boolean;
  supported: boolean;
}

export interface BrowseResponse {
  path: string;
  parent: string | null;
  entries: BrowseEntry[];
}

export interface SearchResponse {
  hits: {
    chunk: { id: string; text: string; locator: string | null; index: number };
    file_name: string;
    file_path: string;
    score: number;
    vector_score: number;
    lexical_score: number;
  }[];
  citations: Citation[];
  searched_sources: number;
  used_fallback_embeddings: boolean;
}

export type Scope =
  | { type: 'global' }
  | { type: 'project'; project_id: string }
  | { type: 'workspace'; workspace_id: string }
  | { type: 'agent'; agent_id: string }
  | { type: 'path'; path: string }
  | { type: 'host'; host: string }
  | { type: 'connector'; connector_id: string };

export interface Grant {
  id: string;
  capability: Capability;
  scopes: Scope[];
  decision: 'allow' | 'allow_once' | 'deny';
  spend_limit_minor: number | null;
  spend_category: string | null;
  expires_at: string | null;
  revoked_at: string | null;
  created_at: string;
  created_by: string;
  note: string | null;
}

export interface PermissionRequest {
  id: string;
  capability: Capability;
  scopes: Scope[];
  summary: string;
  requested_by_agent_id: string | null;
  project_id: string | null;
  task_id: string | null;
  created_at: string;
  resolved_at: string | null;
  resolution: string | null;
}

export interface PermissionsResponse {
  grants: Grant[];
  open_requests: PermissionRequest[];
  emergency_stop: boolean;
  capabilities: { capability: Capability; human_request: string; leaves_device: boolean }[];
}

export interface BudgetSummaryValues {
  budget_id: string;
  currency: string;
  total_minor: number;
  committed_minor: number;
  pending_minor: number;
  remaining_minor: number;
  simulated: boolean;
}

export interface Budget {
  id: string;
  project_id: string | null;
  name: string;
  currency: string;
  total_minor: number;
  approval_threshold_minor: number;
  simulated: boolean;
  created_at: string;
  summary: BudgetSummaryValues;
}

export interface Expense {
  id: string;
  budget_id: string;
  task_id: string | null;
  category: string;
  description: string;
  amount_minor: number;
  state: 'estimated' | 'awaiting_approval' | 'approved' | 'rejected' | 'settled' | 'cancelled';
  receipt_path: string | null;
  approved_by: string | null;
  approved_at: string | null;
  simulated: boolean;
  created_at: string;
}

export type ListingState =
  | 'draft'
  | 'awaiting_creator_approval'
  | 'published'
  | 'assigned'
  | 'submitted'
  | 'revision_requested'
  | 'accepted'
  | 'disputed'
  | 'cancelled'
  | 'rejected';

export interface Listing {
  id: string;
  creator_account_id: string;
  source_task_id: string | null;
  title: string;
  description: string;
  category: string;
  work_mode: 'remote' | 'on_site';
  location_hint: string | null;
  deliverables: string[];
  acceptance_criteria: string[];
  evidence_required: string[];
  deadline: string | null;
  compensation_minor: number;
  expenses_minor: number;
  currency: string;
  safety_class: 'standard' | 'physical_on_site' | 'handles_personal_data';
  state: ListingState;
  simulated_payments: boolean;
  assigned_application_id: string | null;
  created_at: string;
  updated_at: string;
}

/** The receipt from a synchronisation: exactly what left the machine. */
export interface SyncResult {
  synchronised: number;
  titles: string[];
  sent_at: string;
  what_was_sent: string;
}

export interface ModerationFinding {
  category: string;
  explanation: string;
  matched: string;
}

/**
 * What moderation decided. `Refused` carries the phrases that matched and the
 * route to a person, so a creator is never left guessing or stuck.
 */
export type ModerationVerdict =
  | 'Allowed'
  | { Refused: { findings: ModerationFinding[]; escalation: string } };

export interface Application {
  id: string;
  listing_id: string;
  worker_account_id: string;
  proposal: string;
  quoted_minor: number;
  state: 'submitted' | 'withdrawn' | 'declined' | 'assigned';
  created_at: string;
}

export interface ListingDetail extends Listing {
  moderation_findings: ModerationFinding[];
  applications: Application[];
  messages: { id: string; sender_account_id: string; body: string; created_at: string }[];
  notice: string;
}

export interface LedgerEntry {
  id: string;
  listing_id: string;
  entry_type: string;
  amount_minor: number;
  currency: string;
  account_id: string;
  simulated: boolean;
  note: string;
  created_at: string;
}

export interface ActivityEntry {
  id: string;
  created_at: string;
  actor_type: 'user' | 'agent' | 'system' | 'relay';
  actor_id: string | null;
  actor_name: string | null;
  action: string;
  target_type: string | null;
  target_id: string | null;
  project_id: string | null;
  task_id: string | null;
  outcome: 'ok' | 'denied' | 'failed' | 'pending';
  detail: Record<string, unknown>;
}

export interface AccountStatus {
  linked: boolean;
  link: {
    id: string;
    relay_base_url: string;
    account_id: string | null;
    account_email: string | null;
    display_name: string | null;
    has_token: boolean;
    scopes: string[];
    linked_at: string | null;
    revoked_at: string | null;
  } | null;
  available_scopes: string[];
  privacy_notice: string;
}

export interface PairingCode {
  code: string;
  scopes: string[];
  expires_at: string;
  expires_in_seconds: number;
  instructions: string;
}
