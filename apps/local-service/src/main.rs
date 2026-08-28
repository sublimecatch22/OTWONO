//! The standalone service binary.
//!
//! The desktop application embeds the same library; this binary is for
//! development, automated tests and headless use.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,otwono=debug".into()),
        )
        .init();

    let port = std::env::var("OTWONO_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(0);

    let service = otwono_local_service::start(port).await?;
    println!("OTWONO local service listening on {}", service.base_url());
    println!(
        "Handshake file: {}",
        otwono_local_service::runtime::handshake_path()?.display()
    );

    tokio::signal::ctrl_c().await?;
    println!("\nStopping.");
    if let Ok(path) = otwono_local_service::runtime::handshake_path() {
        otwono_local_service::runtime::remove_handshake(&path);
    }
    service.handle.abort();
    Ok(())
}
