use rusqlite::Connection;

const SQL: &str = "
CREATE TABLE slack_files (
    tenant_id  TEXT NOT NULL,
    blob_id    TEXT NOT NULL,
    file_id    TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (tenant_id, blob_id)
);
";

pub fn up(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SQL)
}
