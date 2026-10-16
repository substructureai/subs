use std::collections::BTreeMap;
use std::sync::Arc;

use rusqlite::{params, OptionalExtension, TransactionBehavior};
use substructure_session::branch::Tie;
use substructure_session::delivery::Start;
use substructure_session::events::EventPayload;
use substructure_session::protocol::NewMessage;
use substructure_session::{Entry, Session, SessionEvent, Store, StoreError};

use super::SqliteDb;
use crate::runtime::sessions::{SessionKey, SessionStores, SharedStore};

#[derive(Clone)]
pub struct SqliteSessionStores {
    db: SqliteDb,
}

impl SqliteSessionStores {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }
}

impl SessionStores for SqliteSessionStores {
    fn open(&self, tenant_id: &str, session_id: &str) -> SharedStore {
        Arc::new(SqliteSessionStore {
            db: self.db.clone(),
            tenant_id: tenant_id.to_string(),
            session_id: session_id.to_string(),
        })
    }

    fn behind(&self, readers: &[(String, Start)]) -> Result<Vec<SessionKey>, StoreError> {
        let followers: serde_json::Map<String, serde_json::Value> = readers
            .iter()
            .map(|(name, start)| (name.clone(), (*start == Start::Joined).into()))
            .collect();
        let followers = serde_json::Value::Object(followers).to_string();
        let conn = self
            .db
            .reader
            .open()
            .map_err(|e| StoreError::Unavailable(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT h.tenant_id, h.session_id
                   FROM (SELECT tenant_id, session_id, MAX(last_seq) AS head
                           FROM session_entries GROUP BY tenant_id, session_id) h
                  WHERE h.head IS NOT NULL AND EXISTS (
                        SELECT 1 FROM json_each(?1) f
                          LEFT JOIN session_cursors c
                            ON c.tenant_id = h.tenant_id
                           AND c.session_id = h.session_id
                           AND c.consumer = f.key
                         WHERE COALESCE(c.seq, CASE WHEN f.value THEN h.head ELSE 0 END) < h.head)",
            )
            .map_err(unavailable)?;
        let rows = stmt
            .query_map(params![followers], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(unavailable)?;
        rows.collect::<Result<_, _>>().map_err(unavailable)
    }
}

pub struct SqliteSessionStore {
    db: SqliteDb,
    tenant_id: String,
    session_id: String,
}

impl SqliteSessionStore {
    fn reader(&self) -> Result<rusqlite::Connection, StoreError> {
        self.db
            .reader
            .open()
            .map_err(|e| StoreError::Unavailable(e.to_string()))
    }

    fn entries(&self, sql: &str, after: u64, max: usize) -> Result<Vec<Entry>, StoreError> {
        let conn = self.reader()?;
        let mut stmt = conn.prepare_cached(sql).map_err(unavailable)?;
        let rows = stmt
            .query_map(
                params![
                    self.tenant_id,
                    self.session_id,
                    after as i64,
                    max.min(i64::MAX as usize) as i64
                ],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .map_err(unavailable)?;
        rows.map(|body| Entry::from_json(&body.map_err(unavailable)?))
            .collect()
    }
}

impl Store for SqliteSessionStore {
    fn append(&self, entry: &Entry) -> Result<(), StoreError> {
        let body = entry.to_json()?;
        let mut conn = self
            .db
            .writer
            .lock()
            .map_err(|e| StoreError::Unavailable(e.to_string()))?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(unavailable)?;
        let head: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM session_entries
                  WHERE tenant_id = ?1 AND session_id = ?2",
                params![self.tenant_id, self.session_id],
                |row| row.get(0),
            )
            .map_err(unavailable)?;
        if entry.version != head as u64 + 1 {
            return Err(StoreError::VersionTaken(entry.version));
        }
        tx.execute(
            "INSERT INTO session_entries
                (tenant_id, session_id, version, first_seq, last_seq, body)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                self.tenant_id,
                self.session_id,
                entry.version as i64,
                entry.first_seq().map(|s| s as i64),
                entry.last_seq().map(|s| s as i64),
                body
            ],
        )
        .map_err(unavailable)?;
        for tie in entry.ties() {
            tx.execute(
                "INSERT OR IGNORE INTO session_ties
                    (tenant_id, session_id, node_id, seq, message, parent_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    self.tenant_id,
                    self.session_id,
                    tie.node,
                    tie.seq as i64,
                    tie.message,
                    tie.parent
                ],
            )
            .map_err(unavailable)?;
        }
        tx.commit().map_err(unavailable)
    }

    fn tied(&self, node: &str) -> Result<Vec<Tie>, StoreError> {
        let conn = self.reader()?;
        let mut stmt = conn
            .prepare_cached(
                "SELECT seq, message, parent_id FROM session_ties
                  WHERE tenant_id = ?1 AND session_id = ?2 AND node_id = ?3
                  ORDER BY seq",
            )
            .map_err(unavailable)?;
        let rows = stmt
            .query_map(params![self.tenant_id, self.session_id, node], |row| {
                Ok(Tie {
                    node: node.to_string(),
                    seq: row.get::<_, i64>(0)? as u64,
                    message: row.get(1)?,
                    parent: row.get(2)?,
                })
            })
            .map_err(unavailable)?;
        rows.collect::<Result<_, _>>().map_err(unavailable)
    }

    fn events_at(&self, seqs: &[u64]) -> Result<Vec<SessionEvent>, StoreError> {
        let wanted: std::collections::BTreeSet<u64> = seqs.iter().copied().collect();
        let list =
            serde_json::to_string(&wanted).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        let conn = self.reader()?;
        let mut stmt = conn
            .prepare_cached(
                "SELECT body FROM session_entries e
                  WHERE e.tenant_id = ?1 AND e.session_id = ?2
                    AND EXISTS (SELECT 1 FROM json_each(?3) s
                                 WHERE s.value BETWEEN e.first_seq AND e.last_seq)
                  ORDER BY e.version",
            )
            .map_err(unavailable)?;
        let bodies = stmt
            .query_map(params![self.tenant_id, self.session_id, list], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .map_err(unavailable)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(unavailable)?;
        let mut events = Vec::new();
        for body in bodies {
            let entry = Entry::from_json(&body)?;
            events.extend(entry.events.into_iter().filter(|e| wanted.contains(&e.seq)));
        }
        Ok(events)
    }

    fn messages(&self) -> Result<Vec<NewMessage>, StoreError> {
        let conn = self.reader()?;
        let mut stmt = conn
            .prepare_cached(
                "SELECT json_extract(j.value, '$.payload')
                   FROM session_entries e, json_each(CAST(e.body AS TEXT), '$.events') j
                  WHERE e.tenant_id = ?1 AND e.session_id = ?2
                    AND json_extract(j.value, '$.payload.type') = 'message.new'
                  ORDER BY e.version, j.key",
            )
            .map_err(unavailable)?;
        let rows = stmt
            .query_map(params![self.tenant_id, self.session_id], |row| {
                row.get::<_, String>(0)
            })
            .map_err(unavailable)?;
        let mut nodes = Vec::new();
        for row in rows {
            let payload = row.map_err(unavailable)?;
            match serde_json::from_str::<EventPayload>(&payload) {
                Ok(EventPayload::NewMessage(node)) => nodes.push(node),
                Ok(_) => {}
                Err(e) => return Err(StoreError::Corrupt(e.to_string())),
            }
        }
        Ok(nodes)
    }

    fn read(&self, after_version: u64) -> Result<Vec<Entry>, StoreError> {
        self.entries(
            "SELECT body FROM session_entries
              WHERE tenant_id = ?1 AND session_id = ?2 AND version > ?3
              ORDER BY version LIMIT ?4",
            after_version,
            usize::MAX,
        )
    }

    fn read_after_seq(&self, after_seq: u64, max: usize) -> Result<Vec<Entry>, StoreError> {
        self.entries(
            "SELECT body FROM session_entries
              WHERE tenant_id = ?1 AND session_id = ?2 AND last_seq > ?3
              ORDER BY last_seq LIMIT ?4",
            after_seq,
            max,
        )
    }

    fn snapshot(&self, session: &Session) -> Result<(), StoreError> {
        let body = session.to_json()?;
        self.db
            .writer
            .lock()
            .map_err(|e| StoreError::Unavailable(e.to_string()))?
            .execute(
                "INSERT OR REPLACE INTO session_snapshots (tenant_id, session_id, version, body)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    self.tenant_id,
                    self.session_id,
                    session.version() as i64,
                    body
                ],
            )
            .map(|_| ())
            .map_err(unavailable)
    }

    fn latest(&self) -> Result<Option<Session>, StoreError> {
        let body: Option<Vec<u8>> = self
            .reader()?
            .query_row(
                "SELECT body FROM session_snapshots
                  WHERE tenant_id = ?1 AND session_id = ?2
                  ORDER BY version DESC LIMIT 1",
                params![self.tenant_id, self.session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(unavailable)?;
        body.map(|b| Session::from_json(&b)).transpose()
    }

    fn cursors(&self) -> Result<BTreeMap<String, u64>, StoreError> {
        let conn = self.reader()?;
        let mut stmt = conn
            .prepare_cached(
                "SELECT consumer, seq FROM session_cursors
                  WHERE tenant_id = ?1 AND session_id = ?2",
            )
            .map_err(unavailable)?;
        let rows = stmt
            .query_map(params![self.tenant_id, self.session_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })
            .map_err(unavailable)?;
        rows.collect::<Result<_, _>>().map_err(unavailable)
    }

    fn set_cursor(&self, consumer: &str, seq: u64) -> Result<(), StoreError> {
        self.db
            .writer
            .lock()
            .map_err(|e| StoreError::Unavailable(e.to_string()))?
            .execute(
                "INSERT INTO session_cursors (tenant_id, session_id, consumer, seq)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (tenant_id, session_id, consumer) DO UPDATE SET
                    seq = MAX(session_cursors.seq, excluded.seq)",
                params![self.tenant_id, self.session_id, consumer, seq as i64],
            )
            .map(|_| ())
            .map_err(unavailable)
    }
}

fn unavailable(error: rusqlite::Error) -> StoreError {
    StoreError::Unavailable(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::Utc;
    use substructure_session::Settings;

    use super::*;

    fn database() -> (tempfile::TempDir, SqliteDb) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subs.db");
        let db = SqliteDb::open(path.to_str().unwrap(), Duration::from_secs(5)).unwrap();
        (dir, db)
    }

    fn entry(version: u64) -> Entry {
        Entry {
            version,
            at: Utc::now(),
            commands: Vec::new(),
            events: Vec::new(),
        }
    }

    #[test]
    fn a_version_is_written_once_and_in_order() {
        let (_dir, db) = database();
        let s = SqliteSessionStores::new(db).open("t", "s1");
        s.append(&entry(1)).unwrap();
        assert_eq!(s.append(&entry(1)), Err(StoreError::VersionTaken(1)));
        assert_eq!(s.append(&entry(3)), Err(StoreError::VersionTaken(3)));
        s.append(&entry(2)).unwrap();
        let versions: Vec<u64> = s.read(0).unwrap().iter().map(|e| e.version).collect();
        assert_eq!(versions, vec![1, 2]);
        assert_eq!(s.read(1).unwrap()[0].version, 2);
    }

    #[test]
    fn sessions_in_one_database_keep_apart() {
        let (_dir, db) = database();
        let stores = SqliteSessionStores::new(db);
        stores.open("t", "a").append(&entry(1)).unwrap();
        stores.open("t", "b").append(&entry(1)).unwrap();
        assert_eq!(stores.open("t", "a").read(0).unwrap().len(), 1);
        assert!(stores.open("u", "a").read(0).unwrap().is_empty());
    }

    #[test]
    fn the_index_finds_what_a_scan_of_the_log_finds() {
        use substructure_session::command::{CommandPayload, TurnTarget};
        use substructure_session::protocol::{
            ClientMessage, ClientPayload, Content, DraftMessage, QueueMode, Role, WorkerState,
        };
        use substructure_session::{Command, Work};

        let (_dir, db) = database();
        let store = SqliteSessionStores::new(db).open("t", "s1");
        created(&store);
        let mut session = Session::load(store.as_ref(), "s1", "t", Settings::default()).unwrap();
        let system = |id: &str, payload| Command::system(id, "t", payload);
        let decide = |session: &mut Session, id: &str, transcript: Vec<DraftMessage>| {
            let decision_id = session
                .open_work()
                .into_iter()
                .find_map(|w| match w {
                    Work::AskWorker { work, .. } => Some(work.id),
                    _ => None,
                })
                .unwrap();
            session
                .run(
                    store.as_ref(),
                    chrono::Utc::now(),
                    system(
                        id,
                        CommandPayload::SubmitWorkerDecision {
                            decision_id,
                            transcript,
                            actions: vec![],
                            state: Some(WorkerState::from(serde_json::json!({ "by": id }))),
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
                store.as_ref(),
                chrono::Utc::now(),
                system(
                    "say",
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

        let indexed = store.tied("u1").unwrap();
        assert_eq!(indexed.len(), 2, "the message and the state written on it");
        assert_eq!(
            indexed,
            substructure_session::branch::scan_ties(store.as_ref(), "u1").unwrap()
        );
        assert!(store.tied("nobody").unwrap().is_empty());
        assert!(indexed.iter().any(|t| t.message && t.parent.is_none()));

        let seqs: Vec<u64> = indexed.iter().map(|t| t.seq).collect();
        let found: Vec<u64> = store
            .events_at(&seqs)
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(found, seqs);

        let scanned: Vec<String> = Session::events(store.as_ref(), 0, usize::MAX)
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.payload {
                substructure_session::events::EventPayload::NewMessage(m) => Some(m.message.id),
                _ => None,
            })
            .collect();
        let read: Vec<String> = store
            .messages()
            .unwrap()
            .into_iter()
            .map(|m| m.message.id)
            .collect();
        assert_eq!(read, scanned);
        assert_eq!(read, vec!["u1"]);
    }

    #[test]
    fn the_latest_snapshot_and_the_cursors_come_back() {
        let (_dir, db) = database();
        let s = SqliteSessionStores::new(db).open("t", "s1");
        s.snapshot(&Session::new("s1", "t", Settings::default()))
            .unwrap();
        assert_eq!(s.latest().unwrap().unwrap().id(), "s1");
        s.set_cursor("work", 4).unwrap();
        s.set_cursor("work", 2).unwrap();
        assert_eq!(s.cursors().unwrap().get("work"), Some(&4));
    }

    fn created(store: &SharedStore) -> u64 {
        use crate::protocol::{Issuer, Requester, RetryPolicy, SessionOwner, Subject};
        use substructure_session::command::CommandPayload;
        let mut session = Session::load(store.as_ref(), "s1", "t", Settings::default()).unwrap();
        let create = CommandPayload::CreateSession {
            agent_id: "agent".into(),
            owner: SessionOwner {
                tenant_id: "t".into(),
                requester: Requester::new(
                    Subject::new(Issuer::app(), "u".to_string()),
                    Default::default(),
                ),
                metadata: Default::default(),
            },
            ancestry: vec![],
            worker_retry: RetryPolicy::no_retry(),
            agent: None,
            worker: None,
            events: None,
            state: None,
        };
        session
            .run(
                store.as_ref(),
                Utc::now(),
                substructure_session::Command::system("c1", "t", create),
            )
            .unwrap();
        session.seq()
    }

    #[test]
    fn a_session_is_behind_until_each_consumer_reaches_its_head() {
        let (_dir, db) = database();
        let stores = SqliteSessionStores::new(db);
        let store = stores.open("t", "s1");
        let head = created(&store);
        let consumers = vec![
            ("work".to_string(), Start::Beginning),
            ("slack".to_string(), Start::Joined),
        ];

        assert_eq!(
            stores.behind(&consumers).unwrap(),
            vec![("t".into(), "s1".into())]
        );
        store.set_cursor("work", head).unwrap();
        assert!(stores.behind(&consumers).unwrap().is_empty());
        store.set_cursor("slack", head - 1).unwrap();
        assert_eq!(stores.behind(&consumers).unwrap().len(), 1);
    }
}
