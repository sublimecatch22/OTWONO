//! The OTWONO relay.
//!
//! A small, self-hostable service that holds accounts, profiles and the
//! metadata a user explicitly chose to synchronise. It is the only OTWONO
//! component reachable from the internet, and it is deliberately unable to
//! store a prompt, a file, a knowledge index or a model.

pub mod auth;
pub mod db;
pub mod routes;

use std::net::SocketAddr;

use anyhow::Result;
use axum::Router;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use crate::db::RelayDb;

/// Largest body the relay accepts. Profiles are small; anything larger is a
/// mistake or an attack.
pub const MAX_BODY_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub struct RelayState {
    pub db: RelayDb,
    /// Sites allowed to call the relay from a browser.
    pub allowed_origins: Vec<String>,
}

impl RelayState {
    pub fn new(db: RelayDb, allowed_origins: Vec<String>) -> Self {
        Self {
            db,
            allowed_origins,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests() -> Self {
        Self::new(
            RelayDb::open_in_memory().expect("in-memory relay"),
            Vec::new(),
        )
    }
}

pub fn app(state: RelayState) -> Router {
    routes::router()
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

pub struct RunningRelay {
    pub address: SocketAddr,
    pub handle: tokio::task::JoinHandle<()>,
}

impl RunningRelay {
    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }
}

pub async fn serve(state: RelayState, bind: &str) -> Result<RunningRelay> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let address = listener.local_addr()?;
    let router = app(state);
    let handle = tokio::spawn(async move {
        if let Err(error) = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        {
            tracing::error!(%error, "the relay stopped");
        }
    });
    Ok(RunningRelay { address, handle })
}

#[cfg(any(test, feature = "test-support"))]
pub async fn serve_for_tests() -> Result<(RunningRelay, RelayState)> {
    let state = RelayState::for_tests();
    let relay = serve(state.clone(), "127.0.0.1:0").await?;
    Ok((relay, state))
}
