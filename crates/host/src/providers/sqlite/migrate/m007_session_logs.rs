use rusqlite::{params, Connection};
use serde_json::{json, Value};

const DELIVERED: &[&str] = &["work", "index", "mail", "events", "slack"];

pub fn up(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE session_entries (
            tenant_id  TEXT NOT NULL,
            session_id TEXT NOT NULL,
            version    INTEGER NOT NULL,
            first_seq  INTEGER,
            last_seq   INTEGER,
            body       BLOB NOT NULL,
            PRIMARY KEY (tenant_id, session_id, version)
        ) WITHOUT ROWID;
        CREATE INDEX idx_session_entries_seq
            ON session_entries (tenant_id, session_id, last_seq);
        CREATE TABLE session_snapshots (
            tenant_id  TEXT NOT NULL,
            session_id TEXT NOT NULL,
            version    INTEGER NOT NULL,
            body       BLOB NOT NULL,
            PRIMARY KEY (tenant_id, session_id, version)
        ) WITHOUT ROWID;
        CREATE TABLE session_cursors (
            tenant_id  TEXT NOT NULL,
            session_id TEXT NOT NULL,
            consumer   TEXT NOT NULL,
            seq        INTEGER NOT NULL,
            PRIMARY KEY (tenant_id, session_id, consumer)
        ) WITHOUT ROWID;
        CREATE TABLE session_ties (
            tenant_id  TEXT NOT NULL,
            session_id TEXT NOT NULL,
            node_id    TEXT NOT NULL,
            seq        INTEGER NOT NULL,
            message    INTEGER NOT NULL,
            parent_id  TEXT,
            PRIMARY KEY (tenant_id, session_id, node_id, seq)
        ) WITHOUT ROWID;",
    )?;
    move_sessions(conn)?;
    conn.execute_batch(
        "DROP TABLE events;
         DROP TABLE snapshots;
         DROP TABLE messages;
         DROP TABLE llm_prompts;
         DROP TABLE session_versions;
         DROP TABLE projection_cursors;
         DROP TABLE projection_seeds;
         ALTER TABLE wake_schedule ADD COLUMN version INTEGER NOT NULL DEFAULT 0;
         CREATE TABLE engine_queue (
            id          TEXT PRIMARY KEY,
            topic       TEXT NOT NULL,
            key         TEXT NOT NULL,
            span        TEXT NOT NULL,
            task        TEXT NOT NULL,
            enqueued_at TEXT NOT NULL,
            visible_at  TEXT NOT NULL,
            receipt     TEXT
         );
         CREATE INDEX idx_engine_queue_topic ON engine_queue (topic, visible_at, enqueued_at);
         CREATE INDEX idx_engine_queue_receipt ON engine_queue (receipt);
         INSERT INTO engine_queue (id, topic, key, span, task, enqueued_at, visible_at)
            SELECT 'decision:' || decision_id || ':' || COALESCE(json_extract(payload, '$.attempts'), 0),
                   'decisions:' || tenant_id,
                   COALESCE(json_extract(payload, '$.session_id'), ''),
                   '',
                   payload,
                   enqueued_at,
                   enqueued_at
              FROM engine_worker_queue;
         DROP TABLE engine_worker_queue;",
    )
}

fn move_sessions(conn: &Connection) -> rusqlite::Result<()> {
    let sessions: Vec<(String, String)> = conn
        .prepare("SELECT tenant_id, session_id FROM snapshots")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let mut read = conn.prepare(
        "SELECT id, seq, occurred_at, start_time, end_time, span, payload, meta
           FROM events WHERE tenant_id = ?1 AND session_id = ?2 ORDER BY seq",
    )?;
    for (tenant_id, session_id) in sessions {
        let events: Vec<Value> = read
            .query_map(params![tenant_id, session_id], |row| {
                let parsed = |i: usize| -> rusqlite::Result<Value> {
                    let text: String = row.get(i)?;
                    serde_json::from_str(&text).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            i,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })
                };
                Ok(json!({
                    "id": row.get::<_, String>(0)?,
                    "tenant_id": tenant_id,
                    "session_id": session_id,
                    "seq": row.get::<_, i64>(1)?,
                    "occurred_at": row.get::<_, String>(2)?,
                    "start_time": row.get::<_, String>(3)?,
                    "end_time": row.get::<_, String>(4)?,
                    "span": parsed(5)?,
                    "payload": parsed(6)?,
                    "meta": parsed(7)?,
                }))
            })?
            .collect::<Result<_, _>>()?;
        let (Some(first), Some(last)) = (events.first(), events.last()) else {
            continue;
        };
        let first_seq = first["seq"].as_i64();
        let last_seq = last["seq"].as_i64().unwrap_or_default();
        let at = last["occurred_at"].clone();
        let body = serde_json::to_vec(&json!({
            "version": 1,
            "at": at,
            "commands": [],
            "events": events,
        }))
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        conn.execute(
            "INSERT INTO session_entries
                (tenant_id, session_id, version, first_seq, last_seq, body)
             VALUES (?1, ?2, 1, ?3, ?4, ?5)",
            params![tenant_id, session_id, first_seq, last_seq, body],
        )?;
        for event in &events {
            let payload = &event["payload"];
            let (node, message, parent) = match payload["type"].as_str() {
                Some("message.new") => (
                    payload["message"]["id"].as_str(),
                    true,
                    payload["parent_id"].as_str(),
                ),
                Some("worker.state.updated" | "agent.updated") => {
                    (payload["anchor"].as_str(), false, None)
                }
                _ => (None, false, None),
            };
            if let (Some(node), Some(seq)) = (node, event["seq"].as_i64()) {
                conn.execute(
                    "INSERT OR IGNORE INTO session_ties
                        (tenant_id, session_id, node_id, seq, message, parent_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![tenant_id, session_id, node, seq, message, parent],
                )?;
            }
        }
        for consumer in DELIVERED {
            conn.execute(
                "INSERT INTO session_cursors (tenant_id, session_id, consumer, seq)
                 VALUES (?1, ?2, ?3, ?4)",
                params![tenant_id, session_id, consumer, last_seq],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::Utc;
    use rusqlite::Connection;
    use substructure_session::command::{CommandPayload, TurnTarget};
    use substructure_session::protocol::{
        ClientMessage, ClientPayload, Content, DraftMessage, Issuer, QueueMode, Requester,
        RetryPolicy, Role, SessionOwner, Subject,
    };
    use substructure_session::{Command, MemoryStore, Session, Settings, Store};

    use super::super::{run_on_conn, CORE_LEDGER, CORE_MIGRATIONS};
    use super::*;

    fn old_session() -> (Session, Vec<substructure_session::SessionEvent>) {
        let store = MemoryStore::new();
        let mut session = Session::load(&store, "s1", "t", Settings::default()).unwrap();
        let ran = session
            .run(
                &store,
                Utc::now(),
                Command::system(
                    "c1",
                    "t",
                    CommandPayload::CreateSession {
                        agent_id: "agent".into(),
                        owner: SessionOwner {
                            tenant_id: "t".into(),
                            requester: Requester::new(
                                Subject::new(Issuer::app(), "u".to_string()),
                                Default::default(),
                            ),
                            metadata: HashMap::new(),
                        },
                        ancestry: vec![],
                        worker_retry: RetryPolicy::no_retry(),
                        agent: None,
                        worker: None,
                        events: None,
                        state: None,
                    },
                ),
            )
            .unwrap();
        assert!(ran.written.is_some());
        let decide = |session: &mut Session, id: &str, transcript: Vec<DraftMessage>| {
            let decision_id = session
                .open_work()
                .into_iter()
                .find_map(|w| match w {
                    substructure_session::Work::AskWorker { work, .. } => Some(work.id),
                    _ => None,
                })
                .unwrap();
            session
                .run(
                    &store,
                    Utc::now(),
                    Command::system(
                        id,
                        "t",
                        CommandPayload::SubmitWorkerDecision {
                            decision_id,
                            transcript,
                            actions: vec![],
                            state: None,
                            agent: None,
                        },
                    ),
                )
                .unwrap();
        };
        decide(&mut session, "start", vec![]);
        let hi = DraftMessage {
            id: Some("u1".into()),
            role: Role::User,
            content: Some(Content::Text("hi".into())),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning: None,
        };
        session
            .run(
                &store,
                Utc::now(),
                Command::system(
                    "say",
                    "t",
                    CommandPayload::SubmitClientPayload {
                        payload: ClientPayload::Message(ClientMessage {
                            message: hi.clone(),
                            stream: false,
                        }),
                        turn: TurnTarget::Open("t1".into()),
                        queue: QueueMode::Refuse,
                        metadata: Default::default(),
                    },
                ),
            )
            .unwrap();
        decide(&mut session, "echo", vec![hi]);
        let events = Session::events(&store, 0, usize::MAX).unwrap();
        (session, events)
    }

    #[test]
    fn old_sessions_move_into_their_logs_and_the_old_tables_go() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_on_conn(&mut conn, CORE_LEDGER, &CORE_MIGRATIONS[..6]).unwrap();
        let (session, events) = old_session();
        conn.execute(
            "INSERT INTO snapshots (tenant_id, session_id, seq, shard_key, data)
             VALUES ('t', 's1', ?1, 0, '{}')",
            params![events.len() as i64],
        )
        .unwrap();
        for e in &events {
            conn.execute(
                "INSERT INTO events (id, tenant_id, session_id, seq, occurred_at, start_time,
                                     end_time, span, payload, meta)
                 VALUES (?1, 't', 's1', ?2, ?3, ?3, ?3, ?4, ?5, ?6)",
                params![
                    e.id.to_string(),
                    e.seq as i64,
                    e.occurred_at.to_rfc3339(),
                    serde_json::to_string(&e.span).unwrap(),
                    serde_json::to_string(&e.payload).unwrap(),
                    serde_json::to_string(&e.meta).unwrap(),
                ],
            )
            .unwrap();
        }

        conn.execute(
            "INSERT INTO engine_worker_queue (decision_id, tenant_id, agent_id, payload, enqueued_at)
             VALUES ('d1', 't', 'agent', '{\"session_id\":\"s1\",\"attempts\":2}', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();

        up(&conn).unwrap();

        let queued: (String, String, String) = conn
            .query_row("SELECT id, topic, key FROM engine_queue", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(
            queued,
            ("decision:d1:2".into(), "decisions:t".into(), "s1".into())
        );

        let body: Vec<u8> = conn
            .query_row(
                "SELECT body FROM session_entries WHERE tenant_id = 't' AND session_id = 's1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let store = MemoryStore::new();
        store
            .append(&substructure_session::Entry::from_json(&body).unwrap())
            .unwrap();
        let moved = Session::load(&store, "s1", "t", Settings::default()).unwrap();
        assert_eq!(
            serde_json::to_value(moved.state()).unwrap(),
            serde_json::to_value(session.state()).unwrap()
        );
        let delivered: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_cursors WHERE seq = ?1",
                params![events.len() as i64],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(delivered, DELIVERED.len() as i64);
        let entry = substructure_session::Entry::from_json(&body).unwrap();
        let mut backfilled: Vec<(String, u64, bool, Option<String>)> = conn
            .prepare("SELECT node_id, seq, message, parent_id FROM session_ties ORDER BY seq")
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, i64>(1)? as u64,
                    row.get(2)?,
                    row.get(3)?,
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let mut expected: Vec<(String, u64, bool, Option<String>)> = entry
            .ties()
            .into_iter()
            .map(|t| (t.node, t.seq, t.message, t.parent))
            .collect();
        assert!(!expected.is_empty());
        backfilled.sort();
        expected.sort();
        assert_eq!(backfilled, expected, "the index matches the moved log");
        assert!(conn.prepare("SELECT 1 FROM events").is_err());
    }
}
