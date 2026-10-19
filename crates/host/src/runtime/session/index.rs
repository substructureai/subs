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
    LastEventAsc,
    FirstEventAsc,
    FirstEventDesc,
    WakeAtAsc,
    SpendDesc,
    SpendAsc,
    TurnsDesc,
    TurnsAsc,
    TokensDesc,
    TokensAsc,
    /// The share of input tokens read from cache. No input reads as none cached.
    CacheRateDesc,
    CacheRateAsc,
}

/// Where a session stands, as one word a list can be filtered by. Earlier
/// variants win: a running turn outranks an open interrupt, which outranks a
/// failed last turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionActivity {
    Running,
    Waiting,
    Failed,
    Done,
    #[default]
    Idle,
}

impl SessionActivity {
    pub fn of(status: &SessionStatus, running: bool, last_turn_failed: bool) -> Self {
        match status {
            _ if running => Self::Running,
            SessionStatus::Interrupted { .. } => Self::Waiting,
            _ if last_turn_failed => Self::Failed,
            SessionStatus::Done => Self::Done,
            SessionStatus::Idle => Self::Idle,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Failed => "failed",
            Self::Done => "done",
            Self::Idle => "idle",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::Running,
            Self::Waiting,
            Self::Failed,
            Self::Done,
            Self::Idle,
        ]
        .into_iter()
        .find(|a| a.as_str() == s)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionFilter {
    pub tenant_id: Option<String>,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub user_id: Option<String>,
    pub activity: Option<SessionActivity>,
    /// Matched, case-insensitively, anywhere in the title.
    pub query: Option<String>,
    /// Spend is the session's cost and its subagents', in dollars.
    pub spend_gt: Option<f64>,
    pub spend_lt: Option<f64>,
    pub turns_gt: Option<u64>,
    pub turns_lt: Option<u64>,
    pub started_after: Option<DateTime<Utc>>,
    pub started_before: Option<DateTime<Utc>>,
    #[serde(default)]
    pub top_level: bool,
    #[serde(default)]
    pub sort: SessionSort,
    pub limit: Option<usize>,
    pub cursor: Option<SessionCursor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCursor {
    /// The sort column's value on the last row: a time for the time sorts…
    #[serde(default)]
    pub at: Option<DateTime<Utc>>,
    /// …a number for the others.
    #[serde(default)]
    pub value: Option<f64>,
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
    pub title: Option<String>,
    pub user_id: Option<String>,
    pub turns: u64,
    pub activity: SessionActivity,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub total_tokens: u64,
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
    pub title: Option<String>,
    pub user_id: Option<String>,
    pub turns: u64,
    pub activity: SessionActivity,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub total_tokens: u64,
}

/// What a filtered set of sessions adds up to.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SessionTotals {
    pub sessions: u64,
    pub turns: u64,
    pub spend: Decimal,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub total_tokens: u64,
    pub running: u64,
    pub waiting: u64,
    pub failed: u64,
}

#[async_trait]
pub trait SessionIndexStore: Send + Sync {
    async fn list_sessions(&self, filter: &SessionFilter) -> Result<SessionPage, StoreError>;
    async fn count_sessions(&self, filter: &SessionFilter) -> Result<u64, StoreError>;
    async fn aggregate_sessions(&self, filter: &SessionFilter)
        -> Result<SessionTotals, StoreError>;
    async fn upsert_session_index(&self, record: SessionIndexRecord) -> Result<(), String>;
}
