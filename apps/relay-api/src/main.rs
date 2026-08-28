//! The relay binary.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,otwono=debug".into()),
        )
        .init();

    let database =
        std::env::var("OTWONO_RELAY_DB").unwrap_or_else(|_| "otwono-relay.sqlite3".into());
    let bind = std::env::var("OTWONO_RELAY_BIND").unwrap_or_else(|_| "127.0.0.1:8788".into());
    let origins = std::env::var("OTWONO_RELAY_ORIGINS")
        .map(|value| value.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();

    let db = otwono_relay::db::RelayDb::open(&database)?;
    let state = otwono_relay::RelayState::new(db, origins);
    let relay = otwono_relay::serve(state, &bind).await?;

    println!("OTWONO relay listening on {}", relay.base_url());
    println!("Database: {database}");
    println!(
        "This service stores accounts, profiles and approved project metadata only. It cannot \
         store conversations, files, knowledge indexes or models."
    );

    tokio::signal::ctrl_c().await?;
    println!("\nStopping.");
    relay.handle.abort();
    Ok(())
}
