use crate::runtime::event_store::StoreError;
use crate::runtime::session::state::SessionStatus;
use async_trait::async_trait;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSort {
    #[default]
    LastEventDesc,
    FirstEventAsc,
    FirstEventDesc,
    WakeAtAsc,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionFilter {
    pub tenant_id: Option<String>,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    #[serde(default)]
    pub top_level: bool,
    #[serde(default)]
    pub sort: SessionSort,
    pub limit: Option<usize>,
    pub cursor: Option<SessionCursor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCursor {
    pub at: DateTime<Utc>,
    pub session_id: String,
}

impl SessionCursor {
    pub fn encode(&self) -> Result<String, String> {
        let json = serde_json::to_string(self).map_err(|e| e.to_string())?;
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json.as_bytes()))
    }

    pub fn decode(encoded: &str) -> Result<Self, String> {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|e| format!("invalid cursor encoding: {e}"))?;
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid cursor: {e}"))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionPage {
    pub items: Vec<SessionItem>,
    pub next_cursor: Option<SessionCursor>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionItem {
    pub session_id: String,
    pub tenant_id: String,
    pub seq: u64,
    pub first_event_at: Option<DateTime<Utc>>,
    pub last_event_at: Option<DateTime<Utc>>,
    pub wake_at: Option<DateTime<Utc>>,
    pub top_level: bool,
    pub agent_id: Option<String>,
    pub cost: Decimal,
    pub subagent_cost: Decimal,
    pub status: SessionStatus,
    pub turn_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionIndexRecord {
    pub tenant_id: String,
    pub session_id: String,
    pub seq: u64,
    pub first_event_at: Option<DateTime<Utc>>,
    pub last_event_at: Option<DateTime<Utc>>,
    pub wake_at: Option<DateTime<Utc>>,
    pub top_level: bool,
    pub agent_id: Option<String>,
    pub cost: Decimal,
    pub subagent_cost: Decimal,
    pub status: SessionStatus,
    pub turn_id: Option<String>,
}

#[async_trait]
pub trait SessionIndexStore: Send + Sync {
    async fn list_sessions(&self, filter: &SessionFilter) -> Result<SessionPage, StoreError>;
    async fn count_sessions(&self, filter: &SessionFilter) -> Result<u64, StoreError>;
    async fn upsert_session_index(&self, record: SessionIndexRecord) -> Result<(), String>;
}
