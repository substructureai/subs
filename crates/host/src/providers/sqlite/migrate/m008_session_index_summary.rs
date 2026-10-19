use rusqlite::Connection;

/// Clearing the index cursors makes the reader replay every session, which
/// fills the new columns from each session's current state.
pub fn up(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "ALTER TABLE session_index ADD COLUMN title TEXT;
         ALTER TABLE session_index ADD COLUMN user_id TEXT;
         ALTER TABLE session_index ADD COLUMN turns INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE session_index ADD COLUMN activity TEXT NOT NULL DEFAULT 'idle';
         CREATE INDEX idx_session_index_activity
             ON session_index (tenant_id, top_level, activity, last_event_at DESC, session_id DESC);
         CREATE INDEX idx_session_index_user
             ON session_index (tenant_id, user_id, last_event_at DESC, session_id DESC);",
    )?;
    // Absent from databases an early build of `session_logs` migrated.
    let cursors: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'session_cursors')",
        [],
        |row| row.get(0),
    )?;
    if cursors {
        conn.execute("DELETE FROM session_cursors WHERE consumer = 'index'", [])?;
    }
    Ok(())
}
