mod config;
mod db;
mod error;
mod items;
mod products;
mod retry;

use std::time::Duration;

use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use mongodb::Database;
use serde_json::{Value, json};
use tracing_subscriber::EnvFilter;

use crate::{config::Config, retry::RetryPolicy};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,mongodb=warn".into()),
        )
        .init();

    let cfg = Config::from_env();
    let retry = RetryPolicy::from_config(&cfg);

    let client = db::connect(&cfg).await.expect("invalid MongoDB configuration");
    let database = client.database(&cfg.mongo_db);
    db::ensure_indexes_in_background(database.clone(), retry.clone());

    let app = Router::new()
        .merge(items::router())
        .merge(products::router(&database, retry))
        .route("/health", get(|| async { Json(json!({ "status": "ok" })) }))
        .route("/health/ready", get(readiness).with_state(database));

    let listener = tokio::net::TcpListener::bind(&cfg.bind_addr).await.unwrap();
    tracing::info!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();

    tracing::info!("closing mongodb connection pool");
    client.shutdown().await;
}

/// Readiness probe: reports whether MongoDB is reachable.
async fn readiness(State(db): State<Database>) -> (StatusCode, Json<Value>) {
    match tokio::time::timeout(Duration::from_secs(2), db::ping(&db)).await {
        Ok(Ok(())) => (StatusCode::OK, Json(json!({ "status": "ok", "mongodb": "up" }))),
        Ok(Err(err)) => {
            tracing::warn!(%err, "readiness check failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "status": "degraded", "mongodb": "down" })),
            )
        }
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "degraded", "mongodb": "timeout" })),
        ),
    }
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c().await.expect("failed to listen for ctrl-c");
    tracing::info!("shutdown signal received");
}
