//! The tools an agent may use.
//!
//! Every tool goes through the same gate: a permission check, then the action,
//! then an activity-log entry — including when the check refused. There is no
//! shell tool, and there is no path by which a model can name a tool that is
//! not in this list.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use otwono_permissions::{path_policy, PermissionEngine, Request};
use otwono_store::repo::activity::{ActivityRepo, NewActivity, Outcome};
use otwono_store::repo::budget::BudgetRepo;
use otwono_store::repo::knowledge::KnowledgeRepo;
use otwono_store::repo::projects::ProjectRepo;
use otwono_store::{paths, Db};
use otwono_types::permission::{Capability, CheckOutcome};

/// The complete tool surface. A model can only ask for one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    KnowledgeSearch,
    FileRead,
    FileWrite,
    HttpFetch,
    ArtifactCreate,
    BudgetRecord,
}

impl Tool {
    pub const ALL: [Tool; 6] = [
        Tool::KnowledgeSearch,
        Tool::FileRead,
        Tool::FileWrite,
        Tool::HttpFetch,
        Tool::ArtifactCreate,
        Tool::BudgetRecord,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::KnowledgeSearch => "knowledge_search",
            Self::FileRead => "file_read",
            Self::FileWrite => "file_write",
            Self::HttpFetch => "http_fetch",
            Self::ArtifactCreate => "artifact_create",
            Self::BudgetRecord => "budget_record",
        }
    }

    pub const fn capability(self) -> Capability {
        match self {
            Self::KnowledgeSearch => Capability::KnowledgeSearch,
            Self::FileRead => Capability::FileRead,
            Self::FileWrite => Capability::FileWrite,
            Self::HttpFetch => Capability::HttpFetch,
            Self::ArtifactCreate => Capability::ArtifactCreate,
            Self::BudgetRecord => Capability::BudgetRecord,
        }
    }

    /// One line describing the tool to a model.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::KnowledgeSearch => {
                "Search the user's authorised files. Returns passages with the file name and \
                 location so you can cite them."
            }
            Self::FileRead => "Read one file the user has authorised.",
            Self::FileWrite => "Write a file into this project's output folder.",
            Self::HttpFetch => "Fetch one page from a website the user has approved.",
            Self::ArtifactCreate => "Save a deliverable (a report, a document) into the project.",
            Self::BudgetRecord => {
                "Record a simulated expense against the project budget. No money moves."
            }
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.as_str() == value)
    }
}

/// What a tool was asked to do.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub enum ToolCall {
    KnowledgeSearch {
        query: String,
        source_ids: Vec<String>,
    },
    FileRead {
        path: String,
    },
    FileWrite {
        name: String,
        contents: String,
    },
    HttpFetch {
        url: String,
    },
    ArtifactCreate {
        name: String,
        media_type: String,
        contents: String,
    },
    BudgetRecord {
        category: String,
        description: String,
        amount_minor: i64,
    },
}

impl ToolCall {
    pub const fn tool(&self) -> Tool {
        match self {
            Self::KnowledgeSearch { .. } => Tool::KnowledgeSearch,
            Self::FileRead { .. } => Tool::FileRead,
            Self::FileWrite { .. } => Tool::FileWrite,
            Self::HttpFetch { .. } => Tool::HttpFetch,
            Self::ArtifactCreate { .. } => Tool::ArtifactCreate,
            Self::BudgetRecord { .. } => Tool::BudgetRecord,
        }
    }

    /// A one-line summary for the run drawer and the activity log.
    pub fn summary(&self) -> String {
        match self {
            Self::KnowledgeSearch { query, .. } => format!("search knowledge for {query:?}"),
            Self::FileRead { path } => format!("read {path}"),
            Self::FileWrite { name, contents } => {
                format!("write {name} ({} bytes)", contents.len())
            }
            Self::HttpFetch { url } => format!("fetch {url}"),
            Self::ArtifactCreate { name, .. } => format!("create the deliverable {name}"),
            Self::BudgetRecord {
                description,
                amount_minor,
                ..
            } => format!(
                "record a simulated expense of {:.2} for {description}",
                *amount_minor as f64 / 100.0
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool: String,
    pub ok: bool,
    /// Text handed back to the model. Untrusted content is already fenced.
    pub output: String,
    pub citations: Vec<otwono_types::chat::Citation>,
    /// Set when the call was refused rather than attempted.
    pub refused_reason: Option<String>,
    /// Set when the call needs a human answer before it can proceed.
    pub approval_request_id: Option<String>,
}

impl ToolResult {
    fn refused(tool: Tool, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            tool: tool.as_str().into(),
            ok: false,
            output: format!("This action was not permitted: {reason}"),
            citations: Vec::new(),
            refused_reason: Some(reason),
            approval_request_id: None,
        }
    }

    fn ok(tool: Tool, output: impl Into<String>) -> Self {
        Self {
            tool: tool.as_str().into(),
            ok: true,
            output: output.into(),
            citations: Vec::new(),
            refused_reason: None,
            approval_request_id: None,
        }
    }
}

/// Everything a tool call needs to know about who is asking.
#[derive(Debug, Clone, Default)]
pub struct ToolContext {
    pub agent_id: Option<String>,
    pub agent_name: Option<String>,
    pub project_id: Option<String>,
    pub task_id: Option<String>,
    pub workspace_id: Option<String>,
    /// Hosts the user approved for this project.
    pub approved_hosts: Vec<String>,
    pub budget_id: Option<String>,
}

/// The largest response body a fetch will read.
pub const MAX_FETCH_BYTES: usize = 512 * 1024;
/// The largest file a `file_write` may produce.
pub const MAX_WRITE_BYTES: usize = 2 * 1024 * 1024;

pub struct ToolRunner<'a> {
    db: &'a Db,
    embedder: &'a otwono_knowledge::Embedder,
}

impl<'a> ToolRunner<'a> {
    pub fn new(db: &'a Db, embedder: &'a otwono_knowledge::Embedder) -> Self {
        Self { db, embedder }
    }

    fn log(
        &self,
        context: &ToolContext,
        call: &ToolCall,
        outcome: Outcome,
        detail: serde_json::Value,
    ) {
        let mut entry = match (&context.agent_id, &context.agent_name) {
            (Some(id), Some(name)) => {
                NewActivity::agent(id, name, format!("tool.{}", call.tool().as_str()))
            }
            _ => NewActivity::system(format!("tool.{}", call.tool().as_str())),
        }
        .with_outcome(outcome)
        .with_detail(detail);
        if let Some(project) = &context.project_id {
            entry = entry.with_project(project);
        }
        if let Some(task) = &context.task_id {
            entry = entry.with_task(task);
        }
        if let Err(error) = ActivityRepo::new(self.db).record(entry) {
            tracing::error!(%error, "could not write to the activity log");
        }
    }

    /// Build the permission request a call implies.
    fn permission_request(&self, call: &ToolCall, context: &ToolContext) -> Request {
        let mut request = Request::new(call.tool().capability());
        if let Some(project) = &context.project_id {
            request = request.in_project(project);
        }
        if let Some(workspace) = &context.workspace_id {
            request = request.in_workspace(workspace);
        }
        if let Some(agent) = &context.agent_id {
            request = request.by_agent(agent);
        }
        match call {
            ToolCall::FileRead { path } => request.on_path(path),
            ToolCall::HttpFetch { url } => match url::Url::parse(url) {
                Ok(parsed) => request.on_host(parsed.host_str().unwrap_or_default()),
                Err(_) => request,
            },
            _ => request,
        }
    }

    /// Run a tool call: check, act, log.
    pub async fn run(&self, call: ToolCall, context: &ToolContext) -> Result<ToolResult> {
        let tool = call.tool();
        let engine = PermissionEngine::new(self.db);
        let request = self.permission_request(&call, context);

        match engine.check_and_consume(&request)? {
            CheckOutcome::Allowed { .. } => {}
            CheckOutcome::Stopped => {
                let result = ToolResult::refused(
                    tool,
                    "the emergency stop is engaged; nothing runs until you release it",
                );
                self.log(
                    context,
                    &call,
                    Outcome::Denied,
                    serde_json::json!({ "reason": "emergency_stop" }),
                );
                return Ok(result);
            }
            CheckOutcome::Denied { reason } | CheckOutcome::NeedsApproval { reason } => {
                let result = ToolResult::refused(tool, reason.clone());
                self.log(
                    context,
                    &call,
                    Outcome::Denied,
                    serde_json::json!({ "reason": reason }),
                );
                return Ok(result);
            }
        }

        let result = match &call {
            ToolCall::KnowledgeSearch { query, source_ids } => {
                self.knowledge_search(query, source_ids).await
            }
            ToolCall::FileRead { path } => self.file_read(path),
            ToolCall::FileWrite { name, contents } => self.file_write(context, name, contents),
            ToolCall::HttpFetch { url } => self.http_fetch(context, url).await,
            ToolCall::ArtifactCreate {
                name,
                media_type,
                contents,
            } => self.artifact_create(context, name, media_type, contents),
            ToolCall::BudgetRecord {
                category,
                description,
                amount_minor,
            } => self.budget_record(context, category, description, *amount_minor),
        };

        match result {
            Ok(result) => {
                self.log(
                    context,
                    &call,
                    if result.ok {
                        Outcome::Ok
                    } else {
                        Outcome::Failed
                    },
                    serde_json::json!({
                        "summary": call.summary(),
                        "output_bytes": result.output.len(),
                        "citations": result.citations.len(),
                    }),
                );
                Ok(result)
            }
            Err(error) => {
                let message = error.to_string();
                self.log(
                    context,
                    &call,
                    Outcome::Failed,
                    serde_json::json!({ "summary": call.summary(), "error": message }),
                );
                Ok(ToolResult {
                    tool: tool.as_str().into(),
                    ok: false,
                    output: format!("This action failed: {message}"),
                    citations: Vec::new(),
                    refused_reason: None,
                    approval_request_id: None,
                })
            }
        }
    }

    async fn knowledge_search(&self, query: &str, source_ids: &[String]) -> Result<ToolResult> {
        let retriever = otwono_knowledge::Retriever::new(self.db, self.embedder);
        let hits = retriever.search(query, source_ids).await?;
        if hits.is_empty() {
            return Ok(ToolResult::ok(
                Tool::KnowledgeSearch,
                "No passage in the authorised sources matched that query. Say so rather than \
                 answering from memory.",
            ));
        }

        let citations = otwono_knowledge::Retriever::to_citations(&hits);
        let pieces: Vec<(String, String)> = hits
            .iter()
            .map(|hit| {
                let label = match &hit.chunk.locator {
                    Some(locator) => format!("{} ({locator})", hit.file_name),
                    None => hit.file_name.clone(),
                };
                (label, hit.chunk.text.clone())
            })
            .collect();
        let wrapped = otwono_knowledge::injection::wrap_all(&pieces);

        Ok(ToolResult {
            tool: Tool::KnowledgeSearch.as_str().into(),
            ok: true,
            output: wrapped.text,
            citations,
            refused_reason: None,
            approval_request_id: None,
        })
    }

    fn file_read(&self, path: &str) -> Result<ToolResult> {
        // The permission check already established that a grant covers this
        // path. Confirm containment against an authorised source too, so a
        // stale grant cannot outlive the source it was written for.
        let sources = KnowledgeRepo::new(self.db).list_sources(true)?;
        let permitted = sources
            .iter()
            .any(|source| path_policy::is_prefix_of(&source.root_path, path));
        if !permitted {
            bail!("{path} is not inside a folder you have authorised");
        }

        let candidate = PathBuf::from(path);
        let format = candidate
            .extension()
            .and_then(|e| e.to_str())
            .and_then(otwono_types::knowledge::DocumentFormat::from_extension)
            .ok_or_else(|| anyhow::anyhow!("OTWONO cannot read {path}: unsupported file type"))?;

        let segments = otwono_knowledge::parse::parse(&candidate, format)?;
        let body: String = segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let file_name = candidate
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string());
        let wrapped = otwono_knowledge::injection::wrap(&file_name, &body);

        Ok(ToolResult::ok(Tool::FileRead, wrapped.text))
    }

    fn file_write(&self, context: &ToolContext, name: &str, contents: &str) -> Result<ToolResult> {
        let project_id = context
            .project_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("files can only be written inside a project"))?;
        if contents.len() > MAX_WRITE_BYTES {
            bail!("that file is larger than the {MAX_WRITE_BYTES} byte limit");
        }
        let target = safe_output_path(project_id, name)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, contents)?;
        Ok(ToolResult::ok(
            Tool::FileWrite,
            format!("Wrote {} ({} bytes).", target.display(), contents.len()),
        ))
    }

    async fn http_fetch(&self, context: &ToolContext, url: &str) -> Result<ToolResult> {
        let parsed = url::Url::parse(url)?;
        if parsed.scheme() != "https" && parsed.scheme() != "http" {
            bail!("only http and https addresses can be fetched");
        }
        let host = parsed
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("that address has no host"))?
            .to_string();
        if !context
            .approved_hosts
            .iter()
            .any(|approved| approved == &host)
        {
            bail!("{host} is not on this project's approved list");
        }
        if is_private_host(&host) {
            bail!(
                "{host} is a private or loopback address; OTWONO will not fetch from inside your \
                 own network"
            );
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            // A redirect could land on a host that was never approved.
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let response = client.get(parsed.clone()).send().await?;
        let status = response.status();
        if !status.is_success() {
            bail!("{url} answered with {status}");
        }
        let body = response.text().await?;
        let truncated: String = body.chars().take(MAX_FETCH_BYTES).collect();
        let wrapped = otwono_knowledge::injection::wrap(url, &truncated);
        Ok(ToolResult::ok(Tool::HttpFetch, wrapped.text))
    }

    fn artifact_create(
        &self,
        context: &ToolContext,
        name: &str,
        media_type: &str,
        contents: &str,
    ) -> Result<ToolResult> {
        let project_id = context
            .project_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("deliverables belong to a project"))?;
        if contents.len() > MAX_WRITE_BYTES {
            bail!("that deliverable is larger than the {MAX_WRITE_BYTES} byte limit");
        }
        let target = safe_output_path(project_id, name)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, contents)?;
        ProjectRepo::new(self.db).add_artifact(
            project_id,
            context.task_id.as_deref(),
            name,
            media_type,
            &target.to_string_lossy(),
            contents.len() as u64,
        )?;
        Ok(ToolResult::ok(
            Tool::ArtifactCreate,
            format!("Saved the deliverable {name} ({} bytes).", contents.len()),
        ))
    }

    fn budget_record(
        &self,
        context: &ToolContext,
        category: &str,
        description: &str,
        amount_minor: i64,
    ) -> Result<ToolResult> {
        let budget_id = context
            .budget_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("this project has no budget to record against"))?;
        let expense = BudgetRepo::new(self.db).record_expense(
            budget_id,
            context.task_id.as_deref(),
            category,
            description,
            amount_minor,
        )?;
        let summary = BudgetRepo::new(self.db).summary(budget_id)?;
        Ok(ToolResult::ok(
            Tool::BudgetRecord,
            format!(
                "Recorded a SIMULATED expense of {:.2} {} as {}. No money moved. \
                 Remaining simulated budget: {:.2} {}.",
                amount_minor as f64 / 100.0,
                summary.currency,
                expense.state.as_str(),
                summary.remaining_minor as f64 / 100.0,
                summary.currency
            ),
        ))
    }
}

/// Resolve a model-supplied file name inside the project's output folder,
/// refusing anything that would escape it.
pub fn safe_output_path(project_id: &str, name: &str) -> Result<PathBuf> {
    if name.trim().is_empty() {
        bail!("a file needs a name");
    }
    if name.len() > 200 {
        bail!("that file name is too long");
    }
    let root = paths::project_artifacts_dir(project_id)?;
    let candidate = root.join(name);

    // Compare lexically: the file does not exist yet, so canonicalising it
    // would fail. `path_policy` resolves `..` without touching the disk.
    let root_text = root.to_string_lossy().to_string();
    let candidate_text = candidate.to_string_lossy().to_string();
    if path_policy::escapes(&root_text, &candidate_text) {
        bail!("{name} would write outside this project's folder");
    }
    if Path::new(name).is_absolute() {
        bail!("{name} must be a name inside the project folder, not an absolute path");
    }
    Ok(path_policy::normalise(&candidate_text))
}

/// Loopback, link-local and private ranges the fetch tool refuses.
pub fn is_private_host(host: &str) -> bool {
    let lowered = host.to_ascii_lowercase();
    if lowered == "localhost" || lowered.ends_with(".localhost") || lowered.ends_with(".local") {
        return true;
    }
    if let Ok(address) = lowered.parse::<std::net::IpAddr>() {
        return match address {
            std::net::IpAddr::V4(v4) => {
                v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
            }
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    // Unique-local (fc00::/7) and link-local (fe80::/10).
                    || (v6.segments()[0] & 0xfe00) == 0xfc00
                    || (v6.segments()[0] & 0xffc0) == 0xfe80
            }
        };
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn there_is_no_shell_tool() {
        for tool in Tool::ALL {
            let name = tool.as_str();
            assert!(
                !name.contains("shell") && !name.contains("exec") && !name.contains("command"),
                "{name} looks like arbitrary execution"
            );
        }
        assert!(Tool::parse("run_shell").is_none());
        assert!(Tool::parse("exec").is_none());
        assert_eq!(Tool::parse("knowledge_search"), Some(Tool::KnowledgeSearch));
    }

    #[test]
    fn every_tool_maps_to_a_capability_and_describes_itself() {
        for tool in Tool::ALL {
            assert_eq!(tool.capability().as_str(), tool.as_str());
            assert!(tool.describe().ends_with('.'), "{}", tool.as_str());
        }
    }

    #[test]
    fn a_model_supplied_name_cannot_escape_the_project_folder() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var(paths::DATA_DIR_ENV, tmp.path());

        assert!(safe_output_path("prj_abc", "report.md").is_ok());
        assert!(safe_output_path("prj_abc", "reports/summary.md").is_ok());

        for hostile in [
            "../escape.md",
            "../../etc/passwd",
            "sub/../../escape.md",
            "/etc/passwd",
            "",
            "   ",
        ] {
            assert!(
                safe_output_path("prj_abc", hostile).is_err(),
                "{hostile:?} should have been refused"
            );
        }
        std::env::remove_var(paths::DATA_DIR_ENV);
    }

    #[test]
    fn fetching_from_inside_the_users_own_network_is_refused() {
        for host in [
            "localhost",
            "printer.local",
            "127.0.0.1",
            "0.0.0.0",
            "10.0.0.5",
            "192.168.1.1",
            "172.16.4.4",
            "169.254.169.254",
            "::1",
            "fd00::1",
            "fe80::1",
        ] {
            assert!(is_private_host(host), "{host} should be refused");
        }
        for host in [
            "example.com",
            "docs.rs",
            "93.184.216.34",
            "2606:2800:220:1::1",
        ] {
            assert!(!is_private_host(host), "{host} should be allowed");
        }
    }

    #[test]
    fn tool_call_summaries_read_as_english_and_say_simulated_for_money() {
        let search = ToolCall::KnowledgeSearch {
            query: "annual leave".into(),
            source_ids: vec!["src_1".into()],
        };
        assert!(search.summary().contains("annual leave"));

        let expense = ToolCall::BudgetRecord {
            category: "software".into(),
            description: "a licence".into(),
            amount_minor: 4_250,
        };
        assert!(
            expense.summary().contains("simulated"),
            "{}",
            expense.summary()
        );
        assert!(expense.summary().contains("42.50"));
    }

    #[test]
    fn tool_calls_serialise_with_a_tag_the_service_can_dispatch_on() {
        let call = ToolCall::FileRead {
            path: "/home/u/a.md".into(),
        };
        let json = serde_json::to_value(&call).unwrap();
        assert_eq!(json["tool"], "file_read");
        assert_eq!(json["path"], "/home/u/a.md");

        let round_trip: ToolCall = serde_json::from_value(json).unwrap();
        assert_eq!(round_trip.tool(), Tool::FileRead);
    }
}
