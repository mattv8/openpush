pub mod api;
pub mod config;
pub mod health;
pub mod storage;

use axum::{Router, routing::get};
use health::HealthState;

pub fn app(state: HealthState) -> Router {
    let api = api::router(state.database());
    Router::new()
        .route("/healthz", get(health::liveness))
        .route("/readyz", get(health::readiness))
        .with_state(state)
        .merge(api)
}
