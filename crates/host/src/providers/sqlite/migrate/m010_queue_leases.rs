use rusqlite::Connection;

pub fn up(conn: &Connection) -> rusqlite::Result<()> {
    let queued: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'engine_queue')",
        [],
        |row| row.get(0),
    )?;
    if !queued {
        return Ok(());
    }
    conn.execute_batch("ALTER TABLE engine_queue ADD COLUMN leases INTEGER NOT NULL DEFAULT 0;")
}
