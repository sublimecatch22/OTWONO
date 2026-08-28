//! Route modules and the router that wires them together.

pub mod account;
pub mod activity;
pub mod agents;
pub mod budget;
pub mod chat;
pub mod knowledge;
pub mod marketplace;
pub mod permissions;
pub mod projects;
pub mod providers;
pub mod settings;
pub mod system;
pub mod workspaces;

use axum::routing::{delete, get, post, put};
use axum::Router;

use otwono_store::repo::agents::AgentRepo;
use otwono_store::repo::providers::ProviderRepo;

use crate::error::ApiResult;
use crate::state::AppState;

/// Give every agent that has no model of its own the enabled connection's
/// default, so work can be run straight after setup.
///
/// This lives here rather than in one route module because every way of
/// running an agent needs it: a project, a workspace session and a lab
/// experiment all fail the same way without it, and a user who has connected
/// a runtime and chosen a model has done everything that should be asked.
pub(crate) fn ensure_agent_models(state: &AppState) -> ApiResult<()> {
    let providers = ProviderRepo::new(&state.db);
    let Some(connection) = providers.list()?.into_iter().find(|c| c.enabled) else {
        return Ok(());
    };
    let Some(default_model) = connection.default_model.clone() else {
        return Ok(());
    };
    let agents = AgentRepo::new(&state.db);
    for mut agent in agents.list(None, false)? {
        if agent.model.is_none() || agent.provider_connection_id.is_none() {
            agent.model = agent.model.or_else(|| Some(default_model.clone()));
            agent.provider_connection_id = Some(connection.id.clone());
            agents
                .update(&agent, Some("assigned the default connection"))
                .ok();
        }
    }
    Ok(())
}

/// Everything behind the authentication guard.
pub fn api_router() -> Router<AppState> {
    Router::new()
        // system
        .route("/system/status", get(system::status))
        .route("/system/emergency-stop", post(system::set_emergency_stop))
        .route("/system/backup", post(system::backup))
        .route("/system/backups", get(system::list_backups))
        // settings
        .route(
            "/settings/preferences",
            get(settings::get).put(settings::put),
        )
        .route("/settings/preferences/reset", post(settings::reset))
        .route("/settings/export", get(settings::export))
        .route("/settings/import", post(settings::import))
        // providers
        .route("/connections", get(providers::list).post(providers::create))
        .route("/connections/detect", post(providers::detect_runtimes))
        .route(
            "/connections/{id}",
            put(providers::update).delete(providers::delete),
        )
        .route("/connections/{id}/test", post(providers::test))
        // agents
        .route("/agents", get(agents::list).post(agents::create))
        .route("/agents/templates", get(agents::list_templates))
        .route("/agents/templates/seed", post(agents::seed_templates))
        .route("/agents/import", post(agents::import))
        .route(
            "/agents/{id}",
            get(agents::get).put(agents::update).delete(agents::delete),
        )
        .route("/agents/{id}/versions", get(agents::versions))
        .route(
            "/agents/{id}/versions/{version}/restore",
            post(agents::restore_version),
        )
        .route("/agents/{id}/export", get(agents::export))
        .route("/agents/{id}/test", post(agents::test_console))
        // chat
        .route("/conversations", get(chat::list).post(chat::create))
        .route(
            "/conversations/{id}",
            get(chat::get).put(chat::update).delete(chat::delete),
        )
        .route("/conversations/{id}/messages", post(chat::send))
        .route("/conversations/{id}/preview", post(chat::preview))
        .route("/conversations/{id}/truncate", post(chat::truncate))
        .route("/conversations/{id}/export", get(chat::export))
        // knowledge
        .route(
            "/knowledge/sources",
            get(knowledge::list_sources).post(knowledge::authorise),
        )
        .route("/knowledge/sources/{id}", delete(knowledge::delete_source))
        .route(
            "/knowledge/sources/{id}/authorisation",
            put(knowledge::set_authorised),
        )
        .route("/knowledge/sources/{id}/index", post(knowledge::index))
        .route(
            "/knowledge/sources/{id}/documents",
            get(knowledge::documents),
        )
        .route("/knowledge/search", post(knowledge::search))
        .route("/knowledge/browse", get(knowledge::browse))
        // projects
        .route("/projects", get(projects::list).post(projects::create))
        .route(
            "/projects/{id}",
            get(projects::get)
                .put(projects::update)
                .delete(projects::delete),
        )
        .route("/projects/{id}/state", post(projects::transition))
        .route("/projects/{id}/tasks", post(projects::add_task))
        .route("/projects/{id}/plan", post(projects::plan))
        .route("/projects/{id}/run", post(projects::run))
        .route(
            "/projects/{id}/tasks/{task_id}/decision",
            post(projects::decide_task),
        )
        .route("/projects/{id}/report", get(projects::report))
        // workspaces
        .route(
            "/workspaces",
            get(workspaces::list).post(workspaces::create),
        )
        .route("/workspaces/kinds", get(workspaces::kinds))
        .route(
            "/workspaces/{id}",
            get(workspaces::get)
                .put(workspaces::update)
                .delete(workspaces::delete),
        )
        .route("/workspaces/{id}/duplicate", post(workspaces::duplicate))
        .route("/workspaces/{id}/members", post(workspaces::add_member))
        .route(
            "/workspaces/{id}/members/{agent_id}",
            delete(workspaces::remove_member),
        )
        .route(
            "/workspaces/{id}/sessions",
            post(workspaces::create_session),
        )
        .route(
            "/workspaces/{id}/sessions/{session_id}",
            get(workspaces::get_session),
        )
        .route(
            "/workspaces/{id}/sessions/{session_id}/run",
            post(workspaces::run_session),
        )
        .route(
            "/workspaces/{id}/experiments",
            post(workspaces::create_experiment),
        )
        .route(
            "/workspaces/{id}/experiments/{experiment_id}/run",
            post(workspaces::run_experiment),
        )
        .route(
            "/workspaces/{id}/experiments/{experiment_id}/promote",
            post(workspaces::promote_variant),
        )
        // permissions
        .route("/permissions", get(permissions::list))
        .route("/permissions/history", get(permissions::history))
        .route("/permissions/grants", post(permissions::grant))
        .route("/permissions/grants/{id}/revoke", post(permissions::revoke))
        .route("/permissions/revoke-all", post(permissions::revoke_all))
        .route(
            "/permissions/requests/{id}/resolve",
            post(permissions::resolve),
        )
        .route("/permissions/check", post(permissions::check))
        // budget
        .route("/budgets", get(budget::list).post(budget::create))
        .route("/budgets/{id}", get(budget::get))
        .route("/budgets/{id}/expenses", post(budget::record_expense))
        .route(
            "/budgets/{id}/expenses/{expense_id}/decision",
            post(budget::decide_expense),
        )
        .route(
            "/budgets/{id}/expenses/{expense_id}/receipt",
            post(budget::attach_receipt),
        )
        // marketplace
        .route(
            "/marketplace/listings",
            get(marketplace::browse).post(marketplace::create_listing),
        )
        .route("/marketplace/my-listings", get(marketplace::my_listings))
        .route("/marketplace/my-work", get(marketplace::my_work))
        .route("/marketplace/listings/{id}", get(marketplace::get_listing))
        .route(
            "/marketplace/listings/{id}/state",
            post(marketplace::transition_listing),
        )
        .route("/marketplace/listings/{id}/apply", post(marketplace::apply))
        .route(
            "/marketplace/listings/{id}/assign",
            post(marketplace::assign),
        )
        .route(
            "/marketplace/listings/{id}/messages",
            post(marketplace::post_message),
        )
        .route(
            "/marketplace/listings/{id}/submit",
            post(marketplace::submit),
        )
        .route(
            "/marketplace/listings/{id}/review",
            post(marketplace::review),
        )
        .route(
            "/marketplace/listings/{id}/report",
            post(marketplace::report),
        )
        .route("/marketplace/ledger", get(marketplace::ledger))
        .route(
            "/marketplace/worker-profile",
            get(marketplace::worker_profile).put(marketplace::save_worker_profile),
        )
        // account
        .route("/account", get(account::status))
        .route("/account/link", post(account::link))
        .route("/account/sync", post(account::sync))
        .route("/account/unlink", post(account::unlink))
        .route("/account/pairing-code", post(account::create_pairing_code))
        .route(
            "/account/pairing-code/redeem",
            post(account::redeem_pairing_code),
        )
        // activity
        .route("/activity", get(activity::list))
        .route("/activity/export", get(activity::export))
}
