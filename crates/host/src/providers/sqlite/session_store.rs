use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use quick_cache::sync::Cache;
use quick_cache::{Equivalent, Weighter};
use rusqlite::{params, OptionalExtension, Row, Transaction, TransactionBehavior};
use substructure_session::delivery::Start;
use substructure_session::events::EventPayload;
use substructure_session::state::BranchId;
use substructure_session::tree::{index, Fact, Node, NodeRef, Nodes, Stored};
use substructure_session::{
    CommandId, CommandResult, Entry, Session, SessionEvent, Store, StoreError, Tail,
};

use super::SqliteDb;
use crate::runtime::sessions::{SessionKey, SessionStores, SharedStore};

const NODE_CACHE_BYTES: u64 = 64 << 20;

#[derive(Clone)]
pub struct SqliteSessionStores {
    db: SqliteDb,
    nodes: Arc<NodeCache>,
}

impl SqliteSessionStores {
    pub fn new(db: SqliteDb) -> Self {
        Self::with_node_cache(db, NODE_CACHE_BYTES)
    }

    pub fn with_node_cache(db: SqliteDb, bytes: u64) -> Self {
        Self {
            db,
            nodes: Arc::new(Cache::with_weighter(4096, bytes, ByBytes)),
        }
    }
}

impl SessionStores for SqliteSessionStores {
    fn open(&self, tenant_id: &str, session_id: &str) -> SharedStore {
        Arc::new(SqliteSessionStore {
            db: self.db.clone(),
            nodes: self.nodes.clone(),
            session: Arc::from(format!("{tenant_id}\0{session_id}")),
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
        let conn = self.db.reader.open()?;
        let mut stmt = conn
            .prepare(
                "SELECT h.tenant_id, h.session_id FROM session_heads h
                  WHERE h.seq > 0 AND EXISTS (
                        SELECT 1 FROM json_each(?1) f
                          LEFT JOIN session_cursors c
                            ON c.tenant_id = h.tenant_id
                           AND c.session_id = h.session_id
                           AND c.consumer = f.key
                         WHERE COALESCE(c.seq, CASE WHEN f.value THEN h.seq ELSE 0 END) < h.seq)",
            )
            .map_err(unavailable)?;
        let rows = stmt
            .query_map(params![followers], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(unavailable)?;
        rows.collect::<Result<_, _>>().map_err(unavailable)
    }
}

type NodeCache = Cache<NodeKey, Cached, ByBytes>;

#[derive(Clone)]
struct Cached {
    node: NodeRef,
    bytes: u64,
}

#[derive(Clone)]
struct ByBytes;

impl Weighter<NodeKey, Cached> for ByBytes {
    fn weight(&self, _: &NodeKey, cached: &Cached) -> u64 {
        cached.bytes
    }
}

#[derive(PartialEq, Eq)]
struct NodeKey {
    session: Arc<str>,
    node: String,
}

impl Hash for NodeKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        NodeKeyRef {
            session: &self.session,
            node: &self.node,
        }
        .hash(state);
    }
}

struct NodeKeyRef<'a> {
    session: &'a str,
    node: &'a str,
}

impl Hash for NodeKeyRef<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.session.hash(state);
        self.node.hash(state);
    }
}

impl Equivalent<NodeKey> for NodeKeyRef<'_> {
    fn equivalent(&self, key: &NodeKey) -> bool {
        *key.session == *self.session && key.node == self.node
    }
}

#[derive(Clone)]
pub struct SqliteSessionStore {
    db: SqliteDb,
    nodes: Arc<NodeCache>,
    session: Arc<str>,
    tenant_id: String,
    session_id: String,
}

impl SqliteSessionStore {
    fn reader(&self) -> Result<super::Reader, StoreError> {
        self.db
            .reader
            .open()
            .map_err(|e| StoreError::Unavailable(e.to_string()))
    }

    fn cached(&self, id: &str) -> Option<NodeRef> {
        let key = NodeKeyRef {
            session: &self.session,
            node: id,
        };
        self.nodes.get(&key).map(|c| c.node)
    }

    fn keep(&self, node: NodeRef, bytes: u64) -> NodeRef {
        let key = NodeKey {
            session: self.session.clone(),
            node: node.id().to_string(),
        };
        self.nodes.insert(
            key,
            Cached {
                node: node.clone(),
                bytes,
            },
        );
        node
    }

    fn node_of(&self, seq: i64, payload: &[u8]) -> Result<Option<NodeRef>, StoreError> {
        let event = parse(payload)?;
        Ok(index(seq as u64, &event)
            .node
            .map(|node| self.keep(node, payload.len() as u64)))
    }

    fn select_nodes(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<NodeRef>, StoreError> {
        let conn = self.reader()?;
        let mut stmt = conn.prepare_cached(sql).map_err(unavailable)?;
        let rows = stmt
            .query_map(params, |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(unavailable)?;
        let mut nodes = Vec::new();
        for row in rows {
            let (seq, payload) = row.map_err(unavailable)?;
            nodes.extend(self.node_of(seq, &payload)?);
        }
        Ok(nodes)
    }

    fn select_events(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        let conn = self.reader()?;
        let mut stmt = conn.prepare_cached(sql).map_err(unavailable)?;
        let rows = stmt
            .query_map(params, |row| self.event(row))
            .map_err(unavailable)?;
        rows.map(|row| row.map_err(unavailable)?).collect()
    }

    fn event(&self, row: &Row) -> rusqlite::Result<Result<SessionEvent, StoreError>> {
        let seq: i64 = row.get(0)?;
        let envelope: Vec<u8> = row.get(1)?;
        let payload: Vec<u8> = row.get(2)?;
        Ok(joined(
            &self.tenant_id,
            &self.session_id,
            seq as u64,
            &envelope,
            &payload,
        ))
    }

    fn write_events(&self, tx: &Transaction, entry: &Entry) -> Result<(), StoreError> {
        let mut insert = tx
            .prepare_cached(
                "INSERT INTO session_events
                    (tenant_id, session_id, seq, node_id, parent_id, kind, branch,
                     prev, answers, envelope, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )
            .map_err(unavailable)?;
        let mut above = tx
            .prepare_cached(
                "SELECT CASE WHEN kind = 0 THEN node_id ELSE prev END FROM session_events
                  WHERE tenant_id = ?1 AND session_id = ?2 AND node_id = ?3",
            )
            .map_err(unavailable)?;
        for event in &entry.events {
            let (envelope, payload) = split(event)?;
            let indexed = index(event.seq, &event.payload);
            let node = indexed.node.as_deref().map(|n| &n.entry);
            let prev: Option<String> = match node.and_then(Node::parent) {
                Some(parent) => above
                    .query_row(params![self.tenant_id, self.session_id, parent], |row| {
                        row.get(0)
                    })
                    .optional()
                    .map_err(unavailable)?
                    .flatten(),
                None => None,
            };
            insert
                .execute(params![
                    self.tenant_id,
                    self.session_id,
                    event.seq as i64,
                    node.map(Node::id),
                    node.and_then(Node::parent),
                    node.map(kind),
                    indexed.branch,
                    prev,
                    indexed.answers(),
                    envelope,
                    payload,
                ])
                .map_err(unavailable)?;
        }
        Ok(())
    }

    fn write_branches(&self, tx: &Transaction, entry: &Entry) -> Result<(), StoreError> {
        for event in &entry.events {
            let indexed = index(event.seq, &event.payload);
            let key = indexed.branch.unwrap_or_default();
            if let Some(started) = indexed.started.filter(|_| !indexed.continues()) {
                tx.execute(
                    "INSERT INTO session_branches (tenant_id, session_id, branch_id, from_node)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (tenant_id, session_id, branch_id) DO UPDATE SET
                        leaf = NULL, from_node = excluded.from_node",
                    params![self.tenant_id, self.session_id, key, started.from],
                )
                .map_err(unavailable)?;
            }
            if let Some(leaf) = &indexed.leaf {
                tx.execute(
                    "INSERT INTO session_branches (tenant_id, session_id, branch_id, leaf)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (tenant_id, session_id, branch_id) DO UPDATE SET
                        leaf = excluded.leaf",
                    params![self.tenant_id, self.session_id, key, leaf],
                )
                .map_err(unavailable)?;
            }
        }
        Ok(())
    }
}

fn kind(node: &Node) -> i64 {
    match node.fact {
        Fact::Message { .. } => 0,
        Fact::State { .. } => 1,
        Fact::Config { .. } => 2,
        Fact::Reset { .. } => 4,
        Fact::Context { .. } => 3,
    }
}

fn parse(payload: &[u8]) -> Result<EventPayload, StoreError> {
    serde_json::from_slice(payload).map_err(|e| StoreError::Corrupt(e.to_string()))
}

fn split(event: &SessionEvent) -> Result<(Vec<u8>, Vec<u8>), StoreError> {
    let corrupt = |e: serde_json::Error| StoreError::Corrupt(e.to_string());
    let mut value = serde_json::to_value(event).map_err(corrupt)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| StoreError::Corrupt("an event is an object".into()))?;
    let payload = object.remove("payload").unwrap_or_default();
    for key in ["tenant_id", "session_id", "seq"] {
        object.remove(key);
    }
    Ok((
        serde_json::to_vec(&value).map_err(corrupt)?,
        serde_json::to_vec(&payload).map_err(corrupt)?,
    ))
}

fn joined(
    tenant_id: &str,
    session_id: &str,
    seq: u64,
    envelope: &[u8],
    payload: &[u8],
) -> Result<SessionEvent, StoreError> {
    let corrupt = |e: serde_json::Error| StoreError::Corrupt(e.to_string());
    let mut value: serde_json::Value = serde_json::from_slice(envelope).map_err(corrupt)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| StoreError::Corrupt("an envelope is an object".into()))?;
    object.insert(
        "payload".into(),
        serde_json::from_slice(payload).map_err(corrupt)?,
    );
    object.insert("tenant_id".into(), tenant_id.into());
    object.insert("session_id".into(), session_id.into());
    object.insert("seq".into(), seq.into());
    serde_json::from_value(value).map_err(corrupt)
}

impl Store for SqliteSessionStore {
    fn append(&self, entry: &Entry) -> Result<(), StoreError> {
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
                "SELECT version FROM session_heads WHERE tenant_id = ?1 AND session_id = ?2",
                params![self.tenant_id, self.session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(unavailable)?
            .unwrap_or(0);
        if entry.version != head as u64 + 1 {
            return Err(StoreError::VersionTaken(entry.version));
        }
        self.write_events(&tx, entry)?;
        self.write_branches(&tx, entry)?;
        for handled in &entry.commands {
            let result = serde_json::to_vec(&handled.result)
                .map_err(|e| StoreError::Corrupt(e.to_string()))?;
            let caller = serde_json::to_value(handled.caller)
                .map_err(|e| StoreError::Corrupt(e.to_string()))?;
            tx.execute(
                "INSERT OR REPLACE INTO session_commands
                    (tenant_id, session_id, command_id, version, at, kind, caller, result)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    self.tenant_id,
                    self.session_id,
                    handled.id.0,
                    entry.version as i64,
                    entry.at.to_rfc3339(),
                    handled.kind,
                    caller.as_str().unwrap_or_default(),
                    result
                ],
            )
            .map_err(unavailable)?;
        }
        tx.execute(
            "INSERT INTO session_heads (tenant_id, session_id, version, seq)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (tenant_id, session_id) DO UPDATE SET
                version = excluded.version,
                seq = MAX(session_heads.seq, excluded.seq)",
            params![
                self.tenant_id,
                self.session_id,
                entry.version as i64,
                entry.last_seq().map_or(0, |s| s as i64)
            ],
        )
        .map_err(unavailable)?;
        tx.commit().map_err(unavailable)
    }

    fn read(&self, after_seq: u64) -> Result<Tail, StoreError> {
        let conn = self.reader()?;
        let tx = conn.unchecked_transaction().map_err(unavailable)?;
        let version: i64 = tx
            .query_row(
                "SELECT version FROM session_heads WHERE tenant_id = ?1 AND session_id = ?2",
                params![self.tenant_id, self.session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(unavailable)?
            .unwrap_or(0);
        let mut stmt = tx
            .prepare_cached(
                "SELECT seq, envelope, payload FROM session_events
                  WHERE tenant_id = ?1 AND session_id = ?2 AND seq > ?3
                  ORDER BY seq",
            )
            .map_err(unavailable)?;
        let events = stmt
            .query_map(
                params![self.tenant_id, self.session_id, after_seq as i64],
                |row| self.event(row),
            )
            .map_err(unavailable)?
            .map(|row| row.map_err(unavailable)?)
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        tx.commit().map_err(unavailable)?;
        Ok(Tail {
            version: version as u64,
            events,
        })
    }

    fn events(&self, after_seq: u64, max: usize) -> Result<Vec<SessionEvent>, StoreError> {
        self.select_events(
            "SELECT seq, envelope, payload FROM session_events
              WHERE tenant_id = ?1 AND session_id = ?2 AND seq > ?3
              ORDER BY seq LIMIT ?4",
            params![
                self.tenant_id,
                self.session_id,
                after_seq as i64,
                max.min(i64::MAX as usize) as i64
            ],
        )
    }

    fn events_at(&self, seqs: &[u64]) -> Result<Vec<SessionEvent>, StoreError> {
        let list = serde_json::to_string(seqs).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        self.select_events(
            "SELECT seq, envelope, payload FROM session_events
              WHERE tenant_id = ?1 AND session_id = ?2
                AND seq IN (SELECT value FROM json_each(?3))
              ORDER BY seq",
            params![self.tenant_id, self.session_id, list],
        )
    }

    fn snapshot(&self, session: &Session) -> Result<(), StoreError> {
        let body = session.to_json()?;
        self.db
            .writer
            .lock()
            .map_err(|e| StoreError::Unavailable(e.to_string()))?
            .execute(
                "INSERT INTO session_snapshots (tenant_id, session_id, version, body)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (tenant_id, session_id) DO UPDATE SET
                    version = excluded.version, body = excluded.body
                  WHERE excluded.version > session_snapshots.version",
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
                "SELECT body FROM session_snapshots WHERE tenant_id = ?1 AND session_id = ?2",
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

    fn nodes(&self) -> Arc<dyn Nodes> {
        Arc::new(self.clone())
    }

    fn handled(
        &self,
        id: &CommandId,
    ) -> Result<Option<(CommandResult, DateTime<Utc>)>, StoreError> {
        let row: Option<(String, Vec<u8>)> = self
            .reader()?
            .query_row(
                "SELECT at, result FROM session_commands
                  WHERE tenant_id = ?1 AND session_id = ?2 AND command_id = ?3",
                params![self.tenant_id, self.session_id, id.0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(unavailable)?;
        let Some((at, result)) = row else {
            return Ok(None);
        };
        let at = super::parse_dt(&at)
            .ok_or_else(|| StoreError::Corrupt(format!("command time {at}")))?;
        let result =
            serde_json::from_slice(&result).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        Ok(Some((result, at)))
    }
}

impl Nodes for SqliteSessionStore {
    fn node(&self, id: &str) -> Result<Option<NodeRef>, StoreError> {
        if let Some(node) = self.cached(id) {
            return Ok(Some(node));
        }
        Ok(self
            .select_nodes(
                "SELECT seq, payload FROM session_events
                  WHERE tenant_id = ?1 AND session_id = ?2 AND node_id = ?3",
                params![self.tenant_id, self.session_id, id],
            )?
            .into_iter()
            .next())
    }

    fn ancestors(&self, id: &str, limit: usize) -> Result<Vec<NodeRef>, StoreError> {
        let mut found = Vec::new();
        let mut cursor = Some(id.to_string());
        while let Some(node) = cursor
            .as_deref()
            .filter(|_| found.len() < limit)
            .and_then(|id| self.cached(id))
        {
            cursor = node.parent().map(str::to_string);
            found.push(node);
        }
        let Some(missing) = cursor.filter(|_| found.len() < limit) else {
            return Ok(found);
        };
        found.extend(self.select_nodes(
            "WITH RECURSIVE up(seq, parent_id, depth) AS (
                SELECT seq, parent_id, 0 FROM session_events
                 WHERE tenant_id = ?1 AND session_id = ?2 AND node_id = ?3
                UNION ALL
                SELECT e.seq, e.parent_id, up.depth + 1 FROM up
                  JOIN session_events e
                    ON e.tenant_id = ?1 AND e.session_id = ?2 AND e.node_id = up.parent_id
                 WHERE up.depth + 1 < ?4
             )
             SELECT e.seq, e.payload FROM up
               JOIN session_events e
                 ON e.tenant_id = ?1 AND e.session_id = ?2 AND e.seq = up.seq
              ORDER BY up.depth",
            params![
                self.tenant_id,
                self.session_id,
                missing,
                (limit - found.len()) as i64
            ],
        )?);
        Ok(found)
    }

    fn children(&self, parent: &str, branch: Option<&str>) -> Result<Vec<NodeRef>, StoreError> {
        self.select_nodes(
            "SELECT seq, payload FROM session_events
              WHERE tenant_id = ?1 AND session_id = ?2 AND parent_id = ?3 AND branch IS ?4
              ORDER BY seq",
            params![self.tenant_id, self.session_id, parent, branch],
        )
    }

    fn answers(&self, tool_call_id: &str) -> Result<Vec<NodeRef>, StoreError> {
        self.select_nodes(
            "SELECT seq, payload FROM session_events
              WHERE tenant_id = ?1 AND session_id = ?2 AND answers = ?3
              ORDER BY seq",
            params![self.tenant_id, self.session_id, tool_call_id],
        )
    }

    fn stored(&self, branch: &BranchId) -> Result<Option<Stored>, StoreError> {
        self.reader()?
            .query_row(
                "SELECT leaf, from_node FROM session_branches
                  WHERE tenant_id = ?1 AND session_id = ?2 AND branch_id = ?3",
                params![
                    self.tenant_id,
                    self.session_id,
                    branch.name().unwrap_or_default()
                ],
                |row| {
                    Ok(Stored {
                        leaf: row.get(0)?,
                        from: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(unavailable)
    }

    fn versions(
        &self,
        prev: Option<&str>,
        branch: Option<&str>,
    ) -> Result<Vec<NodeRef>, StoreError> {
        self.select_nodes(
            "SELECT seq, payload FROM session_events
              WHERE tenant_id = ?1 AND session_id = ?2 AND prev IS ?3 AND branch IS ?4
                AND kind = 0
              ORDER BY seq",
            params![self.tenant_id, self.session_id, prev, branch],
        )
    }

    fn cache(&self, nodes: Vec<NodeRef>) {
        for node in nodes {
            let bytes = serde_json::to_vec(&node.entry).map_or(0, |b| b.len() as u64);
            self.keep(node, bytes);
        }
    }
}

fn unavailable(error: rusqlite::Error) -> StoreError {
    match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::ConstraintViolation) => StoreError::Corrupt(error.to_string()),
        _ => StoreError::Unavailable(error.to_string()),
    }
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
        assert_eq!(s.read(0).unwrap().version, 2);
    }

    #[test]
    fn sessions_in_one_database_keep_apart() {
        let (_dir, db) = database();
        let stores = SqliteSessionStores::new(db);
        stores.open("t", "a").append(&entry(1)).unwrap();
        stores.open("t", "b").append(&entry(1)).unwrap();
        assert_eq!(stores.open("t", "a").read(0).unwrap().version, 1);
        assert_eq!(stores.open("u", "a").read(0).unwrap().version, 0);
    }

    #[test]
    fn the_node_index_finds_what_the_log_holds() {
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
                .unwrap()
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

        let nodes = store.nodes();
        let u1 = nodes
            .node("u1")
            .unwrap()
            .expect("the message is indexed by its node");
        assert!(nodes.node("nobody").unwrap().is_none());
        let parent = u1.parent().expect("it hangs below the setup").to_string();
        let path: Vec<String> = nodes
            .ancestors("u1", 256)
            .unwrap()
            .iter()
            .map(|n| n.id().to_string())
            .collect();
        assert_eq!(path.first().map(String::as_str), Some("u1"));
        assert_eq!(path.get(1), Some(&parent));
        let below: Vec<String> = nodes
            .children(&parent, None)
            .unwrap()
            .iter()
            .map(|n| n.id().to_string())
            .collect();
        assert!(below.contains(&"u1".to_string()));

        let found: Vec<u64> = store
            .events_at(&[u1.seq])
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(found, vec![u1.seq]);

        let versions: Vec<String> = nodes
            .versions(None, None)
            .unwrap()
            .iter()
            .map(|n| n.id().to_string())
            .collect();
        assert_eq!(
            versions,
            vec!["u1"],
            "the first message is its only version"
        );
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

#[cfg(test)]
mod cold {
    use std::time::Duration;

    use chrono::Utc;
    use substructure_session::command::{CommandPayload, TurnTarget};
    use substructure_session::decision::{Action, BranchConfig, BranchFrom, BranchStart};
    use substructure_session::protocol::EffectKind;
    use substructure_session::protocol::{
        AgentConfig, BranchMode, ClientMessages, ClientPayload, Content, DraftMessage, Issuer,
        LlmRequest, QueueMode, Requester, RetryPolicy, Role, SessionOwner, Subject, WorkerState,
    };
    use substructure_session::request::WorkerDecisionRequest;
    use substructure_session::{Command, Settings, Work};

    use super::*;

    struct Cold {
        _dir: tempfile::TempDir,
        store: SharedStore,
        session: Session,
        n: usize,
    }

    fn draft(id: &str, role: Role, text: &str) -> DraftMessage {
        DraftMessage {
            id: Some(id.into()),
            role,
            content: Some(Content::Text(text.into())),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning: None,
        }
    }

    fn created() -> CommandPayload {
        let config: AgentConfig =
            serde_json::from_value(serde_json::json!({ "llm": "claude", "model": "m1" })).unwrap();
        CommandPayload::CreateSession {
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
            agent: Some(Box::new(config)),
            worker: None,
            events: None,
            state: None,
        }
    }

    impl Cold {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("subs.db");
            let db = SqliteDb::open(path.to_str().unwrap(), Duration::from_secs(5)).unwrap();
            let store = SqliteSessionStores::with_node_cache(db, 0).open("t", "s1");
            let session = Session::load(store.as_ref(), "s1", "t", Settings::default()).unwrap();
            let mut cold = Cold {
                _dir: dir,
                store,
                session,
                n: 0,
            };
            cold.send(created());
            cold.drain();
            cold
        }

        fn send(&mut self, payload: CommandPayload) {
            self.n += 1;
            let ran = self
                .session
                .run(
                    self.store.as_ref(),
                    Utc::now(),
                    Command::system(format!("c{}", self.n), "t", payload),
                )
                .unwrap();
            ran.result.expect("accepted");
        }

        fn asked(&self) -> Option<WorkerDecisionRequest> {
            self.session
                .open_work()
                .unwrap()
                .into_iter()
                .find_map(|w| match w {
                    Work::AskWorker { request, .. } => Some(*request),
                    _ => None,
                })
        }

        fn reply(&mut self, view: Vec<DraftMessage>, state: Option<serde_json::Value>) {
            self.reply_with(
                view,
                state,
                vec![Action::Done {
                    data: serde_json::Value::Null,
                }],
            );
        }

        fn reply_with(
            &mut self,
            view: Vec<DraftMessage>,
            state: Option<serde_json::Value>,
            actions: Vec<Action>,
        ) {
            let decision_id = self.asked().expect("a decision is open").decision_id;
            self.send(CommandPayload::SubmitWorkerDecision {
                decision_id,
                transcript: view,
                actions,
                state: state.map(WorkerState::from),
                agent: None,
            });
            self.drain();
        }

        fn drain(&mut self) {
            for _ in 0..10 {
                let Some(request) = self.asked() else { return };
                self.send(CommandPayload::SubmitWorkerDecision {
                    decision_id: request.decision_id,
                    transcript: vec![],
                    actions: vec![],
                    state: None,
                    agent: None,
                });
            }
        }

        fn view(&mut self, messages: Vec<DraftMessage>) {
            self.send(CommandPayload::SubmitClientPayload {
                payload: ClientPayload::Messages(ClientMessages {
                    messages,
                    stream: false,
                    client: Default::default(),
                }),
                turn: TurnTarget::Open(format!("v{}", self.n)),
                queue: QueueMode::Refuse,
                metadata: Default::default(),
            });
        }

        fn say(&mut self, text: &str) {
            self.view(
                [
                    self.path(),
                    vec![DraftMessage {
                        id: None,
                        ..draft("", Role::User, text)
                    }],
                ]
                .concat(),
            );
        }

        fn path(&self) -> Vec<DraftMessage> {
            self.session
                .view()
                .on(&BranchId::MAIN)
                .transcript()
                .into_iter()
                .map(DraftMessage::from)
                .collect()
        }

        fn ids(&self) -> Vec<String> {
            self.path().into_iter().filter_map(|m| m.id).collect()
        }

        fn reloaded(&self) -> Session {
            let loaded =
                Session::load(self.store.as_ref(), "s1", "t", Settings::default()).unwrap();
            assert_eq!(
                serde_json::to_value(loaded.open_work().unwrap()).unwrap(),
                serde_json::to_value(self.session.open_work().unwrap()).unwrap(),
                "the work built from a reload is the work built live"
            );
            loaded
        }
    }

    #[test]
    fn a_command_written_without_events_reloads() {
        let mut cold = Cold::new();
        let before = cold.session.version();
        let ran = cold
            .session
            .run(
                cold.store.as_ref(),
                Utc::now(),
                Command::system("again", "t", created()),
            )
            .unwrap();
        assert!(ran.result.is_err());
        assert!(ran.written.is_some_and(|e| e.events.is_empty()));
        assert_eq!(cold.session.version(), before + 1);
        let loaded = Session::load(cold.store.as_ref(), "s1", "t", Settings::default()).unwrap();
        assert_eq!(loaded.version(), cold.session.version());
        assert_eq!(
            serde_json::to_value(loaded.view()).unwrap(),
            serde_json::to_value(cold.session.view()).unwrap()
        );
        assert!(cold.store.handled(&"again".into()).unwrap().is_some());
    }

    #[test]
    fn an_edit_and_a_switch_read_the_log_with_nothing_cached() {
        let mut c = Cold::new();
        let (u1, a1) = (
            draft("u1", Role::User, "hi"),
            draft("a1", Role::Assistant, "hello"),
        );
        let (u2, a2) = (
            draft("u2", Role::User, "more"),
            draft("a2", Role::Assistant, "ok"),
        );
        c.view(vec![u1.clone()]);
        c.reply(
            vec![u1.clone(), a1.clone()],
            Some(serde_json::json!({ "turns": 1 })),
        );
        c.view(vec![u1.clone(), a1.clone(), u2.clone()]);
        c.reply(
            vec![u1.clone(), a1.clone(), u2.clone(), a2.clone()],
            Some(serde_json::json!({ "turns": 2 })),
        );
        assert_eq!(c.ids(), ["u1", "a1", "u2", "a2"]);

        let edit = vec![u1.clone(), a1.clone(), draft("e2", Role::User, "edited")];
        c.view(edit.clone());
        let asked = c.asked().expect("the edit asks the worker");
        assert_eq!(asked.state.0, serde_json::json!({ "turns": 1 }));
        c.reply(
            [edit, vec![draft("b2", Role::Assistant, "sure")]].concat(),
            Some(serde_json::json!({ "turns": 2, "edited": true })),
        );
        assert_eq!(c.ids(), ["u1", "a1", "e2", "b2"]);

        let back = vec![u1, a1, u2, a2];
        c.view(back.clone());
        assert_eq!(
            c.asked().unwrap().state.0,
            serde_json::json!({ "turns": 2 })
        );
        c.reply(back, None);
        assert_eq!(c.ids(), ["u1", "a1", "u2", "a2"]);

        let loaded = c.reloaded();
        assert_eq!(
            serde_json::to_value(loaded.view()).unwrap(),
            serde_json::to_value(c.session.view()).unwrap(),
            "a load from the log is the live state"
        );
        assert_eq!(loaded.view().tree.versions("u2"), ["u2", "e2"]);
    }

    fn call(id: &str, retry: RetryPolicy) -> Action {
        Action::CallLlm {
            id: id.into(),
            llm: "claude".into(),
            request: LlmRequest {
                model: "m1".into(),
                messages: vec![],
                tools: None,
                temperature: None,
                max_completion_tokens: None,
                reasoning: None,
            },
            stream: false,
            retry,
            handler: Default::default(),
            format: None,
            branch: None,
        }
    }

    fn fail(c: &mut Cold, id: &str, attempt: u32, retryable: bool) {
        c.send(CommandPayload::settle(
            EffectKind::LlmCall,
            id,
            Some(attempt),
            substructure_session::effects::Outcome::Error(
                substructure_session::command::SettleError::new(
                    substructure_session::protocol::ErrorInfo::new(
                        substructure_session::protocol::ErrorCode::Internal,
                        "down",
                    ),
                    retryable,
                ),
            ),
        ));
    }

    #[test]
    fn a_call_retried_and_asked_again_keeps_one_context_node_each() {
        let mut c = Cold::new();
        c.say("hi");
        let retry = RetryPolicy {
            max_attempts: 2,
            backoff_base_secs: 0,
            backoff_max_secs: 0,
            ..RetryPolicy::no_retry()
        };
        c.reply_with(c.path(), None, vec![call("c1", retry)]);
        fail(&mut c, "c1", 0, true);
        c.send(CommandPayload::Wake { now: Utc::now() });
        fail(&mut c, "c1", 1, true);
        c.reply_with(c.path(), None, vec![call("c1", RetryPolicy::no_retry())]);
        fail(&mut c, "c1", 0, false);
        c.drain();

        let loaded = c.reloaded();
        assert_eq!(
            serde_json::to_value(loaded.view()).unwrap(),
            serde_json::to_value(c.session.view()).unwrap(),
            "a load from the log is the live state"
        );
        let contexts = loaded
            .view()
            .tree
            .up(loaded.view().leaf(&BranchId::MAIN))
            .filter(|n| matches!(n.fact, Fact::Context { .. }))
            .count();
        assert_eq!(contexts, 2, "one node per request, none for the retry");
    }

    #[test]
    fn a_closed_branch_with_its_own_config_reads_the_log_with_nothing_cached() {
        let mut c = Cold::new();
        c.say("hi");
        let view = c.path();
        let config: AgentConfig =
            serde_json::from_value(serde_json::json!({ "llm": "claude", "model": "m2" })).unwrap();
        c.reply_with(
            view,
            None,
            vec![Action::CallLlm {
                id: "c1".into(),
                llm: "claude".into(),
                request: LlmRequest {
                    model: "m1".into(),
                    messages: vec![],
                    tools: None,
                    temperature: None,
                    max_completion_tokens: None,
                    reasoning: None,
                },
                stream: false,
                retry: RetryPolicy::no_retry(),
                handler: Default::default(),
                format: None,
                branch: Some(BranchStart {
                    id: "side".into(),
                    from: BranchFrom::Head,
                    agent: BranchConfig::Config(Box::new(config)),
                    name: None,
                    mode: BranchMode::Detached,
                }),
            }],
        );
        c.send(CommandPayload::settle(
            EffectKind::LlmCall,
            "c1",
            Some(0),
            substructure_session::effects::Outcome::Llm(Box::new(
                serde_json::from_value(serde_json::json!({
                    "model": "m2",
                    "content": "done",
                    "finish_reason": "stop",
                }))
                .unwrap(),
            )),
        ));
        c.drain();
        assert!(
            !c.session
                .view()
                .branches
                .contains_key(&BranchId::from("side")),
            "it closed"
        );

        let stored = c
            .store
            .nodes()
            .stored(&BranchId::from("side"))
            .unwrap()
            .expect("the log keeps it");
        let at = stored.leaf.or(stored.from);
        let read = c.session.view().at(at.as_deref());
        assert_eq!(read.config().map(|a| a.model), Some("m2".into()));
        let ancestors = c
            .store
            .nodes()
            .ancestors(at.as_deref().unwrap(), 256)
            .unwrap();
        assert_eq!(
            ancestors.last().and_then(|n| n.parent()),
            None,
            "it reaches the root"
        );
    }
}

#[cfg(test)]
mod laws {
    use std::time::Duration;

    use proptest::prelude::*;
    use proptest::test_runner::FileFailurePersistence;
    use substructure_session::testing;

    use super::*;

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 128,
            failure_persistence: Some(Box::new(FileFailurePersistence::SourceParallel(
                "proptest-regressions"
            ))),
            ..ProptestConfig::default()
        })]

        #[test]
        fn a_session_reloaded_from_sqlite_is_the_session_that_ran(ops in testing::ops()) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("subs.db");
            let db = SqliteDb::open(path.to_str().unwrap(), Duration::from_secs(5)).unwrap();
            let store = SqliteSessionStores::with_node_cache(db, 0)
                .open(testing::TENANT, testing::SESSION);
            testing::check(ops, store)?;
        }
    }
}
