use rusqlite::Connection;

/// Spend is stored as a number beside the decimal cost so it can be sorted,
/// filtered and summed. Clearing the index cursors replays every session to
/// fill the token counts.
pub fn up(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "ALTER TABLE session_index ADD COLUMN input_tokens INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE session_index ADD COLUMN output_tokens INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE session_index ADD COLUMN cache_read_tokens INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE session_index ADD COLUMN total_tokens INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE session_index ADD COLUMN spend REAL NOT NULL DEFAULT 0;
         UPDATE session_index SET spend = CAST(cost AS REAL) + CAST(sub_agent_cost AS REAL);
         CREATE INDEX idx_session_index_spend
             ON session_index (tenant_id, top_level, spend DESC, session_id DESC);
         CREATE INDEX idx_session_index_turns
             ON session_index (tenant_id, top_level, turns DESC, session_id DESC);
         CREATE INDEX idx_session_index_tokens
             ON session_index (tenant_id, top_level, total_tokens DESC, session_id DESC);",
    )?;
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
