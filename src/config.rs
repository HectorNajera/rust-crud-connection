use std::{env, fmt, str::FromStr, time::Duration};

/// Runtime configuration, read from environment variables with sane defaults.
#[derive(Clone)]
pub struct Config {
    pub bind_addr: String,
    pub mongo_uri: String,
    pub mongo_db: String,
    pub mongo_max_pool_size: u32,
    pub mongo_min_pool_size: u32,
    pub mongo_max_idle_time: Duration,
    pub mongo_connect_timeout: Duration,
    pub mongo_server_selection_timeout: Duration,
    pub op_timeout: Duration,
    pub retry_max_attempts: u32,
    pub retry_base_delay: Duration,
    pub retry_max_delay: Duration,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            bind_addr: var("BIND_ADDR", "0.0.0.0:3000".to_string()),
            mongo_uri: var("MONGODB_URI", "mongodb://localhost:27017".to_string()),
            mongo_db: var("MONGODB_DB", "catalog".to_string()),
            mongo_max_pool_size: var("MONGO_MAX_POOL_SIZE", 20),
            mongo_min_pool_size: var("MONGO_MIN_POOL_SIZE", 2),
            mongo_max_idle_time: Duration::from_secs(var("MONGO_MAX_IDLE_SECS", 300)),
            mongo_connect_timeout: Duration::from_millis(var("MONGO_CONNECT_TIMEOUT_MS", 5_000)),
            mongo_server_selection_timeout: Duration::from_millis(var(
                "MONGO_SERVER_SELECTION_TIMEOUT_MS",
                3_000,
            )),
            op_timeout: Duration::from_millis(var("MONGO_OP_TIMEOUT_MS", 5_000)),
            retry_max_attempts: var("RETRY_MAX_ATTEMPTS", 3),
            retry_base_delay: Duration::from_millis(var("RETRY_BASE_DELAY_MS", 100)),
            retry_max_delay: Duration::from_millis(var("RETRY_MAX_DELAY_MS", 2_000)),
        }
    }
}

/// Hand-written so that credentials in `mongo_uri` never end up in logs.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("bind_addr", &self.bind_addr)
            .field("mongo_uri", &redact_uri(&self.mongo_uri))
            .field("mongo_db", &self.mongo_db)
            .field("mongo_max_pool_size", &self.mongo_max_pool_size)
            .field("mongo_min_pool_size", &self.mongo_min_pool_size)
            .field("mongo_max_idle_time", &self.mongo_max_idle_time)
            .field("mongo_connect_timeout", &self.mongo_connect_timeout)
            .field("mongo_server_selection_timeout", &self.mongo_server_selection_timeout)
            .field("op_timeout", &self.op_timeout)
            .field("retry_max_attempts", &self.retry_max_attempts)
            .field("retry_base_delay", &self.retry_base_delay)
            .field("retry_max_delay", &self.retry_max_delay)
            .finish()
    }
}

/// Replaces the `user:password@` part of a connection string with `***@`.
pub fn redact_uri(uri: &str) -> String {
    let Some(scheme_end) = uri.find("://") else {
        return uri.to_string();
    };
    let rest = &uri[scheme_end + 3..];
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{}://***@{}", &uri[..scheme_end], &rest[at + 1..]),
        None => uri.to_string(),
    }
}

fn var<T: FromStr>(key: &str, default: T) -> T {
    match env::var(key) {
        Ok(raw) => raw.parse().unwrap_or_else(|_| {
            tracing::warn!(key, raw, "invalid value for env var, using default");
            default
        }),
        Err(_) => default,
    }
}

#[cfg(test)]
mod tests {
    use super::redact_uri;

    #[test]
    fn redacts_credentials() {
        assert_eq!(
            redact_uri("mongodb://user:p%40ss@db.example.com:27017/?authSource=admin"),
            "mongodb://***@db.example.com:27017/?authSource=admin"
        );
        assert_eq!(
            redact_uri("mongodb+srv://u:p@cluster0.mongodb.net/catalog"),
            "mongodb+srv://***@cluster0.mongodb.net/catalog"
        );
    }

    #[test]
    fn leaves_uri_without_credentials_untouched() {
        assert_eq!(redact_uri("mongodb://localhost:27017"), "mongodb://localhost:27017");
        assert_eq!(redact_uri("mongodb://h/?x=a@b"), "mongodb://h/?x=a@b");
    }
}
