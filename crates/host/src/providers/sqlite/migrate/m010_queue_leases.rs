use rusqlite::Connection;

/// How many times a task went out without coming back. A task whose leases
/// keep running out is taking the process down with it.
pub fn up(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("ALTER TABLE engine_queue ADD COLUMN leases INTEGER NOT NULL DEFAULT 0;")
}
