use rusqlite::Connection;

pub fn up(conn: &Connection) -> rusqlite::Result<()> {
    let tied: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('session_ties') WHERE name = 'message')",
        [],
        |row| row.get(0),
    )?;
    if !tied {
        return Ok(());
    }
    conn.execute_batch("ALTER TABLE session_ties DROP COLUMN message;")
}
