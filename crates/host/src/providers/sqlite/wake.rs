use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension};

use crate::wake::{Alarm, Alarms};

use super::{parse_dt, SqliteDb};

pub struct SqliteAlarms {
    db: SqliteDb,
}

impl SqliteAlarms {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }
}

#[async_trait]
impl Alarms for SqliteAlarms {
    async fn set(
        &self,
        tenant_id: &str,
        session_id: &str,
        version: u64,
        at: Option<DateTime<Utc>>,
    ) -> Result<(), String> {
        let tenant_id = tenant_id.to_string();
        let session_id = session_id.to_string();
        let writer = self.db.writer.clone();
        tokio::task::spawn_blocking(move || {
            let conn = writer.lock().map_err(|e| e.to_string())?;
            match at {
                Some(at) => upsert(&conn, &tenant_id, &session_id, version, at),
                None => remove(&conn, &tenant_id, &session_id, version),
            }
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn due(&self, now: DateTime<Utc>, limit: usize) -> Result<Vec<Alarm>, String> {
        let reader = self.db.reader.clone();
        tokio::task::spawn_blocking(move || {
            let conn = reader.open().map_err(|e| e.to_string())?;
            due(&conn, now, limit)
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn next(&self) -> Result<Option<DateTime<Utc>>, String> {
        let reader = self.db.reader.clone();
        tokio::task::spawn_blocking(move || {
            let conn = reader.open().map_err(|e| e.to_string())?;
            next(&conn)
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

fn upsert(
    conn: &Connection,
    tenant_id: &str,
    session_id: &str,
    version: u64,
    at: DateTime<Utc>,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO wake_schedule (tenant_id, session_id, wake_at, updated_at, version)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(tenant_id, session_id) DO UPDATE SET
             wake_at = excluded.wake_at,
             updated_at = excluded.updated_at,
             version = excluded.version
         WHERE excluded.version >= wake_schedule.version",
        rusqlite::params![
            tenant_id,
            session_id,
            at.to_rfc3339(),
            Utc::now().to_rfc3339(),
            version as i64
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn remove(
    conn: &Connection,
    tenant_id: &str,
    session_id: &str,
    version: u64,
) -> Result<(), String> {
    conn.execute(
        "DELETE FROM wake_schedule
          WHERE tenant_id = ?1 AND session_id = ?2 AND version <= ?3",
        rusqlite::params![tenant_id, session_id, version as i64],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn due(conn: &Connection, now: DateTime<Utc>, limit: usize) -> Result<Vec<Alarm>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT tenant_id, session_id, wake_at
             FROM wake_schedule
             WHERE wake_at <= ?1
             ORDER BY wake_at ASC
             LIMIT ?2",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(
            rusqlite::params![now.to_rfc3339(), limit.min(i64::MAX as usize) as i64],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        let (tenant_id, session_id, at) = row.map_err(|e| e.to_string())?;
        let Some(at) = parse_dt(&at) else {
            continue;
        };
        out.push(Alarm {
            tenant_id,
            session_id,
            at,
        });
    }
    Ok(out)
}

fn next(conn: &Connection) -> Result<Option<DateTime<Utc>>, String> {
    let at = conn
        .query_row(
            "SELECT wake_at FROM wake_schedule ORDER BY wake_at ASC LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    Ok(at.as_deref().and_then(parse_dt))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn an_older_version_does_not_move_a_newer_alarm() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subs.db");
        let db = SqliteDb::open(path.to_str().unwrap(), Duration::from_secs(5)).unwrap();
        let alarms = SqliteAlarms::new(db);
        let now = Utc::now();
        let soon = now + chrono::Duration::seconds(1);

        alarms.set("t", "s1", 2, Some(now)).await.unwrap();
        alarms.set("t", "s1", 1, Some(soon)).await.unwrap();
        assert_eq!(
            alarms.next().await.unwrap().map(|a| a.timestamp()),
            Some(now.timestamp())
        );
        alarms.set("t", "s1", 1, None).await.unwrap();
        assert_eq!(alarms.due(soon, 10).await.unwrap().len(), 1);
        alarms.set("t", "s1", 3, None).await.unwrap();
        assert!(alarms.next().await.unwrap().is_none());
    }
}
