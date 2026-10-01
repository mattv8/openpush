use std::net::SocketAddr;

use openpush_server::{app, config::Config, health::HealthState};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter("openpush_server=info")
        .init();
    let command = std::env::args().nth(1);
    let config = Config::from_env()?;
    if command.as_deref() == Some("migrate") {
        sqlx::migrate!()
            .run(&sqlx::PgPool::connect(&config.database_url).await?)
            .await?;
        return Ok(());
    }
    if command.as_deref() == Some("create-owner") {
        let profile = std::env::var("OPENPUSH_OWNER_PUBLIC_KEY_PROFILE")
            .map_err(|_| "OPENPUSH_OWNER_PUBLIC_KEY_PROFILE JSON is required")?;
        let header = std::env::var("OPENPUSH_OWNER_VAULT_CHECK_HEADER_HEX")
            .map_err(|_| "OPENPUSH_OWNER_VAULT_CHECK_HEADER_HEX is required")?;
        let fingerprint = std::env::var("OPENPUSH_OWNER_PROFILE_FINGERPRINT")
            .map_err(|_| "OPENPUSH_OWNER_PROFILE_FINGERPRINT is required")?;
        let epoch = std::env::var("OPENPUSH_OWNER_KEY_EPOCH")
            .unwrap_or_else(|_| "1".into())
            .parse()?;
        let credential = openpush_server::api::create_owner(
            &sqlx::PgPool::connect(&config.database_url).await?,
            serde_json::from_str(&profile)?,
            hex::decode(header)?,
            fingerprint,
            epoch,
        )
        .await
        .map_err(std::io::Error::other)?;
        println!(
            "vault_id={}\ndevice_id={}\ndevice_token={}",
            credential.vault_id, credential.device_id, credential.device_token
        );
        return Ok(());
    }
    if command.as_deref() == Some("storage-check") {
        return openpush_server::health::storage_contract_check(&config).await;
    }
    if command.as_deref() == Some("healthcheck") {
        return healthcheck(config.bind_addr).await;
    }
    let state = HealthState::connect(&config).await?;
    let listener = tokio::net::TcpListener::bind(config.bind_addr).await?;
    tracing::info!(address = %config.bind_addr, "openpush server listening");
    axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn healthcheck(
    bind_addr: SocketAddr,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("http://127.0.0.1:{}/healthz", bind_addr.port());
    reqwest::get(url).await?.error_for_status()?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler can be installed");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
