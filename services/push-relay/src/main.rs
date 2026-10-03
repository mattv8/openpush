use peppy_push_relay::{AppState, app, deliver_one, providers::ConfiguredProvider};
use std::{net::SocketAddr, sync::Arc, time::Duration};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter("peppy_push_relay=info")
        .init();
    let database_url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is required")?;
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8090".into());
    let selection = std::env::var("PEPPY_RELAY_PROVIDER").unwrap_or_else(|_| "unconfigured".into());
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await?;
    if std::env::args().nth(1).as_deref() == Some("migrate") {
        sqlx::migrate!().run(&pool).await?;
        return Ok(());
    }
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    let state = if selection == "unconfigured" {
        AppState::new(pool)
    } else {
        AppState::with_provider(
            pool,
            Arc::new(
                ConfiguredProvider::from_environment(&selection).map_err(std::io::Error::other)?,
            ),
        )
    };
    if !state.provider_configured() {
        tracing::warn!(%bind_addr, "push relay has no provider adapter configured and cannot deliver push");
    }
    let worker_state = state.clone();
    tokio::spawn(async move {
        loop {
            while deliver_one(&worker_state).await.unwrap_or(false) {}
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
