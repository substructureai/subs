use rusqlite::Connection;

pub fn up(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE auth_flows (
            link_hash      TEXT PRIMARY KEY,
            tenant_id      TEXT NOT NULL,
            connection_id  TEXT NOT NULL,
            subject        TEXT NOT NULL,
            state          TEXT NOT NULL,
            state_hash     TEXT,
            pending_secret TEXT,
            created_at     TEXT NOT NULL,
            updated_at     TEXT NOT NULL,
            expires_at     TEXT NOT NULL
        );
        CREATE INDEX idx_auth_flows_expiry ON auth_flows (expires_at);
        CREATE INDEX idx_auth_flows_oauth ON auth_flows (state_hash);",
    )
}
