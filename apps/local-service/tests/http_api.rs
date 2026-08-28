//! The service over real HTTP: authentication, origin checks, and the
//! streaming chat flow against a server speaking the real Ollama protocol.

// The fake runtime is shared with the provider adapters' tests rather than
// duplicated, so both suites exercise the same wire behaviour.
#[path = "../../../packages/provider-adapters/tests/support/mod.rs"]
mod fake_runtime;

use fake_runtime::{Behaviour, FakeRuntime};
use otwono_local_service::{start_for_tests, AppState, RunningService};
use serde_json::json;

struct Harness {
    service: RunningService,
    state: AppState,
    client: reqwest::Client,
}

impl Harness {
    async fn start() -> Self {
        let (service, state) = start_for_tests().await.unwrap();
        Self {
            service,
            state,
            client: reqwest::Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.service.base_url())
    }

    fn authorised(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder
            .bearer_auth(&self.service.token)
            .header("Origin", "tauri://localhost")
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.authorised(self.client.get(self.url(path)))
            .send()
            .await
            .unwrap()
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> reqwest::Response {
        self.authorised(self.client.post(self.url(path)).json(&body))
            .send()
            .await
            .unwrap()
    }

    async fn get_json(&self, path: &str) -> serde_json::Value {
        let response = self.get(path).await;
        assert!(
            response.status().is_success(),
            "GET {path} -> {}",
            response.status()
        );
        response.json().await.unwrap()
    }

    async fn post_json(&self, path: &str, body: serde_json::Value) -> serde_json::Value {
        let response = self.post(path, body).await;
        assert!(
            response.status().is_success(),
            "POST {path} -> {} {}",
            response.status(),
            response.text().await.unwrap_or_default()
        );
        response.json().await.unwrap()
    }
}

#[tokio::test]
async fn health_needs_no_token_and_everything_else_does() {
    let harness = Harness::start().await;

    let health = harness
        .client
        .get(harness.url("/health"))
        .send()
        .await
        .unwrap();
    assert!(health.status().is_success());
    assert_eq!(
        health.json::<serde_json::Value>().await.unwrap()["status"],
        "ok"
    );

    let unauthenticated = harness
        .client
        .get(harness.url("/api/system/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), 401);

    let wrong_token = harness
        .client
        .get(harness.url("/api/system/status"))
        .bearer_auth("not-the-token")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_token.status(), 401);

    assert!(harness
        .get("/api/system/status")
        .await
        .status()
        .is_success());
}

#[tokio::test]
async fn a_request_from_another_origin_is_refused_even_with_the_right_token() {
    let harness = Harness::start().await;

    let response = harness
        .client
        .get(harness.url("/api/system/status"))
        .bearer_auth(&harness.service.token)
        .header("Origin", "https://evil.example.com")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 403);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "forbidden");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("does not recognise"));
}

#[tokio::test]
async fn the_service_listens_on_loopback_only() {
    let harness = Harness::start().await;
    assert!(
        harness.service.address.ip().is_loopback(),
        "the service must not be reachable from the network"
    );
}

#[tokio::test]
async fn an_unknown_route_is_a_clean_404_not_a_crash() {
    let harness = Harness::start().await;
    let response = harness.get("/api/does-not-exist").await;
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn the_shipped_agent_templates_exist_from_first_start() {
    let harness = Harness::start().await;
    let templates = harness.get_json("/api/agents/templates").await;
    assert_eq!(templates.as_array().unwrap().len(), 10);
    assert!(templates
        .as_array()
        .unwrap()
        .iter()
        .all(|t| !t["agent_id"].is_null()));
}

#[tokio::test]
async fn first_run_reports_no_connection_and_says_what_to_do() {
    let harness = Harness::start().await;
    let connections = harness.get_json("/api/connections").await;
    assert_eq!(connections["ready_for_chat"], false);
    assert!(connections["guidance"]
        .as_str()
        .unwrap()
        .contains("works without one"));
}

/// The critical path: connect a runtime, choose a model, hold a chat that
/// persists, and read it back after a restart of the reader.
#[tokio::test]
async fn connect_a_runtime_then_hold_a_streaming_chat_that_persists() {
    let runtime = FakeRuntime::start(Behaviour::ollama_default()).await;
    let harness = Harness::start().await;

    // 1. Create the connection.
    let connection = harness
        .post_json(
            "/api/connections",
            json!({
                "kind": "ollama",
                "label": "Ollama",
                "endpoint": runtime.base_url,
                "default_model": "llama3.1:8b",
                "enabled": true
            }),
        )
        .await;
    let connection_id = connection["id"].as_str().unwrap().to_string();

    // 2. Test it and see the models it serves.
    let test = harness
        .post_json(&format!("/api/connections/{connection_id}/test"), json!({}))
        .await;
    assert_eq!(test["health"], "reachable");
    assert_eq!(test["models"].as_array().unwrap().len(), 2);

    let connections = harness.get_json("/api/connections").await;
    assert_eq!(connections["ready_for_chat"], true);

    // 3. Start a conversation and stream a reply.
    let conversation = harness
        .post_json("/api/conversations", json!({ "title": "" }))
        .await;
    let conversation_id = conversation["id"].as_str().unwrap().to_string();

    let response = harness
        .post(
            &format!("/api/conversations/{conversation_id}/messages"),
            json!({ "message": "Say hello" }),
        )
        .await;
    assert!(response.status().is_success());
    assert!(response
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));

    let body = response.text().await.unwrap();
    let events: Vec<serde_json::Value> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|payload| serde_json::from_str(payload.trim()).ok())
        .collect();

    let kinds: Vec<&str> = events.iter().filter_map(|e| e["type"].as_str()).collect();
    assert!(kinds.contains(&"start"), "{kinds:?}");
    assert!(kinds.contains(&"delta"), "{kinds:?}");
    assert!(kinds.contains(&"done"), "{kinds:?}");

    let streamed: String = events
        .iter()
        .filter(|e| e["type"] == "delta")
        .filter_map(|e| e["text"].as_str())
        .collect();
    assert_eq!(streamed, "Hello, world");

    // 4. The conversation persisted, titled from the first message.
    let saved = harness
        .get_json(&format!("/api/conversations/{conversation_id}"))
        .await;
    assert_eq!(saved["title"], "Say hello");
    let messages = saved["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "Hello, world");
    assert_eq!(messages[1]["model"], "llama3.1:8b");
}

#[tokio::test]
async fn a_failing_runtime_produces_an_error_event_and_a_recorded_failure() {
    let runtime = FakeRuntime::start(Behaviour {
        chat_status: Some(503),
        ..Behaviour::ollama_default()
    })
    .await;
    let harness = Harness::start().await;

    harness
        .post_json(
            "/api/connections",
            json!({
                "kind": "ollama", "label": "Ollama", "endpoint": runtime.base_url,
                "default_model": "llama3.1:8b", "enabled": true
            }),
        )
        .await;
    let conversation = harness.post_json("/api/conversations", json!({})).await;
    let conversation_id = conversation["id"].as_str().unwrap().to_string();

    let body = harness
        .post(
            &format!("/api/conversations/{conversation_id}/messages"),
            json!({ "message": "Hello" }),
        )
        .await
        .text()
        .await
        .unwrap();

    let events: Vec<serde_json::Value> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|payload| serde_json::from_str(payload.trim()).ok())
        .collect();
    let error = events
        .iter()
        .find(|e| e["type"] == "error")
        .expect("an error event");
    assert_eq!(error["retryable"], true);

    // The failure is recorded rather than leaving an empty assistant message
    // with no explanation.
    let saved = harness
        .get_json(&format!("/api/conversations/{conversation_id}"))
        .await;
    let assistant = &saved["messages"].as_array().unwrap()[1];
    assert!(assistant["stopped_reason"]
        .as_str()
        .unwrap()
        .starts_with("failed:"));
}

#[tokio::test]
async fn preferences_persist_across_requests_and_reset_cleanly() {
    let harness = Harness::start().await;

    let current = harness.get_json("/api/settings/preferences").await;
    let mut preferences = current["preferences"].clone();
    preferences["theme"] = json!("dark");
    preferences["accent"] = json!("ember");
    preferences["sidebar_collapsed"] = json!(true);

    let response = harness
        .authorised(
            harness
                .client
                .put(harness.url("/api/settings/preferences"))
                .json(&preferences),
        )
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());

    let reloaded = harness.get_json("/api/settings/preferences").await;
    assert_eq!(reloaded["preferences"]["theme"], "dark");
    assert_eq!(reloaded["preferences"]["accent"], "ember");
    assert_eq!(reloaded["preferences"]["sidebar_collapsed"], true);

    harness
        .post_json("/api/settings/preferences/reset", json!({}))
        .await;
    let after_reset = harness.get_json("/api/settings/preferences").await;
    assert_eq!(after_reset["preferences"]["theme"], "system");
}

#[tokio::test]
async fn the_emergency_stop_blocks_permission_checks_over_the_api() {
    let harness = Harness::start().await;

    harness
        .post_json(
            "/api/permissions/grants",
            json!({
                "capability": "knowledge_search",
                "scopes": [{ "type": "global" }],
                "decision": "allow"
            }),
        )
        .await;

    let allowed = harness
        .post_json(
            "/api/permissions/check",
            json!({ "capability": "knowledge_search", "scopes": [] }),
        )
        .await;
    assert_eq!(allowed["outcome"], "allowed");

    harness
        .post_json(
            "/api/system/emergency-stop",
            json!({ "engaged": true, "revoke_all_permissions": false }),
        )
        .await;

    let stopped = harness
        .post_json(
            "/api/permissions/check",
            json!({ "capability": "knowledge_search", "scopes": [] }),
        )
        .await;
    assert_eq!(stopped["outcome"], "stopped");

    let status = harness.get_json("/api/system/status").await;
    assert_eq!(status["emergency_stop"], true);
}

#[tokio::test]
async fn knowledge_can_be_authorised_indexed_searched_and_revoked_over_the_api() {
    let harness = Harness::start().await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("policy.md"),
        "# Leave\n\nEvery employee receives 25 days of annual leave each year.",
    )
    .unwrap();

    let source = harness
        .post_json(
            "/api/knowledge/sources",
            json!({ "path": tmp.path().to_string_lossy(), "label": "Docs" }),
        )
        .await;
    let source_id = source["id"].as_str().unwrap().to_string();

    let indexed = harness
        .post_json(
            &format!("/api/knowledge/sources/{source_id}/index"),
            json!({}),
        )
        .await;
    assert_eq!(indexed["indexed"], 1);
    assert_eq!(indexed["used_fallback_embeddings"], true);
    assert!(indexed["message"]
        .as_str()
        .unwrap()
        .contains("without an embedding model"));

    let results = harness
        .post_json(
            "/api/knowledge/search",
            json!({ "query": "how many days of annual leave", "source_ids": [source_id] }),
        )
        .await;
    assert!(!results["hits"].as_array().unwrap().is_empty());
    assert_eq!(results["citations"][0]["file_name"], "policy.md");
    assert!(!results["citations"][0]["locator"].is_null());

    let response = harness
        .authorised(
            harness
                .client
                .put(harness.url(&format!("/api/knowledge/sources/{source_id}/authorisation")))
                .json(&json!({ "authorised": false })),
        )
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());

    let after = harness
        .post_json(
            "/api/knowledge/search",
            json!({ "query": "annual leave", "source_ids": [source_id] }),
        )
        .await;
    assert!(after["hits"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn the_activity_log_records_what_happened_and_redacts_secrets() {
    let harness = Harness::start().await;

    harness
        .post_json(
            "/api/connections",
            json!({
                "kind": "openai_compatible",
                "label": "Hosted",
                "endpoint": "https://api.example.com/v1",
                "api_key": "sk-live-must-not-appear",
                "enabled": false
            }),
        )
        .await;

    let log = harness.get_json("/api/activity?limit=50").await;
    let entries = log["entries"].as_array().unwrap();
    assert!(entries.iter().any(|e| e["action"] == "connection.create"));

    let raw = serde_json::to_string(&log).unwrap();
    assert!(
        !raw.contains("sk-live-must-not-appear"),
        "the log leaked a key"
    );

    let export = harness
        .get("/api/activity/export")
        .await
        .text()
        .await
        .unwrap();
    assert!(export.contains("connection.create"));
    assert!(!export.contains("sk-live-must-not-appear"));
}

#[tokio::test]
async fn a_body_larger_than_the_limit_is_refused_rather_than_buffered() {
    let harness = Harness::start().await;
    let huge = "x".repeat(otwono_local_service::MAX_BODY_BYTES + 1_024);
    let response = harness
        .authorised(
            harness
                .client
                .post(harness.url("/api/conversations"))
                .json(&json!({ "title": huge })),
        )
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_client_error(),
        "expected a client error, got {}",
        response.status()
    );
}

#[tokio::test]
async fn the_state_used_by_tests_is_isolated_per_service() {
    let first = Harness::start().await;
    let second = Harness::start().await;

    first
        .post_json("/api/conversations", json!({ "title": "In the first" }))
        .await;

    let listed = second.get_json("/api/conversations").await;
    assert!(
        listed.as_array().unwrap().is_empty(),
        "each service must have its own database"
    );
    assert_ne!(first.service.token, second.service.token);
    let _ = (&first.state, &second.state);
}

/// A regression test for a real gap: projects assigned the enabled
/// connection's default model to agents that had none, but workspace sessions
/// did not, so a boardroom refused to run on a machine where chat and projects
/// both worked. Nothing here chooses a model for an agent by hand.
#[tokio::test]
async fn a_boardroom_session_runs_after_only_connecting_a_runtime() {
    let runtime = FakeRuntime::start(Behaviour::ollama_default()).await;
    let harness = Harness::start().await;

    harness
        .post_json(
            "/api/connections",
            json!({
                "kind": "ollama",
                "label": "Ollama",
                "endpoint": runtime.base_url,
                "default_model": "llama3.1:8b",
                "enabled": true
            }),
        )
        .await;

    // The shipped agents have no model of their own at this point.
    let agents = harness.get_json("/api/agents").await;
    let agents = agents.as_array().unwrap();
    assert!(
        agents.iter().all(|agent| agent["model"].is_null()),
        "the test is meaningless if an agent already has a model"
    );

    let workspace = harness
        .post_json(
            "/api/workspaces",
            json!({ "kind": "boardroom", "name": "Release Board" }),
        )
        .await;
    let workspace_id = workspace["id"].as_str().unwrap().to_string();

    for (position, agent) in agents.iter().take(3).enumerate() {
        harness
            .post_json(
                &format!("/api/workspaces/{workspace_id}/members"),
                json!({ "agent_id": agent["id"], "is_coordinator": position == 0 }),
            )
            .await;
    }

    let session = harness
        .post_json(
            &format!("/api/workspaces/{workspace_id}/sessions"),
            json!({ "question": "Should we ship on Friday?" }),
        )
        .await;
    let session_id = session["id"].as_str().unwrap().to_string();

    let response = harness
        .post(
            &format!("/api/workspaces/{workspace_id}/sessions/{session_id}/run"),
            json!({}),
        )
        .await;
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(status.is_success(), "the session should run: {body}");
    assert_eq!(body["stage"], "completed");
    assert!(
        !body["contributions"].as_array().unwrap().is_empty(),
        "every member should have spoken"
    );
}
