//! Shared state handed to every route.

use std::sync::Arc;

use anyhow::Result;

use otwono_knowledge::Embedder;
use otwono_store::secrets::SecretStore;
use otwono_store::{Db, SecretBackend};

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub secrets: Arc<dyn SecretStore>,
    pub token: String,
    pub allowed_origins: Arc<Vec<String>>,
    /// Schema version reached at start-up, reported by `/api/system/status`.
    pub schema_version: i64,
    pub started_at: String,
}

impl AppState {
    pub fn new(db: Db, secrets: Arc<dyn SecretStore>, token: String) -> Result<Self> {
        let schema_version = db.schema_version()?;
        Ok(Self {
            db,
            secrets,
            token,
            allowed_origins: Arc::new(crate::auth::default_allowed_origins()),
            schema_version,
            started_at: otwono_types::ids::format_ts(&otwono_types::now()),
        })
    }

    pub fn secret_backend(&self) -> SecretBackend {
        self.secrets.backend()
    }

    /// Build the embedder for the current settings: a real model when one is
    /// configured and reachable, the labelled fallback otherwise.
    pub async fn embedder(&self) -> Embedder {
        match self.try_model_embedder().await {
            Some(embedder) => embedder,
            None => Embedder::lexical(),
        }
    }

    async fn try_model_embedder(&self) -> Option<Embedder> {
        use otwono_store::repo::providers::ProviderRepo;

        let connections = ProviderRepo::new(&self.db).list().ok()?;
        let connection = connections
            .into_iter()
            .find(|c| c.enabled && c.default_embedding_model.is_some())?;
        let model = connection.default_embedding_model.clone()?;
        let api_key = if connection.has_credential {
            self.secrets
                .get(&otwono_store::secrets::provider_key(&connection.id))
                .ok()
                .flatten()
        } else {
            None
        };
        let provider: Arc<dyn otwono_providers::Provider> = Arc::from(
            otwono_providers::adapter_for(connection.kind, &connection.endpoint, api_key),
        );
        Some(Embedder::with_model(provider, connection.id, model))
    }

    /// A provider adapter for a connection, with its credential attached.
    pub fn provider_for(
        &self,
        connection: &otwono_types::provider::ProviderConnection,
    ) -> Arc<dyn otwono_providers::Provider> {
        let api_key = if connection.has_credential {
            self.secrets
                .get(&otwono_store::secrets::provider_key(&connection.id))
                .ok()
                .flatten()
        } else {
            None
        };
        Arc::from(otwono_providers::adapter_for(
            connection.kind,
            &connection.endpoint,
            api_key,
        ))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests() -> Self {
        use otwono_store::secrets::EphemeralSecretStore;
        let db = Db::open_in_memory().expect("in-memory database");
        Self::new(
            db,
            Arc::new(EphemeralSecretStore::default()),
            crate::runtime::mint_token(),
        )
        .expect("test state")
    }
}
