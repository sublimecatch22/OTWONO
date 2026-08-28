//! The relay over real HTTP: accounts, sessions, profiles, pairing and the
//! limits on what it will store.

use otwono_relay::{serve_for_tests, RunningRelay};
use serde_json::json;

struct Harness {
    relay: RunningRelay,
    client: reqwest::Client,
}

impl Harness {
    async fn start() -> Self {
        let (relay, _state) = serve_for_tests().await.unwrap();
        Self {
            relay,
            client: reqwest::Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.relay.base_url())
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> reqwest::Response {
        self.client
            .post(self.url(path))
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn post_as(&self, path: &str, token: &str, body: serde_json::Value) -> reqwest::Response {
        self.client
            .post(self.url(path))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn get_as(&self, path: &str, token: &str) -> reqwest::Response {
        self.client
            .get(self.url(path))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
    }

    async fn put_as(&self, path: &str, token: &str, body: serde_json::Value) -> reqwest::Response {
        self.client
            .put(self.url(path))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    /// Register, verify and sign in, returning the session token.
    async fn account(&self, email: &str, scopes: &[&str]) -> (String, String) {
        let registered: serde_json::Value = self
            .post(
                "/v1/accounts",
                json!({ "email": email, "password": "a-long-enough-password", "display_name": "A Person" }),
            )
            .await
            .json()
            .await
            .unwrap();
        let account_id = registered["account_id"].as_str().unwrap().to_string();

        self.post(
            "/v1/accounts/verify",
            json!({ "token": registered["verification_token"] }),
        )
        .await;

        let signed_in: serde_json::Value = self
            .post(
                "/v1/accounts/sign-in",
                json!({
                    "email": email,
                    "password": "a-long-enough-password",
                    "device_label": "Test device",
                    "scopes": scopes
                }),
            )
            .await
            .json()
            .await
            .unwrap();

        (account_id, signed_in["token"].as_str().unwrap().to_string())
    }
}

#[tokio::test]
async fn health_states_plainly_what_the_relay_can_and_cannot_hold() {
    let harness = Harness::start().await;
    let body: serde_json::Value = harness
        .client
        .get(harness.url("/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body["status"], "ok");
    let stores = body["stores"].as_str().unwrap();
    assert!(stores.contains("cannot store conversations"));
    assert!(stores.contains("knowledge"));
}

#[tokio::test]
async fn an_account_can_register_verify_and_sign_in() {
    let harness = Harness::start().await;
    let registered: serde_json::Value = harness
        .post(
            "/v1/accounts",
            json!({ "email": "Person@Example.com", "password": "a-long-enough-password" }),
        )
        .await
        .json()
        .await
        .unwrap();

    assert_eq!(registered["email_verified"], false);
    assert!(registered["notice"]
        .as_str()
        .unwrap()
        .contains("no email service"));

    let verified = harness
        .post(
            "/v1/accounts/verify",
            json!({ "token": registered["verification_token"] }),
        )
        .await;
    assert!(verified.status().is_success());

    // The address is matched case-insensitively.
    let signed_in: serde_json::Value = harness
        .post(
            "/v1/accounts/sign-in",
            json!({ "email": "person@example.com", "password": "a-long-enough-password" }),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(signed_in["email_verified"], true);
    assert!(signed_in["token"].as_str().unwrap().len() > 40);
}

#[tokio::test]
async fn a_duplicate_address_does_not_reveal_that_the_account_exists() {
    let harness = Harness::start().await;
    harness
        .post(
            "/v1/accounts",
            json!({ "email": "person@example.com", "password": "a-long-enough-password" }),
        )
        .await;

    let response = harness
        .post(
            "/v1/accounts",
            json!({ "email": "person@example.com", "password": "another-long-password" }),
        )
        .await;
    assert_eq!(response.status(), 400);
    let body: serde_json::Value = response.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        !message.to_lowercase().contains("already exists")
            && !message.to_lowercase().contains("taken"),
        "the message confirms the account exists: {message}"
    );
}

#[tokio::test]
async fn a_wrong_password_gives_the_same_answer_as_an_unknown_account() {
    let harness = Harness::start().await;
    harness
        .account("person@example.com", &["profile.read"])
        .await;

    let wrong: serde_json::Value = harness
        .post(
            "/v1/accounts/sign-in",
            json!({ "email": "person@example.com", "password": "not-the-password" }),
        )
        .await
        .json()
        .await
        .unwrap();
    let unknown: serde_json::Value = harness
        .post(
            "/v1/accounts/sign-in",
            json!({ "email": "nobody@example.com", "password": "a-long-enough-password" }),
        )
        .await
        .json()
        .await
        .unwrap();

    assert_eq!(wrong["error"]["message"], unknown["error"]["message"]);
}

#[tokio::test]
async fn nothing_is_readable_without_a_token() {
    let harness = Harness::start().await;
    for path in ["/v1/profile", "/v1/sessions", "/v1/projects"] {
        let response = harness.client.get(harness.url(path)).send().await.unwrap();
        assert_eq!(response.status(), 401, "{path} should need a token");
    }
}

#[tokio::test]
async fn a_session_can_be_listed_and_revoked_and_stops_working() {
    let harness = Harness::start().await;
    let (_, token) = harness
        .account("person@example.com", &["profile.read"])
        .await;

    let sessions: serde_json::Value = harness
        .get_as("/v1/sessions", &token)
        .await
        .json()
        .await
        .unwrap();
    let session_id = sessions[0]["id"].as_str().unwrap().to_string();
    assert_eq!(sessions[0]["label"], "Test device");

    let revoked = harness
        .client
        .delete(harness.url(&format!("/v1/sessions/{session_id}")))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(revoked.status().is_success());

    let after = harness.get_as("/v1/profile", &token).await;
    assert_eq!(after.status(), 403);
}

#[tokio::test]
async fn a_token_only_does_what_its_scopes_allow() {
    let harness = Harness::start().await;
    let (_, read_only) = harness
        .account("person@example.com", &["profile.read"])
        .await;

    assert!(harness
        .get_as("/v1/profile", &read_only)
        .await
        .status()
        .is_success());

    let write = harness
        .put_as(
            "/v1/profile",
            &read_only,
            json!({ "display_name": "Changed" }),
        )
        .await;
    assert_eq!(write.status(), 403);
    let body: serde_json::Value = write.json().await.unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("profile.write"));

    let projects = harness.get_as("/v1/projects", &read_only).await;
    assert_eq!(projects.status(), 403);
}

#[tokio::test]
async fn a_profile_is_private_until_each_field_is_made_public() {
    let harness = Harness::start().await;
    let (account_id, token) = harness
        .account("person@example.com", &["profile.read", "profile.write"])
        .await;

    harness
        .put_as(
            "/v1/profile",
            &token,
            json!({
                "display_name": "A Person",
                "biography": "Private by default.",
                "interests": ["gardening"],
                "visibility": { "display_name": true }
            }),
        )
        .await;

    let public: serde_json::Value = harness
        .client
        .get(harness.url(&format!("/v1/profiles/{account_id}")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(public["fields"]["display_name"], "A Person");
    assert!(
        public["fields"].get("biography").is_none(),
        "biography was not made public"
    );
    assert!(public["fields"].get("interests").is_none());
}

#[tokio::test]
async fn an_ai_identity_is_labelled_wherever_it_is_shown() {
    let harness = Harness::start().await;
    let (account_id, token) = harness
        .account("agent@example.com", &["profile.read", "profile.write"])
        .await;

    harness
        .put_as(
            "/v1/profile",
            &token,
            json!({
                "display_name": "Research Assistant",
                "is_ai_identity": true,
                "visibility": { "display_name": true }
            }),
        )
        .await;

    let public: serde_json::Value = harness
        .client
        .get(harness.url(&format!("/v1/profiles/{account_id}")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let notice = public["identity_notice"].as_str().unwrap();
    assert!(notice.contains("AI identity, not a person"));
}

#[tokio::test]
async fn a_pairing_code_works_once_and_cannot_exceed_its_sessions_scopes() {
    let harness = Harness::start().await;
    let (account_id, token) = harness
        .account("person@example.com", &["profile.read", "profile.write"])
        .await;

    // A pairing cannot request a scope the session does not hold.
    let refused = harness
        .post_as(
            "/v1/pairings",
            &token,
            json!({ "scopes": ["marketplace.write"] }),
        )
        .await;
    assert_eq!(refused.status(), 403);

    let pairing: serde_json::Value = harness
        .post_as(
            "/v1/pairings",
            &token,
            json!({ "scopes": ["profile.read"] }),
        )
        .await
        .json()
        .await
        .unwrap();
    let code = pairing["code"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 8);

    let redeemed: serde_json::Value = harness
        .post(
            "/v1/pairings/redeem",
            json!({ "code": code.to_lowercase(), "site": "https://example.com" }),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(redeemed["account_id"], account_id);
    let site_token = redeemed["token"].as_str().unwrap().to_string();

    // The site token reads the profile but cannot write it.
    assert!(harness
        .get_as("/v1/profile", &site_token)
        .await
        .status()
        .is_success());
    assert_eq!(
        harness
            .put_as("/v1/profile", &site_token, json!({ "display_name": "x" }))
            .await
            .status(),
        403
    );

    // The code cannot be used a second time.
    let again = harness
        .post(
            "/v1/pairings/redeem",
            json!({ "code": code, "site": "https://evil.example" }),
        )
        .await;
    assert_eq!(again.status(), 403);
}

#[tokio::test]
async fn project_metadata_synchronises_but_content_sized_titles_are_refused() {
    let harness = Harness::start().await;
    let (_, token) = harness
        .account("person@example.com", &["profile.read", "projects.read"])
        .await;

    let synced = harness
        .post_as(
            "/v1/projects",
            &token,
            json!({
                "projects": [
                    { "id": "prj_1", "title": "Quarterly report", "state": "running",
                      "task_count": 4, "completed_tasks": 2 }
                ]
            }),
        )
        .await;
    assert!(synced.status().is_success());

    let listed: serde_json::Value = harness
        .get_as("/v1/projects", &token)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(listed[0]["title"], "Quarterly report");
    assert_eq!(listed[0]["completed_tasks"], 2);
    // Only metadata came back — there is no field that could carry content.
    assert!(listed[0].get("objective").is_none());
    assert!(listed[0].get("output").is_none());

    let refused = harness
        .post_as(
            "/v1/projects",
            &token,
            json!({
                "projects": [
                    { "id": "prj_2", "title": "x".repeat(400), "state": "draft" }
                ]
            }),
        )
        .await;
    assert_eq!(refused.status(), 400);
    let body: serde_json::Value = refused.json().await.unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("titles and states, not content"));
}

#[tokio::test]
async fn a_password_reset_signs_every_session_out() {
    let harness = Harness::start().await;
    let (_, token) = harness
        .account("person@example.com", &["profile.read"])
        .await;
    assert!(harness
        .get_as("/v1/profile", &token)
        .await
        .status()
        .is_success());

    let requested: serde_json::Value = harness
        .post(
            "/v1/accounts/reset",
            json!({ "email": "person@example.com" }),
        )
        .await
        .json()
        .await
        .unwrap();
    let reset_token = requested["reset_token"].as_str().unwrap().to_string();

    let completed: serde_json::Value = harness
        .post(
            "/v1/accounts/reset/complete",
            json!({ "token": reset_token, "password": "a-brand-new-password" }),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(completed["message"]
        .as_str()
        .unwrap()
        .contains("signed out"));

    assert_eq!(harness.get_as("/v1/profile", &token).await.status(), 403);

    let again: serde_json::Value = harness
        .post(
            "/v1/accounts/sign-in",
            json!({ "email": "person@example.com", "password": "a-brand-new-password" }),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(again["token"].is_string());
}

#[tokio::test]
async fn a_reset_request_for_an_unknown_address_says_the_same_thing() {
    let harness = Harness::start().await;
    let unknown: serde_json::Value = harness
        .post(
            "/v1/accounts/reset",
            json!({ "email": "nobody@example.com" }),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(unknown["message"]
        .as_str()
        .unwrap()
        .starts_with("If that address"));
    assert!(
        unknown["reset_token"].is_null(),
        "no token for an address with no account"
    );
}

/// A regression test for a deadlock: a handler that held a pooled connection
/// while the audit log took another one stalled until the pool timed out, once
/// per request. Anything slower than a few seconds here means it is back.
#[tokio::test]
async fn a_burst_of_requests_does_not_exhaust_the_connection_pool() {
    let harness = Harness::start().await;
    harness
        .account("person@example.com", &["profile.read"])
        .await;

    let started = std::time::Instant::now();
    for _ in 0..12 {
        harness
            .post(
                "/v1/accounts/sign-in",
                json!({ "email": "person@example.com", "password": "wrong" }),
            )
            .await;
    }
    let elapsed = started.elapsed();

    assert!(
        elapsed.as_secs() < 10,
        "twelve sign-in attempts took {elapsed:?}; a handler is probably holding two \
         connections at once"
    );
}

#[tokio::test]
async fn repeated_sign_in_attempts_are_rate_limited() {
    let harness = Harness::start().await;
    harness
        .account("person@example.com", &["profile.read"])
        .await;

    let mut limited = false;
    for _ in 0..15 {
        let response = harness
            .post(
                "/v1/accounts/sign-in",
                json!({ "email": "person@example.com", "password": "wrong" }),
            )
            .await;
        if response.status() == 429 {
            limited = true;
            break;
        }
    }
    assert!(
        limited,
        "the relay should rate-limit repeated sign-in attempts"
    );
}
