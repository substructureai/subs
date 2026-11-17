use rusqlite::Connection;

const RESET: &[&str] = &[
    "session_cursors",
    "session_index",
    "engine_queue",
    "wake_schedule",
];

pub fn up(conn: &Connection) -> rusqlite::Result<()> {
    for table in RESET {
        let exists: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get(0),
        )?;
        if exists {
            conn.execute(&format!("DELETE FROM {table}"), [])?;
        }
    }
    conn.execute_batch(
        "DROP TABLE IF EXISTS session_ties;
         DROP TABLE IF EXISTS session_entries;
         DROP TABLE IF EXISTS session_snapshots;
         CREATE TABLE session_entries (
            tenant_id  TEXT NOT NULL,
            session_id TEXT NOT NULL,
            version    INTEGER NOT NULL,
            at         TEXT NOT NULL,
            commands   BLOB NOT NULL,
            PRIMARY KEY (tenant_id, session_id, version)
         ) WITHOUT ROWID;
         CREATE TABLE session_events (
            tenant_id  TEXT NOT NULL,
            session_id TEXT NOT NULL,
            seq        INTEGER NOT NULL,
            version    INTEGER NOT NULL,
            node_id    TEXT,
            parent_id  TEXT,
            kind       INTEGER,
            branch     TEXT,
            prev       TEXT,
            answers    TEXT,
            envelope   BLOB NOT NULL,
            payload    BLOB NOT NULL,
            PRIMARY KEY (tenant_id, session_id, seq)
         );
         CREATE INDEX idx_session_events_version
            ON session_events (tenant_id, session_id, version);
         CREATE UNIQUE INDEX idx_session_events_node
            ON session_events (tenant_id, session_id, node_id) WHERE node_id IS NOT NULL;
         CREATE INDEX idx_session_events_children
            ON session_events (tenant_id, session_id, parent_id, branch, seq)
            WHERE parent_id IS NOT NULL;
         CREATE INDEX idx_session_events_branch
            ON session_events (tenant_id, session_id, branch, seq) WHERE branch IS NOT NULL;
         CREATE INDEX idx_session_events_versions
            ON session_events (tenant_id, session_id, prev, branch, seq) WHERE kind = 0;
         CREATE INDEX idx_session_events_answers
            ON session_events (tenant_id, session_id, answers, seq) WHERE answers IS NOT NULL;
         CREATE TABLE session_branches (
            tenant_id  TEXT NOT NULL,
            session_id TEXT NOT NULL,
            branch_id  TEXT NOT NULL,
            seq        INTEGER NOT NULL,
            started    BLOB NOT NULL,
            closed     BLOB,
            PRIMARY KEY (tenant_id, session_id, branch_id)
         ) WITHOUT ROWID;
         CREATE TABLE session_heads (
            tenant_id  TEXT NOT NULL,
            session_id TEXT NOT NULL,
            version    INTEGER NOT NULL,
            seq        INTEGER NOT NULL,
            PRIMARY KEY (tenant_id, session_id)
         ) WITHOUT ROWID;
         CREATE TABLE session_commands (
            tenant_id  TEXT NOT NULL,
            session_id TEXT NOT NULL,
            command_id TEXT NOT NULL,
            at         TEXT NOT NULL,
            result     BLOB NOT NULL,
            PRIMARY KEY (tenant_id, session_id, command_id)
         ) WITHOUT ROWID;
         CREATE TABLE session_snapshots (
            tenant_id  TEXT NOT NULL,
            session_id TEXT NOT NULL,
            version    INTEGER NOT NULL,
            body       BLOB NOT NULL,
            PRIMARY KEY (tenant_id, session_id)
         ) WITHOUT ROWID;",
    )
}
