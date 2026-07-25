mod frontend;

pub mod management;
pub mod subsonic;

use axum::{Router, routing::get};

use crate::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(management::health))
        .merge(management::router())
        .merge(subsonic::router())
        .fallback(frontend::serve)
        .with_state(state)
}
