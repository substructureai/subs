CREATE INDEX idx_auth_flows_expiry ON auth_flows (expires_at);

CREATE INDEX idx_auth_flows_oauth ON auth_flows (state_hash);

CREATE INDEX idx_engine_queue_receipt ON engine_queue (receipt);

CREATE INDEX idx_engine_queue_topic ON engine_queue (topic, visible_at, enqueued_at);

CREATE INDEX idx_session_entries_seq
            ON session_entries (tenant_id, session_id, last_seq);

CREATE INDEX idx_session_index_last_event
  ON session_index (tenant_id, last_event_at DESC, session_id DESC);

CREATE INDEX idx_session_index_top_level_last_event
  ON session_index (tenant_id, top_level, last_event_at DESC, session_id DESC);

CREATE INDEX idx_session_index_wake_at
  ON session_index (tenant_id, wake_at);

CREATE INDEX idx_wake_schedule_wake_at ON wake_schedule (wake_at);

CREATE TABLE auth_flows (
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

CREATE TABLE blobs (
    tenant_id  TEXT NOT NULL,
    id         TEXT NOT NULL,
    mime       TEXT NOT NULL,
    name       TEXT,
    size       INTEGER NOT NULL,
    bytes      BLOB NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (tenant_id, id)
);

CREATE TABLE "connector_credentials" (
            tenant_id     TEXT NOT NULL,
            connection_id TEXT NOT NULL,
            issuer        TEXT NOT NULL,
            subject       TEXT NOT NULL,
            secret_id     TEXT NOT NULL,
            PRIMARY KEY (tenant_id, connection_id, issuer, subject)
         );

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

CREATE TABLE secrets (
            tenant_id   TEXT NOT NULL,
            id          TEXT NOT NULL,
            data        TEXT NOT NULL,
            key_version INTEGER,
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL,
            PRIMARY KEY (tenant_id, id)
        );

CREATE TABLE session_cursors (
            tenant_id  TEXT NOT NULL,
            session_id TEXT NOT NULL,
            consumer   TEXT NOT NULL,
            seq        INTEGER NOT NULL,
            PRIMARY KEY (tenant_id, session_id, consumer)
        ) WITHOUT ROWID;

CREATE TABLE session_entries (
            tenant_id  TEXT NOT NULL,
            session_id TEXT NOT NULL,
            version    INTEGER NOT NULL,
            first_seq  INTEGER,
            last_seq   INTEGER,
            body       BLOB NOT NULL,
            PRIMARY KEY (tenant_id, session_id, version)
        ) WITHOUT ROWID;

CREATE TABLE session_index (
    tenant_id       TEXT NOT NULL,
    session_id      TEXT NOT NULL,
    seq             INTEGER NOT NULL,
    first_event_at  TEXT,
    last_event_at   TEXT,
    wake_at         TEXT,
    top_level       INTEGER NOT NULL,
    agent_id        TEXT,
    cost            TEXT NOT NULL,
    sub_agent_cost  TEXT NOT NULL,
    status_json     TEXT NOT NULL,
    turn_id         TEXT,
    updated_at      TEXT NOT NULL,
    PRIMARY KEY (tenant_id, session_id)
);

CREATE TABLE session_snapshots (
            tenant_id  TEXT NOT NULL,
            session_id TEXT NOT NULL,
            version    INTEGER NOT NULL,
            body       BLOB NOT NULL,
            PRIMARY KEY (tenant_id, session_id, version)
        ) WITHOUT ROWID;

CREATE TABLE session_ties (
            tenant_id  TEXT NOT NULL,
            session_id TEXT NOT NULL,
            node_id    TEXT NOT NULL,
            seq        INTEGER NOT NULL,
            message    INTEGER NOT NULL,
            parent_id  TEXT,
            PRIMARY KEY (tenant_id, session_id, node_id, seq)
        ) WITHOUT ROWID;

CREATE TABLE slack_files (
    tenant_id  TEXT NOT NULL,
    blob_id    TEXT NOT NULL,
    file_id    TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (tenant_id, blob_id)
);

CREATE TABLE slack_turn_streams (
    tenant_id  TEXT NOT NULL,
    session_id TEXT NOT NULL,
    turn_id    TEXT NOT NULL,
    start_seq  INTEGER NOT NULL,
    started_at TEXT NOT NULL,
    ts         TEXT,
    version    INTEGER NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (tenant_id, session_id, turn_id)
);

CREATE TABLE substructure_migrations (
            version     INTEGER PRIMARY KEY,
            name        TEXT NOT NULL,
            applied_at  TEXT NOT NULL
        );

CREATE TABLE wake_schedule (
    tenant_id   TEXT NOT NULL,
    session_id  TEXT NOT NULL,
    wake_at     TEXT NOT NULL,
    updated_at  TEXT NOT NULL, version INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant_id, session_id)
)
