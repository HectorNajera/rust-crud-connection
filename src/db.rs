use std::time::Duration;

use mongodb::{
    Client, Database, IndexModel,
    bson::doc,
    options::{ClientOptions, IndexOptions},
};

use crate::{
    config::Config,
    products::ProductDoc,
    retry::{RetryPolicy, is_transient},
};

pub const PRODUCTS: &str = "products";

/// Builds a pooled client. The driver connects lazily, so this succeeds even
/// when MongoDB is down; requests will then fail with 503 until it comes back.
pub async fn connect(cfg: &Config) -> mongodb::error::Result<Client> {
    let mut opts = ClientOptions::parse(&cfg.mongo_uri).await?;
    opts.app_name = Some("catalog-api".into());
    opts.max_pool_size = Some(cfg.mongo_max_pool_size);
    opts.min_pool_size = Some(cfg.mongo_min_pool_size);
    opts.max_idle_time = Some(cfg.mongo_max_idle_time);
    opts.connect_timeout = Some(cfg.mongo_connect_timeout);
    opts.server_selection_timeout = Some(cfg.mongo_server_selection_timeout);
    opts.retry_reads = Some(true);
    opts.retry_writes = Some(true);

    tracing::info!(
        db = cfg.mongo_db,
        max_pool_size = cfg.mongo_max_pool_size,
        min_pool_size = cfg.mongo_min_pool_size,
        "mongodb client configured"
    );
    Client::with_options(opts)
}

pub async fn ping(db: &Database) -> mongodb::error::Result<()> {
    db.run_command(doc! { "ping": 1 }).await.map(|_| ())
}

/// Creates indexes in the background, retrying until MongoDB is reachable so
/// the HTTP server can start even if the database is not up yet.
pub fn ensure_indexes_in_background(db: Database, retry: RetryPolicy) {
    tokio::spawn(async move {
        let coll = db.collection::<ProductDoc>(PRODUCTS);
        let indexes = [
            IndexModel::builder()
                .keys(doc! { "sku": 1 })
                .options(IndexOptions::builder().unique(true).build())
                .build(),
            IndexModel::builder()
                .keys(doc! { "category": 1, "created_at": -1 })
                .build(),
        ];
        let mut attempt = 1;
        loop {
            match coll.create_indexes(indexes.clone()).await {
                Ok(_) => {
                    tracing::info!("mongodb indexes ensured");
                    return;
                }
                Err(err) if !is_transient(&err) => {
                    tracing::error!(%err, "could not ensure indexes, giving up");
                    return;
                }
                Err(err) => {
                    let delay = retry.backoff(attempt).max(Duration::from_secs(1));
                    tracing::warn!(%err, ?delay, "could not ensure indexes, retrying");
                    tokio::time::sleep(delay).await;
                    attempt = (attempt + 1).min(10);
                }
            }
        }
    });
}
