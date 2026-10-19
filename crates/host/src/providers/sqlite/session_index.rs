use async_trait::async_trait;
use chrono::Utc;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;
use sea_query::{
    Cond, Expr, ExprTrait, Func, Iden, Order, Query, SelectStatement, SqliteQueryBuilder, Value,
};

use crate::event_store::StoreError;
use crate::runtime::session::index::SessionSort;
use crate::session::index::{
    SessionActivity, SessionCursor, SessionFilter, SessionIndexRecord, SessionIndexStore,
    SessionItem, SessionPage, SessionTotals,
};

use super::{parse_dt, sea_params, spawn_err, SqliteDb};

#[derive(Iden, Clone, Copy)]
enum SessionIndex {
    Table,
    SessionId,
    TenantId,
    Seq,
    FirstEventAt,
    LastEventAt,
    WakeAt,
    TopLevel,
    AgentId,
    Cost,
    #[iden = "sub_agent_cost"]
    SubagentCost,
    StatusJson,
    TurnId,
    Title,
    UserId,
    Turns,
    Activity,
    InputTokens,
    OutputTokens,
    CacheReadTokens,
    TotalTokens,
    Spend,
}

const CACHE_RATE: &str =
    "(CASE WHEN input_tokens > 0 THEN CAST(cache_read_tokens AS REAL) / input_tokens ELSE 0 END)";

/// What a sort orders by, which way, and whether its cursor is a number.
fn sort_spec(sort: &SessionSort) -> (Expr, Order, bool) {
    let col = |c: SessionIndex| Expr::col(c);
    match sort {
        SessionSort::LastEventDesc => (col(SessionIndex::LastEventAt), Order::Desc, false),
        SessionSort::LastEventAsc => (col(SessionIndex::LastEventAt), Order::Asc, false),
        SessionSort::FirstEventAsc => (col(SessionIndex::FirstEventAt), Order::Asc, false),
        SessionSort::FirstEventDesc => (col(SessionIndex::FirstEventAt), Order::Desc, false),
        SessionSort::WakeAtAsc => (col(SessionIndex::WakeAt), Order::Asc, false),
        SessionSort::SpendDesc => (col(SessionIndex::Spend), Order::Desc, true),
        SessionSort::SpendAsc => (col(SessionIndex::Spend), Order::Asc, true),
        SessionSort::TurnsDesc => (col(SessionIndex::Turns), Order::Desc, true),
        SessionSort::TurnsAsc => (col(SessionIndex::Turns), Order::Asc, true),
        SessionSort::TokensDesc => (col(SessionIndex::TotalTokens), Order::Desc, true),
        SessionSort::TokensAsc => (col(SessionIndex::TotalTokens), Order::Asc, true),
        SessionSort::CacheRateDesc => (Expr::cust(CACHE_RATE), Order::Desc, true),
        SessionSort::CacheRateAsc => (Expr::cust(CACHE_RATE), Order::Asc, true),
    }
}

/// The same share `CACHE_RATE` computes, for a cursor taken from a row.
fn cache_rate(cache_read: u64, input: u64) -> f64 {
    if input > 0 {
        cache_read as f64 / input as f64
    } else {
        0.0
    }
}

/// Stored beside the decimal cost so it sorts, filters and sums.
fn spend(cost: Decimal, subagent_cost: Decimal) -> f64 {
    (cost + subagent_cost).to_f64().unwrap_or(0.0)
}

/// The predicates `list` and `count` share.
fn apply_filter(q: &mut SelectStatement, filter: &SessionFilter) {
    if let Some(v) = &filter.tenant_id {
        q.and_where(Expr::col(SessionIndex::TenantId).eq(v));
    }
    if let Some(v) = &filter.session_id {
        q.and_where(Expr::col(SessionIndex::SessionId).eq(v));
    }
    if let Some(v) = &filter.agent_id {
        q.and_where(Expr::col(SessionIndex::AgentId).eq(v));
    }
    if let Some(v) = &filter.user_id {
        q.and_where(Expr::col(SessionIndex::UserId).eq(v));
    }
    if let Some(v) = filter.activity {
        q.and_where(Expr::col(SessionIndex::Activity).eq(v.as_str()));
    }
    if let Some(v) = filter
        .query
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        let escaped = v
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        q.and_where(Expr::cust_with_values(
            "title LIKE ? ESCAPE '\\'",
            [format!("%{escaped}%")],
        ));
    }
    if let Some(v) = filter.spend_gt {
        q.and_where(Expr::col(SessionIndex::Spend).gt(v));
    }
    if let Some(v) = filter.spend_lt {
        q.and_where(Expr::col(SessionIndex::Spend).lt(v));
    }
    if let Some(v) = filter.turns_gt {
        q.and_where(Expr::col(SessionIndex::Turns).gt(v));
    }
    if let Some(v) = filter.turns_lt {
        q.and_where(Expr::col(SessionIndex::Turns).lt(v));
    }
    if let Some(v) = filter.started_after {
        q.and_where(Expr::col(SessionIndex::FirstEventAt).gte(v.to_rfc3339()));
    }
    if let Some(v) = filter.started_before {
        q.and_where(Expr::col(SessionIndex::FirstEventAt).lt(v.to_rfc3339()));
    }
    if filter.top_level {
        q.and_where(Expr::col(SessionIndex::TopLevel).eq(1));
    }
}

pub struct SqliteSessionIndexStore {
    db: SqliteDb,
}

impl SqliteSessionIndexStore {
    pub fn new(db: SqliteDb) -> Result<Self, StoreError> {
        Ok(Self { db })
    }
}

#[async_trait]
impl SessionIndexStore for SqliteSessionIndexStore {
    async fn list_sessions(&self, filter: &SessionFilter) -> Result<SessionPage, StoreError> {
        let filter = filter.clone();
        let reader = self.db.reader.clone();
        tokio::task::spawn_blocking(move || {
            let conn = reader.open()?;
            do_list_sessions(&conn, &filter)
        })
        .await
        .map_err(spawn_err)?
    }

    async fn count_sessions(&self, filter: &SessionFilter) -> Result<u64, StoreError> {
        let filter = filter.clone();
        let reader = self.db.reader.clone();
        tokio::task::spawn_blocking(move || {
            let conn = reader.open()?;
            do_count_sessions(&conn, &filter)
        })
        .await
        .map_err(spawn_err)?
    }

    async fn aggregate_sessions(
        &self,
        filter: &SessionFilter,
    ) -> Result<SessionTotals, StoreError> {
        let filter = filter.clone();
        let reader = self.db.reader.clone();
        tokio::task::spawn_blocking(move || {
            let conn = reader.open()?;
            do_aggregate_sessions(&conn, &filter)
        })
        .await
        .map_err(spawn_err)?
    }

    async fn upsert_session_index(&self, record: SessionIndexRecord) -> Result<(), String> {
        let writer = self.db.writer.clone();
        tokio::task::spawn_blocking(move || {
            let conn = writer.lock().map_err(|e| e.to_string())?;
            do_upsert_session_index(&conn, record)
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

fn do_list_sessions(
    conn: &rusqlite::Connection,
    filter: &SessionFilter,
) -> Result<SessionPage, StoreError> {
    let fetch_limit = filter.limit.unwrap_or(50);

    let (order_by, order_dir, numeric) = sort_spec(&filter.sort);

    let mut q = Query::select()
        .columns([
            SessionIndex::SessionId,
            SessionIndex::TenantId,
            SessionIndex::Seq,
            SessionIndex::FirstEventAt,
            SessionIndex::LastEventAt,
            SessionIndex::WakeAt,
            SessionIndex::TopLevel,
            SessionIndex::AgentId,
            SessionIndex::Cost,
            SessionIndex::SubagentCost,
            SessionIndex::StatusJson,
            SessionIndex::TurnId,
            SessionIndex::Title,
            SessionIndex::UserId,
            SessionIndex::Turns,
            SessionIndex::Activity,
            SessionIndex::InputTokens,
            SessionIndex::OutputTokens,
            SessionIndex::CacheReadTokens,
            SessionIndex::TotalTokens,
        ])
        .from(SessionIndex::Table)
        .take();
    apply_filter(&mut q, filter);

    if let Some(ref cursor) = filter.cursor {
        let at: Value = if numeric {
            cursor.value.into()
        } else {
            cursor.at.map(|at| at.to_rfc3339()).into()
        };
        let id = cursor.session_id.clone();
        let (past, past_id) = match order_dir {
            Order::Asc => (
                order_by.clone().gt(at.clone()),
                Expr::col(SessionIndex::SessionId).gt(id),
            ),
            _ => (
                order_by.clone().lt(at.clone()),
                Expr::col(SessionIndex::SessionId).lt(id),
            ),
        };
        q.cond_where(
            Cond::any()
                .add(past)
                .add(Cond::all().add(order_by.clone().eq(at)).add(past_id)),
        );
    }

    q.order_by_expr(order_by, order_dir.clone());
    q.order_by(SessionIndex::SessionId, order_dir);
    q.limit((fetch_limit + 1) as u64);

    let (sql, values) = q.build(SqliteQueryBuilder);
    let params = sea_params(values);
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StoreError::Other(e.to_string()))?;

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<String>>(12)?,
                row.get::<_, Option<String>>(13)?,
                row.get::<_, u64>(14)?,
                row.get::<_, String>(15)?,
                row.get::<_, u64>(16)?,
                row.get::<_, u64>(17)?,
                row.get::<_, u64>(18)?,
                row.get::<_, u64>(19)?,
            ))
        })
        .map_err(|e| StoreError::Other(e.to_string()))?;

    let mut items = Vec::new();
    for row in rows {
        let (
            session_id,
            tenant_id,
            seq,
            first_event_at,
            last_event_at,
            wake_at,
            top_level,
            agent_id,
            cost,
            subagent_cost,
            status_json,
            turn_id,
            title,
            user_id,
            turns,
            activity,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            total_tokens,
        ) = row.map_err(|e| StoreError::Other(e.to_string()))?;

        let cost = cost
            .parse()
            .map_err(|e: rust_decimal::Error| StoreError::Other(e.to_string()))?;
        let subagent_cost = subagent_cost
            .parse()
            .map_err(|e: rust_decimal::Error| StoreError::Other(e.to_string()))?;
        let status =
            serde_json::from_str(&status_json).map_err(|e| StoreError::Other(e.to_string()))?;

        items.push(SessionItem {
            session_id,
            tenant_id,
            seq,
            first_event_at: first_event_at.as_deref().and_then(parse_dt),
            last_event_at: last_event_at.as_deref().and_then(parse_dt),
            wake_at: wake_at.as_deref().and_then(parse_dt),
            top_level: top_level != 0,
            agent_id,
            cost,
            subagent_cost,
            status,
            turn_id,
            title,
            user_id,
            turns,
            activity: SessionActivity::parse(&activity).unwrap_or_default(),
            input_tokens,
            output_tokens,
            cache_read_tokens,
            total_tokens,
        });
    }

    let next_cursor = if items.len() > fetch_limit {
        items.pop();
        let last = &items[items.len() - 1];
        let (at, value) = match filter.sort {
            SessionSort::LastEventDesc | SessionSort::LastEventAsc => (last.last_event_at, None),
            SessionSort::FirstEventAsc | SessionSort::FirstEventDesc => (last.first_event_at, None),
            SessionSort::WakeAtAsc => (last.wake_at, None),
            SessionSort::SpendDesc | SessionSort::SpendAsc => {
                (None, Some(spend(last.cost, last.subagent_cost)))
            }
            SessionSort::TurnsDesc | SessionSort::TurnsAsc => (None, Some(last.turns as f64)),
            SessionSort::TokensDesc | SessionSort::TokensAsc => {
                (None, Some(last.total_tokens as f64))
            }
            SessionSort::CacheRateDesc | SessionSort::CacheRateAsc => (
                None,
                Some(cache_rate(last.cache_read_tokens, last.input_tokens)),
            ),
        };
        (at.is_some() || value.is_some()).then(|| SessionCursor {
            at,
            value,
            session_id: last.session_id.clone(),
        })
    } else {
        None
    };

    Ok(SessionPage { items, next_cursor })
}

fn do_count_sessions(
    conn: &rusqlite::Connection,
    filter: &SessionFilter,
) -> Result<u64, StoreError> {
    let mut q = Query::select()
        .expr(Func::count(Expr::col(SessionIndex::SessionId)))
        .from(SessionIndex::Table)
        .take();
    apply_filter(&mut q, filter);

    let (sql, values) = q.build(SqliteQueryBuilder);
    let params = sea_params(values);
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let count: i64 = conn
        .query_row(&sql, param_refs.as_slice(), |row| row.get(0))
        .map_err(|e| StoreError::Other(e.to_string()))?;

    u64::try_from(count).map_err(|e| StoreError::Other(e.to_string()))
}

fn do_aggregate_sessions(
    conn: &rusqlite::Connection,
    filter: &SessionFilter,
) -> Result<SessionTotals, StoreError> {
    let sum =
        |col: SessionIndex| Func::coalesce([Expr::from(Func::sum(Expr::col(col))), Expr::val(0)]);
    let count_of = |activity: SessionActivity| {
        Func::coalesce([
            Expr::from(Func::sum(
                Expr::col(SessionIndex::Activity).eq(activity.as_str()),
            )),
            Expr::val(0),
        ])
    };
    let mut q = Query::select()
        .expr(Func::count(Expr::col(SessionIndex::SessionId)))
        .expr(sum(SessionIndex::Turns))
        .expr(Func::coalesce([
            Expr::from(Func::sum(Expr::col(SessionIndex::Spend))),
            Expr::val(0.0),
        ]))
        .expr(sum(SessionIndex::InputTokens))
        .expr(sum(SessionIndex::OutputTokens))
        .expr(sum(SessionIndex::CacheReadTokens))
        .expr(sum(SessionIndex::TotalTokens))
        .expr(count_of(SessionActivity::Running))
        .expr(count_of(SessionActivity::Waiting))
        .expr(count_of(SessionActivity::Failed))
        .from(SessionIndex::Table)
        .take();
    apply_filter(&mut q, filter);

    let (sql, values) = q.build(SqliteQueryBuilder);
    let params = sea_params(values);
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    conn.query_row(&sql, param_refs.as_slice(), |row| {
        Ok(SessionTotals {
            sessions: row.get(0)?,
            turns: row.get(1)?,
            spend: Decimal::from_f64(row.get::<_, f64>(2)?)
                .unwrap_or_default()
                .round_dp(6),
            input_tokens: row.get(3)?,
            output_tokens: row.get(4)?,
            cache_read_tokens: row.get(5)?,
            total_tokens: row.get(6)?,
            running: row.get(7)?,
            waiting: row.get(8)?,
            failed: row.get(9)?,
        })
    })
    .map_err(|e| StoreError::Other(e.to_string()))
}

fn do_upsert_session_index(
    conn: &rusqlite::Connection,
    record: SessionIndexRecord,
) -> Result<(), String> {
    let status_json = serde_json::to_string(&record.status).map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO session_index (
            tenant_id,
            session_id,
            seq,
            first_event_at,
            last_event_at,
            wake_at,
            top_level,
            agent_id,
            cost,
            sub_agent_cost,
            status_json,
            turn_id,
            updated_at,
            title,
            user_id,
            turns,
            activity,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            total_tokens,
            spend
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                   ?18, ?19, ?20, ?21, ?22)
         ON CONFLICT(tenant_id, session_id) DO UPDATE SET
            seq = excluded.seq,
            first_event_at = COALESCE(session_index.first_event_at, excluded.first_event_at),
            last_event_at = excluded.last_event_at,
            wake_at = excluded.wake_at,
            top_level = excluded.top_level,
            agent_id = excluded.agent_id,
            cost = excluded.cost,
            sub_agent_cost = excluded.sub_agent_cost,
            status_json = excluded.status_json,
            turn_id = excluded.turn_id,
            updated_at = excluded.updated_at,
            title = excluded.title,
            user_id = excluded.user_id,
            turns = excluded.turns,
            activity = excluded.activity,
            input_tokens = excluded.input_tokens,
            output_tokens = excluded.output_tokens,
            cache_read_tokens = excluded.cache_read_tokens,
            total_tokens = excluded.total_tokens,
            spend = excluded.spend
         WHERE excluded.seq >= session_index.seq",
        rusqlite::params![
            record.tenant_id,
            record.session_id,
            record.seq,
            record.first_event_at.map(|t| t.to_rfc3339()),
            record.last_event_at.map(|t| t.to_rfc3339()),
            record.wake_at.map(|t| t.to_rfc3339()),
            if record.top_level { 1i64 } else { 0i64 },
            record.agent_id,
            record.cost.to_string(),
            record.subagent_cost.to_string(),
            status_json,
            record.turn_id,
            Utc::now().to_rfc3339(),
            record.title,
            record.user_id,
            record.turns,
            record.activity.as_str(),
            record.input_tokens,
            record.output_tokens,
            record.cache_read_tokens,
            record.total_tokens,
            spend(record.cost, record.subagent_cost),
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, TimeZone, Utc};
    use uuid::Uuid;

    use super::*;
    use crate::runtime::session::state::SessionStatus;

    fn store() -> SqliteSessionIndexStore {
        let path = std::env::temp_dir().join(format!("session-index-{}.db", Uuid::now_v7()));
        let db = SqliteDb::open(path.to_str().unwrap(), Duration::from_secs(5)).unwrap();
        SqliteSessionIndexStore::new(db).unwrap()
    }

    fn at(minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, minute, 0).unwrap()
    }

    fn record(
        id: &str,
        minute: u32,
        title: &str,
        user: &str,
        activity: SessionActivity,
    ) -> SessionIndexRecord {
        SessionIndexRecord {
            tenant_id: "t".into(),
            session_id: id.into(),
            seq: 1,
            first_event_at: Some(at(minute)),
            last_event_at: Some(at(60 - minute)),
            wake_at: None,
            top_level: true,
            agent_id: Some("support".into()),
            cost: Default::default(),
            subagent_cost: Default::default(),
            status: SessionStatus::Idle,
            turn_id: None,
            title: Some(title.into()),
            user_id: Some(user.into()),
            turns: 2,
            activity,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            total_tokens: 0,
        }
    }

    fn filter() -> SessionFilter {
        SessionFilter {
            tenant_id: Some("t".into()),
            top_level: true,
            ..Default::default()
        }
    }

    async fn ids(store: &SqliteSessionIndexStore, filter: SessionFilter) -> Vec<String> {
        let page = store.list_sessions(&filter).await.unwrap();
        page.items.into_iter().map(|i| i.session_id).collect()
    }

    #[tokio::test]
    async fn filters_by_activity_user_and_title() {
        let store = store();
        for r in [
            record("a", 1, "Where is my order?", "u1", SessionActivity::Running),
            record("b", 2, "Refund 100% please", "u2", SessionActivity::Failed),
            record("c", 3, "where IS the manual", "u1", SessionActivity::Idle),
        ] {
            store.upsert_session_index(r).await.unwrap();
        }

        let running = SessionFilter {
            activity: Some(SessionActivity::Running),
            ..filter()
        };
        assert_eq!(ids(&store, running).await, ["a"]);

        let u1 = SessionFilter {
            user_id: Some("u1".into()),
            ..filter()
        };
        assert_eq!(ids(&store, u1).await, ["a", "c"]);

        let where_ = SessionFilter {
            query: Some("WHERE is".into()),
            ..filter()
        };
        assert_eq!(ids(&store, where_).await, ["a", "c"]);

        // A wildcard in the query is matched literally.
        let percent = SessionFilter {
            query: Some("100%".into()),
            ..filter()
        };
        assert_eq!(ids(&store, percent).await, ["b"]);
        let bare = SessionFilter {
            query: Some("%".into()),
            ..filter()
        };
        assert_eq!(ids(&store, bare).await, ["b"]);

        let item = store
            .list_sessions(&filter())
            .await
            .unwrap()
            .items
            .remove(0);
        assert_eq!(item.title.as_deref(), Some("Where is my order?"));
        assert_eq!(item.user_id.as_deref(), Some("u1"));
        assert_eq!(item.turns, 2);
        assert_eq!(item.activity, SessionActivity::Running);
        assert_eq!(
            store
                .count_sessions(&SessionFilter {
                    user_id: Some("u2".into()),
                    ..filter()
                })
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn pages_by_the_column_it_sorts_on() {
        let store = store();
        for (id, minute) in [("a", 1), ("b", 2), ("c", 3)] {
            store
                .upsert_session_index(record(id, minute, "t", "u", SessionActivity::Idle))
                .await
                .unwrap();
        }

        let first = SessionFilter {
            sort: SessionSort::FirstEventAsc,
            limit: Some(2),
            ..filter()
        };
        let page = store.list_sessions(&first).await.unwrap();
        assert_eq!(
            page.items
                .iter()
                .map(|i| i.session_id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );

        let rest = SessionFilter {
            cursor: page.next_cursor,
            ..first
        };
        assert_eq!(ids(&store, rest).await, ["c"]);
    }

    fn spent(id: &str, minute: u32, dollars: &str, turns: u64, tokens: u64) -> SessionIndexRecord {
        SessionIndexRecord {
            cost: dollars.parse().unwrap(),
            turns,
            input_tokens: tokens / 2,
            cache_read_tokens: tokens / 4,
            output_tokens: tokens / 2,
            total_tokens: tokens,
            ..record(id, minute, id, "u", SessionActivity::Idle)
        }
    }

    #[tokio::test]
    async fn pages_by_spend_and_filters_by_ranges() {
        let store = store();
        for r in [
            spent("a", 1, "0.5", 1, 100),
            SessionIndexRecord {
                cache_read_tokens: 0,
                ..spent("b", 2, "2.25", 4, 900)
            },
            spent("c", 3, "0.5", 2, 300),
            spent("d", 4, "1", 3, 500),
        ] {
            store.upsert_session_index(r).await.unwrap();
        }

        let by_spend = SessionFilter {
            sort: SessionSort::SpendDesc,
            limit: Some(2),
            ..filter()
        };
        let page = store.list_sessions(&by_spend).await.unwrap();
        assert_eq!(
            page.items
                .iter()
                .map(|i| i.session_id.as_str())
                .collect::<Vec<_>>(),
            ["b", "d"]
        );
        let rest = SessionFilter {
            cursor: page.next_cursor,
            ..by_spend
        };
        // A tie in spend is broken by id, and the cursor carries across it.
        assert_eq!(ids(&store, rest).await, ["c", "a"]);

        let rate = SessionFilter {
            sort: SessionSort::CacheRateDesc,
            limit: Some(1),
            ..filter()
        };
        let mut seen = Vec::new();
        let mut page = store.list_sessions(&rate).await.unwrap();
        loop {
            seen.extend(page.items.iter().map(|i| i.session_id.clone()));
            let Some(cursor) = page.next_cursor else {
                break;
            };
            page = store
                .list_sessions(&SessionFilter {
                    cursor: Some(cursor),
                    ..rate.clone()
                })
                .await
                .unwrap();
        }
        // `b` read nothing from cache; the others tie at half, which falls back
        // to id, and paging one at a time still visits each once.
        assert_eq!(seen, ["d", "c", "a", "b"]);

        let tokens = SessionFilter {
            sort: SessionSort::TokensAsc,
            ..filter()
        };
        assert_eq!(ids(&store, tokens).await, ["a", "c", "d", "b"]);

        let mid = SessionFilter {
            spend_gt: Some(0.5),
            turns_lt: Some(4),
            ..filter()
        };
        assert_eq!(ids(&store, mid).await, ["d"]);

        let late = SessionFilter {
            started_after: Some(at(3)),
            ..filter()
        };
        assert_eq!(ids(&store, late).await, ["c", "d"]);
    }

    #[tokio::test]
    async fn totals_follow_the_filter() {
        let store = store();
        for r in [
            spent("a", 1, "0.5", 1, 100),
            spent("b", 2, "2.25", 4, 900),
            spent("c", 3, "0.25", 2, 300),
        ] {
            store.upsert_session_index(r).await.unwrap();
        }
        store
            .upsert_session_index(record("x", 5, "t", "u", SessionActivity::Failed))
            .await
            .unwrap();

        let all = store.aggregate_sessions(&filter()).await.unwrap();
        assert_eq!(all.sessions, 4);
        assert_eq!(all.turns, 9);
        assert_eq!(all.spend, "3".parse::<Decimal>().unwrap());
        assert_eq!(all.total_tokens, 1300);
        assert_eq!(all.cache_read_tokens, 25 + 225 + 75);
        assert_eq!(all.failed, 1);

        let cheap = store
            .aggregate_sessions(&SessionFilter {
                spend_lt: Some(1.0),
                ..filter()
            })
            .await
            .unwrap();
        assert_eq!((cheap.sessions, cheap.total_tokens), (3, 400));

        let none = store
            .aggregate_sessions(&SessionFilter {
                user_id: Some("nobody".into()),
                ..filter()
            })
            .await
            .unwrap();
        assert_eq!(
            (none.sessions, none.turns, none.spend),
            (0, 0, Decimal::ZERO)
        );
    }

    #[test]
    fn activity_ranks_a_running_turn_first() {
        let interrupted = SessionStatus::Interrupted {
            interrupt_id: "i".into(),
            origin: crate::protocol::InterruptOrigin::Machine,
            reason: "r".into(),
        };
        assert_eq!(
            SessionActivity::of(&interrupted, true, true),
            SessionActivity::Running
        );
        assert_eq!(
            SessionActivity::of(&interrupted, false, true),
            SessionActivity::Waiting
        );
        assert_eq!(
            SessionActivity::of(&SessionStatus::Done, false, true),
            SessionActivity::Failed
        );
        assert_eq!(
            SessionActivity::of(&SessionStatus::Done, false, false),
            SessionActivity::Done
        );
        assert_eq!(
            SessionActivity::of(&SessionStatus::Idle, false, false),
            SessionActivity::Idle
        );
    }
}
