use std::{
    future::IntoFuture,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use mongodb::error::{
    Error as MongoError, ErrorKind, RETRYABLE_WRITE_ERROR, SYSTEM_OVERLOADED_ERROR,
    TRANSIENT_TRANSACTION_ERROR,
};

use crate::{config::Config, error::AppError};

/// Server error codes that indicate a transient condition (network blip,
/// failover, shutdown, primary step-down...).
const TRANSIENT_CODES: &[i32] = &[
    6,     // HostUnreachable
    7,     // HostNotFound
    89,    // NetworkTimeout
    91,    // ShutdownInProgress
    189,   // PrimarySteppedDown
    262,   // ExceededTimeLimit
    9001,  // SocketException
    10107, // NotWritablePrimary
    11600, // InterruptedAtShutdown
    11602, // InterruptedDueToReplStateChange
    13435, // NotPrimaryNoSecondaryOk
    13436, // NotPrimaryOrSecondary
];

/// Whether an error is worth retrying.
pub fn is_transient(err: &MongoError) -> bool {
    if [RETRYABLE_WRITE_ERROR, TRANSIENT_TRANSACTION_ERROR, SYSTEM_OVERLOADED_ERROR]
        .iter()
        .any(|label| err.contains_label(label))
    {
        return true;
    }
    match err.kind.as_ref() {
        ErrorKind::Io(_)
        | ErrorKind::ServerSelection { .. }
        | ErrorKind::ConnectionPoolCleared { .. } => true,
        ErrorKind::Command(e) => TRANSIENT_CODES.contains(&e.code),
        _ => false,
    }
}

/// Retries an operation with per-attempt timeout and exponential backoff with jitter.
///
/// Only use it for idempotent operations (reads, `$set` updates, deletes by id,
/// inserts with a client-generated `_id`).
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    pub op_timeout: Duration,
}

impl RetryPolicy {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            max_attempts: cfg.retry_max_attempts.max(1),
            base_delay: cfg.retry_base_delay,
            max_delay: cfg.retry_max_delay,
            op_timeout: cfg.op_timeout,
        }
    }

    pub async fn run<T, F, Fut>(&self, op: &'static str, mut f: F) -> Result<T, AppError>
    where
        F: FnMut() -> Fut,
        Fut: IntoFuture<Output = mongodb::error::Result<T>>,
    {
        let mut attempt = 1;
        loop {
            let err = match tokio::time::timeout(self.op_timeout, f().into_future()).await {
                Ok(Ok(value)) => return Ok(value),
                Ok(Err(e)) if is_transient(&e) => AppError::from(e),
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => AppError::Timeout,
            };
            if attempt >= self.max_attempts {
                tracing::error!(op, attempt, ?err, "giving up after retries");
                return Err(err);
            }
            let delay = self.backoff(attempt);
            tracing::warn!(op, attempt, ?delay, ?err, "transient failure, retrying");
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }

    /// `base * 2^(attempt-1)`, capped at `max_delay`, with "full jitter".
    pub fn backoff(&self, attempt: u32) -> Duration {
        let exp = self
            .base_delay
            .saturating_mul(2u32.saturating_pow(attempt - 1))
            .min(self.max_delay);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        exp.mul_f64(0.5 + (nanos % 1000) as f64 / 2000.0)
    }
}
