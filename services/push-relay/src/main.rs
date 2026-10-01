use openpush_push_relay::{AppState, app};
use std::net::SocketAddr;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter("openpush_push_relay=info")
        .init();
    let database_url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is required")?;
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8090".into());
    if let Ok(provider) = std::env::var("OPENPUSH_RELAY_PROVIDER")
        && provider != "unconfigured"
    {
        return Err(
            format!("OPENPUSH_RELAY_PROVIDER must be 'unconfigured', got '{provider}'").into(),
        );
    }
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await?;
    if std::env::args().nth(1).as_deref() == Some("migrate") {
        sqlx::migrate!().run(&pool).await?;
        return Ok(());
    }
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::warn!(%bind_addr, "push relay has no provider adapter configured and cannot deliver push");
    axum::serve(
        listener,
        app(AppState::new(pool)).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
