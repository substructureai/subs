pub mod auth_flows;
pub mod blob;
pub mod connector_credentials;
pub mod migrate;
pub mod queue;
mod secret;
pub mod session_index;
pub mod session_store;
pub mod wake;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};

use crate::event_store::StoreError;

pub use auth_flows::{Flow, Link, SqliteAuthFlows};
pub use blob::SqliteBlobStore;
pub use connector_credentials::SqliteCredentialStore;
pub use queue::SqliteQueue;
pub use secret::SqliteSecretStore;
pub use session_index::SqliteSessionIndexStore;
pub use session_store::{SqliteSessionStore, SqliteSessionStores};
pub use wake::SqliteAlarms;

#[derive(Clone)]
pub struct ReaderConfig {
    path: String,
    busy_timeout: Duration,
}

impl ReaderConfig {
    pub fn open(&self) -> Result<Connection, StoreError> {
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| StoreError::Other(e.to_string()))?;
        conn.busy_timeout(self.busy_timeout)
            .map_err(|e| StoreError::Other(e.to_string()))?;
        Ok(conn)
    }
}

#[derive(Clone)]
pub struct SqliteDb {
    pub writer: Arc<Mutex<Connection>>,
    pub reader: ReaderConfig,
}

impl SqliteDb {
    pub fn open(path: &str, busy_timeout: Duration) -> Result<Self, StoreError> {
        let in_memory = path == ":memory:";

        let path_str = if in_memory {
            "file::memory:?mode=memory&cache=shared".to_string()
        } else {
            path.to_string()
        };

        let open_flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI;

        let writer = Connection::open_with_flags(&path_str, open_flags)
            .map_err(|e| StoreError::Other(e.to_string()))?;

        writer
            .busy_timeout(busy_timeout)
            .map_err(|e| StoreError::Other(e.to_string()))?;

        if !in_memory {
            writer
                .pragma_update(None, "journal_mode", "WAL")
                .map_err(|e| StoreError::Other(e.to_string()))?;
        }

        let reader = ReaderConfig {
            path: path_str,
            busy_timeout,
        };

        let db = Self {
            writer: Arc::new(Mutex::new(writer)),
            reader,
        };

        let version = migrate::run(&db, migrate::CORE_LEDGER, migrate::CORE_MIGRATIONS)?;
        tracing::debug!(version, path, "opened database");

        Ok(db)
    }
}

pub(crate) fn parse_dt(s: &str) -> Option<DateTime<Utc>> {
    s.parse().ok()
}

pub(crate) fn spawn_err(e: tokio::task::JoinError) -> StoreError {
    StoreError::Unavailable(format!("spawn_blocking: {e}"))
}

pub(crate) fn sea_params(values: sea_query::Values) -> Vec<Box<dyn rusqlite::types::ToSql>> {
    values
        .0
        .into_iter()
        .map(|v| -> Box<dyn rusqlite::types::ToSql> {
            match v {
                sea_query::Value::String(s) => Box::new(s),
                sea_query::Value::Int(i) => Box::new(i),
                sea_query::Value::BigInt(i) => Box::new(i),
                sea_query::Value::BigUnsigned(u) => Box::new(u.map(|n| n as i64)),
                sea_query::Value::TinyInt(i) => Box::new(i),
                sea_query::Value::SmallInt(i) => Box::new(i),
                sea_query::Value::TinyUnsigned(u) => Box::new(u),
                sea_query::Value::SmallUnsigned(u) => Box::new(u),
                sea_query::Value::Unsigned(u) => Box::new(u),
                sea_query::Value::Float(f) => Box::new(f.map(f64::from)),
                sea_query::Value::Double(f) => Box::new(f),
                sea_query::Value::Bool(b) => Box::new(b),
                _ => Box::new(Option::<String>::None),
            }
        })
        .collect()
}
