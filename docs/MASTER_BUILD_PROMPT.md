# OTWONO AI — Claude Max / Claude Code Master Build Prompt

Copy everything in this document into a new Claude Max conversation or Claude Code project. Give Claude access to a clean project directory and permission to create files, install normal development dependencies, run tests, and build local installers. Keep this document in the repository as the controlling product specification.

---

## BEGIN PROMPT FOR CLAUDE MAX / CLAUDE CODE

You are the principal software architect, product engineer, security engineer, UI/UX designer, QA engineer, release engineer, and technical writer responsible for building a working MVP named **OTWONO AI**.

OTWONO means **On The Work Of No One**.

Do not produce only a design, plan, mockup, static prototype, or partial code sample. Build, run, test, debug, package, and document a functional installable MVP. Work autonomously inside the provided project directory, but stop for confirmation before destructive actions, purchases, production deployments, domain/DNS changes, credential use outside the project, or any action that could create legal or financial obligations.

If an existing repository is provided, inspect it first, preserve working functionality, and improve it incrementally. Never delete or overwrite user work merely to simplify the build. If requirements conflict, prioritize security and data integrity, record the conflict in `DECISIONS.md`, and implement the safest reversible interpretation.

# 1. Product Vision

OTWONO AI is a local-first, user-controlled AI work platform specialized in creating, managing, coordinating, and upgrading AI agents. A user describes an outcome. OTWONO AI plans the work, divides it into tasks, assigns the best available agent or approved human worker, supervises execution, verifies results, and returns a completed deliverable with an auditable history.

The public-facing promise is:

> An AI that works for you.

The long-term platform may include AI identities, `@otwono.com` email accounts, controlled purchasing, a human task marketplace, physical-world work, connected vision, robotics, and vehicle integrations. These are roadmap directions, not permission to implement unsafe autonomy or misleading production claims in the MVP.

# 2. Required MVP Deliverables

Produce all of the following:

1. A Windows-first desktop application that installs with a normal guided installer, launches from the Start menu and desktop shortcut, can optionally launch at user sign-in, and uninstalls cleanly.
2. A responsive local web UI embedded in the desktop app.
3. A local backend and database that work without an OTWONO cloud account.
4. Local AI connectivity supporting both Ollama and LM Studio through their local APIs.
5. Extensible provider adapters for future OpenAI, Anthropic, and other OpenAI-compatible APIs. Online providers must be disabled until the user supplies credentials.
6. Agent creation, editing, deletion, testing, export, and import.
7. Offices, Labs, Boardrooms, and Think Tanks as functional workspaces—not decorative menu items.
8. Persistent chat, projects, tasks, files, permissions, agent runs, and audit history.
9. A local knowledge system that lets the user choose folders and files for AI retrieval without uploading them by default.
10. A customizable interface with themes, layout preferences, module visibility, density, font sizing, sidebar behavior, and saved user preferences.
11. A WordPress plugin delivered as an installable ZIP that connects an OTWONO.com website section to the desktop application's account and project system through a secure API.
12. Automated tests, build scripts, release scripts, seed/demo data, user documentation, administrator documentation, and a concise troubleshooting guide.
13. A final release folder containing the desktop installer, WordPress plugin ZIP, checksums, release notes, and exact installation instructions.

For this specification, **bootable application** means an installable desktop application that reliably starts, can optionally start when Windows starts, and can be continuously upgraded. Do not build a bootable operating-system ISO for this MVP.

# 3. Recommended Technical Architecture

Use this architecture unless the existing repository or verified platform constraints justify a better choice. Document any deviation before implementing it.

## Desktop shell and frontend

- Tauri 2 desktop shell
- React with TypeScript
- Vite
- A maintainable component system with accessible primitives
- CSS variables/design tokens for user customization
- TanStack Query for server state where useful
- A small predictable client store for view preferences

## Local application service

Prefer a Rust service within Tauri when practical. If Python substantially accelerates reliable AI/RAG implementation, use a bundled Python sidecar with a pinned runtime and clean lifecycle management. Do not require the user to install Python, Node.js, Rust, or developer tools.

The local service must expose only loopback interfaces by default. Use authenticated IPC or authenticated localhost requests. Randomize or safely allocate local ports and protect against cross-site request forgery, origin abuse, and unauthorized local calls.

## Persistence

- SQLite for application state
- Versioned database migrations
- Transactional writes
- Automatic local backup before migrations
- Export/import for user-controlled portability
- Never store secrets in plaintext
- Use Windows Credential Manager or an equivalent OS credential vault for provider keys and tokens

## Local knowledge and retrieval

- User-selected source folders only
- Explicit permission and revocation controls
- File metadata, chunking, indexing status, and source citations
- Local embeddings when available
- Pluggable vector index; select the smallest reliable local implementation
- Incremental re-indexing and deletion propagation
- Supported MVP formats: TXT, Markdown, PDF, DOCX, CSV, and common source-code files
- Never claim knowledge was indexed until parsing and indexing succeeded
- Answers using local knowledge must cite the source filename and relevant location/page when available

## WordPress plugin

- Modern namespaced PHP compatible with current supported WordPress/PHP versions
- WordPress REST API integration
- WordPress coding and security practices
- Nonces, capability checks, sanitization, escaping, prepared queries, rate limiting, and secure token storage
- No modification of WordPress core files
- Uninstall behavior must preserve user data by default and offer an explicit deletion setting

# 4. Core User Experience

## Default application layout

The default home screen is a chatbot workspace.

### Top navigation tabs

Include functional tabs for:

- Chat
- Projects
- Agents
- Tasks
- Knowledge
- Connections
- Marketplace
- Activity
- Settings

Tabs may adapt at smaller screen widths, but all sections must remain reachable by keyboard and touch.

### Collapsible left sidebar

The sidebar must separate and organize:

- Chats
- Offices
- Labs
- Boardrooms
- Think Tanks
- Saved projects
- Favorites
- Archived items

It must support collapse/expand, search, sorting, drag-and-drop reordering where safe, context menus, rename, archive, and creation of new items. Its state must persist between launches.

### Main chat area

Include:

- Streaming responses
- Markdown and code rendering
- File attachment
- Stop generation
- Retry and edit-and-resend
- Copy/export conversation
- Model/provider selector
- Active agent/team selector
- Knowledge-source selector
- Context and permission indicators
- Clear notices when an action requires user approval
- A task/run drawer showing plan, tool actions, progress, results, failures, and retries

### Optional right inspector

Provide a collapsible inspector for project metadata, selected agents, attached files, permissions, budget simulation, citations, and run history.

## Customization

The user must be able to customize:

- Light, dark, high-contrast, and system themes
- Accent colors and background appearance
- Font family from a safe included set
- Font size and interface density
- Sidebar position, width, and default collapsed state
- Visible tabs/modules
- Chat width and message presentation
- Reduced-motion behavior
- Dashboard widgets
- Saved layouts or profiles

Include **Reset to Default** and make customizations portable through export/import. Do not allow custom CSS or arbitrary scripts in the MVP.

# 5. Functional Workspace Definitions

## Chat

A conversation with one selected model or agent. It has memory only according to the user's configured scope.

## Office

A persistent group of agents with named job roles, shared project instructions, selected knowledge sources, and an executive/coordinator agent. Offices are for repeated operational work.

## Lab

An experimentation workspace where prompts, models, tools, and agent configurations can be tested without changing production configurations. A lab can compare outputs and promote a tested configuration to an Office.

## Boardroom

A structured multi-agent decision session. Several agents provide position statements, challenge assumptions, and evaluate alternatives. One designated chair produces the final synthesis, dissent summary, unresolved questions, and recommended decision. Text-based sessions are required for the MVP; architecture must permit future real-time voice meetings.

## Think Tank

A research and ideation workspace that supports divergent proposals, critique rounds, synthesis, source collection, and a final research brief. It must distinguish sourced claims from speculation.

Every workspace must be creatable, editable, duplicable, archivable, exportable, and connected to projects and activity history.

# 6. Agent System

Each agent must support:

- Name, avatar/icon, description, and role
- System instructions
- Provider and model selection
- Temperature and supported model parameters
- Selected tools/capabilities
- Allowed knowledge sources
- Memory scope
- Permission policy
- Maximum step count and timeout
- Human-approval rules
- Optional parent Office, Lab, Boardroom, or Think Tank
- Version history
- Test console
- Export/import as a safe declarative JSON package

Provide initial templates for:

- Executive Orchestrator
- Planner
- Researcher
- Software Engineer
- Writer
- Designer
- Budget Reviewer
- Security Reviewer
- Verification Agent
- Human Task Coordinator

Agent packages must not contain API keys, passwords, session cookies, or other secrets.

# 7. Orchestration and Task Execution

Implement a bounded, inspectable orchestration engine. Do not create an uncontrolled self-replicating or indefinitely running agent loop.

The Executive Orchestrator must be able to:

1. Convert a user objective into a structured project.
2. Ask for missing acceptance criteria only when necessary.
3. Create a dependency-aware task plan.
4. Recommend an agent for each task based on declared capabilities.
5. Execute tasks sequentially or in parallel when dependencies permit.
6. Pause for permission-gated actions.
7. Record inputs, actions, outputs, errors, retries, and costs/estimated costs.
8. Send results to a Verification Agent.
9. Rework failed items within configured limits.
10. Produce a final deliverable and completion report.

Represent projects and tasks with explicit state machines. At minimum:

- Project: draft, planned, awaiting_approval, running, blocked, verifying, completed, failed, cancelled, archived
- Task: queued, ready, running, awaiting_approval, blocked, verifying, completed, failed, cancelled

Prevent invalid transitions and cover the transition rules with tests. Interrupted work must recover cleanly after an application restart.

For the MVP, tools should be limited to explicitly enabled safe local capabilities such as selected-file read/write, knowledge search, controlled HTTP requests to approved domains, and creation of project artifacts. Show every tool call in the activity log. Never execute arbitrary model-generated shell commands without a visible approval step and a strict sandbox.

# 8. Local AI Connections

Create a guided setup wizard that:

- Detects running Ollama and LM Studio services
- Tests the connection
- Lists installed/served models
- Runs a short capability test
- Lets the user select a default model
- Explains hardware limitations without blocking setup
- Stores endpoints and preferences safely

Support OpenAI-compatible local endpoints so additional local runtimes can be added later. The application must remain usable for organization, settings, agent design, and knowledge management when no model is connected.

Build a provider adapter interface with capability discovery for chat, streaming, tool calling, JSON/structured output, vision, embeddings, and context length. Do not assume every model supports every capability. Disable or adapt unavailable functions visibly.

# 9. Knowledge, Memory, and Continuous Upgrades

Separate these concepts:

- Conversation history
- User preferences
- Project memory
- Agent configuration/version history
- Indexed knowledge sources
- Model files managed by external runtimes

The user must be able to inspect, edit where appropriate, export, delete, and disable each memory category.

Make upgrades sustainable through:

- Semantic application versioning
- Database migrations with rollback-safe backups
- Versioned provider adapters
- Versioned agent schemas
- Feature flags
- Import/export compatibility checks
- A signed-update-ready architecture
- Manual update installation for the MVP
- Clear separation between core code and user data

Do not silently download or replace AI models. Model management must remain explicit and should integrate with the local runtime rather than duplicating its storage.

# 10. Accounts, Profiles, and Identity

The local application must work without an online account.

Add optional OTWONO account connection with:

- Register, sign in, sign out, email verification, password reset, and device-session management
- A customizable personal profile with public/private visibility controls
- Display name, biography, interests, capabilities, portfolio links, avatar, and selected agents/projects
- A clearly labeled AI identity associated with its human or organization owner

Prepare for future `@otwono.com` AI email accounts, but implement the MVP as a safe email alias/profile field or simulated development adapter unless real mail infrastructure and administrator credentials are supplied. Never represent an AI as a human. Never send external email without a preview and explicit approval in the MVP.

# 11. Permissions, Security, and Auditability

Use least privilege and deny-by-default policies.

Permissions must be scoped by:

- Capability
- Account or connector
- Project/workspace
- Folder or file
- Time window
- Spending category and limit when applicable

Required controls:

- Human-readable permission requests
- Allow once, allow for project, deny, and revoke
- Global emergency stop
- Session/device revocation
- Secure secret storage
- Audit log with timestamps and actor identity
- Sensitive-field redaction in logs
- Exportable activity report
- Automatic local database backup
- Protection against prompt injection from retrieved files and web content
- Clear separation of untrusted content from system/developer instructions

Threat-model the desktop app, local APIs, WordPress bridge, update process, plugin authentication, file ingestion, agent imports, and marketplace inputs. Put the threat model in `docs/THREAT_MODEL.md` and resolve all critical/high findings required for MVP release.

# 12. Budget and Purchasing Boundary

Build a **budget simulator and approval ledger**, not autonomous banking.

The MVP may:

- Let a user create a project budget
- Record estimated and approved expenses
- Require approval above configurable limits
- Attach receipts manually
- Track remaining budget
- Simulate a purchase request through a test adapter

The MVP must not:

- Hold customer funds
- Issue currency or coins
- Store bank passwords
- Claim to be a bank
- Make real purchases automatically
- Release payments to human workers

Create interfaces for future licensed payment providers, virtual cards, escrow-like marketplace payments, and payouts, but use test mode or mock adapters until separately authorized and legally reviewed.

# 13. Human Task Marketplace MVP

Implement the marketplace as an invitation-only development MVP with two paths:

## Creator path

- Create a task request from a project task
- Specify remote/local, category, description, deliverables, acceptance criteria, deadline, compensation estimate, expenses, evidence required, and safety classification
- Review and approve the listing before publishing
- View applications and select a worker
- Message through the platform
- Accept, request revision, dispute, or cancel

## Worker path

- Create a worker profile
- List skills, equipment, availability, location radius, and portfolio
- Browse eligible tasks
- Apply with a proposal
- Accept an assignment
- Ask clarifying questions
- Submit deliverables and evidence
- View simulated earnings and status

Use simulated/test payments only. Include reporting, moderation, blocked categories, rate limits, abuse controls, and human escalation. Prohibit illegal, unsafe, deceptive, exploitative, surveillance, credential-harvesting, harassment, weapons, and privacy-invasive tasks.

# 14. WordPress Plugin Requirements

Create an installable plugin named **OTWONO AI Connector**.

The plugin must provide:

- Setup wizard
- Connection status and diagnostics
- Secure configuration page for the OTWONO API endpoint and credentials
- Account registration, login, logout, email verification placeholder/adapter, and password-reset flow
- Profile dashboard and editable public/private profile fields
- Creator and Worker path selection
- Marketplace task browsing and task-detail pages
- Creator task submission and management
- Project/agent summaries for connected users
- Shortcodes and/or blocks for login, profile, dashboard, marketplace, and connection status
- REST endpoints required by the desktop/web system
- Role/capability mapping that does not grant WordPress administrator access
- Audit-friendly logging without secrets
- Rate limiting and abuse controls
- Clean activation, migration, deactivation, and uninstall routines

Use a secure pairing flow. For local development, support a one-time pairing code displayed in the desktop app and entered into WordPress. Exchange it for revocable scoped tokens. In production architecture, prepare for OAuth 2.1 with PKCE or an equivalent reviewed flow. Do not expose a user's localhost service directly to the public internet.

Because a hosted WordPress site cannot reliably call a user's private desktop over the internet, implement a clean transport abstraction:

1. **Local development mode:** desktop and WordPress development environment communicate on the same machine/network through documented URLs.
2. **MVP hosted mode:** a minimal OTWONO relay/account API synchronizes only approved account, profile, task, and project metadata. AI prompts, private files, local knowledge, and model data remain local unless the user explicitly selects them for synchronization.
3. **Future mode:** device-to-device encrypted connectivity can be added later.

If no hosted backend credentials are supplied, ship a runnable local development backend and Docker Compose configuration. Do not pretend public remote synchronization works when it has not been deployed and tested.

Deliver the plugin source and `otwono-ai-connector.zip`. Include precise installation instructions through WordPress Admin → Plugins → Add New → Upload Plugin.

# 15. Visual Design

The default design should feel futuristic, intelligent, technical, and distinctly OTWONO without looking like a generic AI-generated cyberpunk template.

Use:

- Primarily dark neutral surfaces
- Restrained high-contrast accent colors controlled by theme tokens
- Strong information hierarchy
- Crisp typography
- Subtle depth and motion
- Clear operational states
- Responsive layouts suitable for laptop, desktop, tablet, and narrow mobile web views

Avoid:

- Excessive neon glow
- Constant animated backgrounds
- Tiny low-contrast text
- Decorative complexity that hides controls
- Fake terminal text
- Placeholder buttons
- Inaccessible hover-only interactions

Meet WCAG 2.2 AA for essential flows. Support keyboard navigation, visible focus states, screen-reader labels, reduced motion, and touch-friendly targets.

# 16. Privacy and Data Ownership

Local-first is a product requirement, not marketing copy.

- Default to local storage and local inference
- Explain exactly what leaves the device before enabling online providers or synchronization
- Provide per-project synchronization controls
- Make export and deletion straightforward
- Do not train on user data
- Do not add analytics or telemetry without a separate opt-in
- If crash reporting is offered, show the report before submission and remove sensitive content
- Keep private knowledge files out of WordPress and the relay API by default

# 17. Repository Structure

Use a monorepo with clear boundaries similar to:

```text
otwono-ai/
  apps/
    desktop/
    web/
    local-service/
    relay-api/
  packages/
    ui/
    shared-types/
    agent-core/
    provider-adapters/
    permissions/
    knowledge/
  wordpress/
    otwono-ai-connector/
  infrastructure/
    docker/
  docs/
  scripts/
  installers/
  releases/
```

Adapt this where Tauri/Rust workspace conventions require it. Avoid duplicating business logic across desktop, web, relay, and plugin components.

# 18. Development Method

Work in verifiable phases. Do not wait for repeated permission to continue through ordinary reversible development steps.

## Phase 0: Discovery and architecture

- Inspect the environment and any existing repository
- Record system requirements and development dependencies
- Create `PRODUCT_SPEC.md`, `ARCHITECTURE.md`, `DATA_MODEL.md`, `API_SPEC.md`, `SECURITY.md`, `DECISIONS.md`, and a milestone checklist
- Create low-fidelity UI structure directly in code
- Identify anything requiring owner credentials or infrastructure

## Phase 1: Vertical foundation

- Scaffold monorepo
- Launch desktop shell
- Create migrations and local database
- Implement navigation, sidebar, settings, persistence, logging, and error boundaries
- Add Ollama and LM Studio connection tests
- Complete one persistent streaming chat flow

## Phase 2: Agents and knowledge

- Agent CRUD and templates
- Provider capability detection
- File/folder authorization
- Parsing, indexing, retrieval, citations, and deletion
- Agent test console

## Phase 3: Projects and orchestration

- Projects, task state machines, execution queue, approval gates, restart recovery, verification, activity log, and exports
- Functional Offices, Labs, Boardrooms, and Think Tanks

## Phase 4: Accounts and WordPress

- Local account/relay development service
- Secure pairing
- Profiles and visibility controls
- WordPress plugin, blocks/shortcodes, REST integration, diagnostics, and ZIP packaging

## Phase 5: Marketplace development MVP

- Creator/Worker paths
- Listings, applications, assignment, messages, submissions, review, disputes, moderation, and simulated financial ledger

## Phase 6: Hardening and release

- Threat-model fixes
- Accessibility verification
- Migration and backup tests
- Offline tests
- Installer and clean-machine smoke test
- WordPress installation test
- Release artifacts and documentation

At the end of each phase:

1. Run formatting, linting, type checks, unit tests, integration tests, and relevant end-to-end tests.
2. Launch the application and verify the newly implemented flow.
3. Fix failures rather than documenting them as finished.
4. Update `STATUS.md` with completed work, evidence, open issues, decisions, and the next exact actions.
5. Commit a coherent checkpoint if Git is available and the repository is under version control. Never rewrite unrelated history.

# 19. Testing Requirements

Include tests for at least:

- Database migrations and recovery
- Provider detection and failure handling
- Streaming chat cancellation and restart
- Agent import/export and secret exclusion
- Permission allow/deny/revoke behavior
- Project/task state transitions
- Orchestrator step and retry limits
- Prompt-injection boundaries
- Knowledge ingestion, citation, update, and deletion
- Workspace CRUD and persistence
- Budget approval rules
- Marketplace authorization and moderation rules
- Pairing-token creation, expiry, scope, rotation, and revocation
- WordPress nonce, capability, sanitization, escaping, and REST permissions
- Offline startup and operation
- Settings export/import
- Installer upgrade preserving user data
- Uninstall behavior

Add end-to-end tests for these critical paths:

1. First launch → connect Ollama or LM Studio → select model → complete persistent chat.
2. Create agent → attach a test knowledge file → ask a question → receive a cited answer.
3. Create Office → assign agents → create project → approve plan → execute → verify → export report.
4. Create Boardroom → collect agent positions → produce synthesis and dissent summary.
5. Pair WordPress development site → sign in → edit profile → view synchronized approved metadata.
6. Create marketplace task → worker applies → creator assigns → worker submits → creator accepts → simulated ledger updates.
7. Upgrade application over an existing data directory without losing settings, projects, chats, agents, or knowledge metadata.

# 20. Definition of Done

The MVP is not complete until:

- A non-developer can install and launch the Windows application
- The app functions after a restart
- Local chat works with at least one actually tested local runtime
- All visible primary buttons and navigation items perform real actions or are clearly labeled as unavailable roadmap items
- Agents and all five workspace types persist and work as defined
- The application can complete the required project/orchestration flow
- Knowledge retrieval returns traceable citations
- Permissions and emergency stop work
- The WordPress plugin installs from the ZIP without manual source editing
- The plugin securely connects in the documented development environment
- Creator and Worker marketplace flows work with simulated payments
- Automated critical-path tests pass
- A release installer and plugin ZIP exist
- Installation, usage, backup, update, and troubleshooting documentation are accurate
- No default credentials, secrets, unexplained mock success messages, or critical/high unresolved security defects remain

# 21. Required Final Handoff

When the build is complete, provide:

- Exact summary of implemented functionality
- Exact list of deferred roadmap functionality
- Desktop installer path and SHA-256 checksum
- WordPress plugin ZIP path and SHA-256 checksum
- Source repository structure
- Build and test commands
- Test results
- Known limitations
- Clean installation instructions
- Upgrade instructions
- WordPress setup instructions
- Local AI connection instructions for Ollama and LM Studio
- Backup/restore instructions
- Security and privacy notes
- Credentials, domain settings, or production services still needed from the owner

Also create a `DEMO_SCRIPT.md` that walks through a ten-minute demonstration of the complete MVP.

# 22. Execution Instructions

Begin now.

First inspect the working environment and repository. Then present a concise architecture decision and phased implementation plan. After that, immediately begin building Phase 1 unless a genuinely blocking choice or missing credential prevents local development.

Use sensible defaults when choices are reversible. Ask the owner only about decisions that materially affect scope, security, cost, production infrastructure, or brand identity. Group questions rather than interrupting repeatedly.

Do not state that the application is finished merely because code was generated. Completion requires builds, tests, launched flows, packaged artifacts, and the Definition of Done above.

## END PROMPT FOR CLAUDE MAX / CLAUDE CODE
