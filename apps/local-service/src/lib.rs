//! The OTWONO AI local application service.
//!
//! It listens on loopback only, requires a bearer token that is written to an
//! owner-only file, and validates the request `Origin` against an allow-list.
//! It is compiled into the desktop application; the standalone binary exists
//! for development, tests and headless use.

pub mod auth;
pub mod error;
pub mod routes;
pub mod runtime;
pub mod state;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::routing::get;
use axum::Router;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

pub use state::AppState;

/// Largest request body accepted. Generous enough for a pasted document,
/// small enough that a runaway client cannot exhaust memory.
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Build the whole application.
pub fn app(state: AppState) -> Router {
    let api = routes::api_router().layer(axum::middleware::from_fn_with_state(
        state.clone(),
        auth::guard,
    ));

    Router::new()
        .route("/health", get(routes::system::health))
        .nest("/api", api)
        .fallback(not_found)
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Anything that matched no route.
async fn not_found(method: axum::http::Method, uri: axum::http::Uri) -> error::ApiError {
    error::ApiError::NotFound(format!(
        "There is no {method} {} in this version of the OTWONO API.",
        uri.path()
    ))
}

/// A running service.
pub struct RunningService {
    pub address: SocketAddr,
    pub token: String,
    pub handle: tokio::task::JoinHandle<()>,
}

impl RunningService {
    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }
}

/// Open the database, choose a secret store, bind a loopback port and start
/// serving. Writes the handshake file the desktop shell reads.
pub async fn start(preferred_port: u16) -> Result<RunningService> {
    let (db, migration) = otwono_store::Db::open_default().context("opening the database")?;
    if !migration.applied.is_empty() {
        tracing::info!(
            from = migration.from,
            to = migration.to,
            applied = ?migration.applied,
            backup = ?migration.backup,
            "database migrated"
        );
    }

    // Work interrupted by a previous shutdown is returned to a state the
    // scheduler can pick up, before anything else can touch it.
    match otwono_store::repo::projects::ProjectRepo::new(&db).recover_interrupted() {
        Ok(recovered) if !recovered.is_empty() => {
            tracing::info!(count = recovered.len(), "recovered interrupted tasks");
        }
        Err(error) => tracing::error!(%error, "recovery of interrupted tasks failed"),
        _ => {}
    }

    // The shipped agent templates exist from first run.
    if let Err(error) = otwono_agent_core::seed::seed_templates(&db) {
        tracing::error!(%error, "could not create the shipped agent templates");
    }

    let secrets: Arc<dyn otwono_store::SecretStore> =
        Arc::from(otwono_store::secrets::open_best()?);
    tracing::info!(backend = ?secrets.backend(), "secret storage selected");

    let token = runtime::mint_token();
    let state = AppState::new(db, secrets, token.clone())?;

    let (listener, address) = runtime::bind(preferred_port).await?;
    let handshake = runtime::RuntimeHandshake {
        version: env!("CARGO_PKG_VERSION").to_string(),
        address: address.ip().to_string(),
        port: address.port(),
        token: token.clone(),
        started_at: otwono_types::ids::format_ts(&otwono_types::now()),
        pid: std::process::id(),
    };
    runtime::write_handshake(&runtime::handshake_path()?, &handshake)?;

    let router = app(state);
    let handle = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router).await {
            tracing::error!(%error, "the local service stopped");
        }
    });

    tracing::info!(%address, "OTWONO local service listening");
    Ok(RunningService {
        address,
        token,
        handle,
    })
}

/// Start a service against an in-memory database. Used by tests and by the
/// desktop shell's diagnostics.
#[cfg(any(test, feature = "test-support"))]
pub async fn start_for_tests() -> Result<(RunningService, AppState)> {
    use otwono_store::secrets::EphemeralSecretStore;

    let db = otwono_store::Db::open_in_memory()?;
    otwono_agent_core::seed::seed_templates(&db)?;
    let token = runtime::mint_token();
    let state = AppState::new(db, Arc::new(EphemeralSecretStore::default()), token.clone())?;

    let (listener, address) = runtime::bind(0).await?;
    let router = app(state.clone());
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    Ok((
        RunningService {
            address,
            token,
            handle,
        },
        state,
    ))
}
