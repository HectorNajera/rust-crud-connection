mod config;
mod db;
mod error;
mod items;
mod products;
mod ratelimit;
mod retry;

use std::{net::SocketAddr, time::Duration};

use axum::{Json, Router, extract::State, http::StatusCode, middleware, routing::get};
use mongodb::Database;
use serde_json::{Value, json};
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

use crate::{config::Config, ratelimit::RateLimiter, retry::RetryPolicy};

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

    // If MongoDB never becomes reachable, stop the service instead of looping forever,
    // so the orchestrator (Docker, Kubernetes, systemd...) can restart or alert.
    let (fatal_tx, fatal_rx) = watch::channel(false);
    {
        let database = database.clone();
        let retry = retry.clone();
        let max_attempts = cfg.mongo_startup_max_attempts;
        tokio::spawn(async move {
            if let Err(err) = db::ensure_indexes(&database, &retry, max_attempts).await {
                tracing::error!(%err, max_attempts, "mongodb unavailable at startup, shutting down");
                let _ = fatal_tx.send(true);
            }
        });
    }

    let mut app = Router::new()
        .merge(items::router())
        .merge(products::router(&database, retry))
        .route("/health", get(|| async { Json(json!({ "status": "ok" })) }))
        .route("/health/ready", get(readiness).with_state(database));

    match RateLimiter::from_config(&cfg) {
        Some(limiter) => {
            tracing::info!(
                per_second = cfg.rate_limit_per_second,
                burst = cfg.rate_limit_burst,
                trust_proxy = cfg.rate_limit_trust_proxy,
                "rate limiting enabled"
            );
            limiter.spawn_eviction_task();
            app = app.layer(middleware::from_fn_with_state(limiter, ratelimit::middleware));
        }
        None => tracing::info!("rate limiting disabled"),
    }

    let listener = tokio::net::TcpListener::bind(&cfg.bind_addr).await.unwrap();
    tracing::info!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal(fatal_rx.clone()))
        .await
        .unwrap();

    tracing::info!("closing mongodb connection pool");
    client.shutdown().await;

    if *fatal_rx.borrow() {
        std::process::exit(1);
    }
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

/// Resolves on Ctrl+C or when a fatal error is reported through `fatal`.
async fn shutdown_signal(mut fatal: watch::Receiver<bool>) {
    tokio::select! {
        res = tokio::signal::ctrl_c() => {
            res.expect("failed to listen for ctrl-c");
            tracing::info!("shutdown signal received");
        }
        // If the sender is dropped (indexes created fine) this branch is disabled.
        Ok(_) = fatal.wait_for(|failed| *failed) => {
            tracing::info!("fatal error, shutting down");
        }
    }
}
