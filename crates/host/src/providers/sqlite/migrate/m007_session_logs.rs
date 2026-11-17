use rusqlite::Connection;

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
