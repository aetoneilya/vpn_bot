//! Pending access requests, persisted in SQLite so they survive restarts.

use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result, anyhow};
use rusqlite::{Connection, OptionalExtension, Row, params};
use teloxide::types::ChatId;

#[derive(Debug, Clone)]
pub struct PendingRequest {
    pub id: u64,
    pub chat_id: ChatId,
    pub user_id: u64,
    pub login: String,
    pub created_at_unix: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertResult {
    Created(u64),
    AlreadyPending(u64),
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &str) -> Result<Self> {
        log::info!("opening sqlite database at {path}");
        let conn =
            Connection::open(path).with_context(|| format!("failed to open sqlite db {path}"))?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;

            CREATE TABLE IF NOT EXISTS access_requests (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                chat_id    INTEGER NOT NULL,
                user_id    INTEGER NOT NULL UNIQUE,
                login      TEXT    NOT NULL,
                created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
            );

            -- Relay <-> exit round-trip samples; rtt_ms is NULL when the relay was unreachable.
            CREATE TABLE IF NOT EXISTS latency_samples (
                ts     INTEGER NOT NULL,
                rtt_ms REAL
            );
            CREATE INDEX IF NOT EXISTS idx_latency_samples_ts ON latency_samples (ts);
            "#,
        )
        .context("failed to initialize sqlite schema")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Adds a request unless the user already has one pending.
    pub fn insert(&self, chat_id: ChatId, user_id: u64, login: &str) -> Result<InsertResult> {
        let conn = self.conn()?;
        let inserted = conn
            .execute(
                "INSERT OR IGNORE INTO access_requests (chat_id, user_id, login) VALUES (?1, ?2, ?3)",
                params![chat_id.0, user_id as i64, login],
            )
            .context("failed to insert access request")?;
        if inserted > 0 {
            return Ok(InsertResult::Created(conn.last_insert_rowid() as u64));
        }

        let existing: i64 = conn
            .query_row(
                "SELECT id FROM access_requests WHERE user_id = ?1",
                params![user_id as i64],
                |r| r.get(0),
            )
            .context("failed to look up existing access request")?;
        Ok(InsertResult::AlreadyPending(existing as u64))
    }

    pub fn get(&self, id: u64) -> Result<Option<PendingRequest>> {
        self.conn()?
            .query_row(
                "SELECT id, chat_id, user_id, login, created_at FROM access_requests WHERE id = ?1",
                params![id as i64],
                read_request,
            )
            .optional()
            .context("failed to query access request")
    }

    pub fn find_by_user(&self, user_id: u64) -> Result<Option<PendingRequest>> {
        self.conn()?
            .query_row(
                "SELECT id, chat_id, user_id, login, created_at FROM access_requests WHERE user_id = ?1",
                params![user_id as i64],
                read_request,
            )
            .optional()
            .context("failed to query access request by user")
    }

    pub fn delete(&self, id: u64) -> Result<bool> {
        let deleted = self
            .conn()?
            .execute(
                "DELETE FROM access_requests WHERE id = ?1",
                params![id as i64],
            )
            .context("failed to delete access request")?;
        Ok(deleted > 0)
    }

    pub fn list(&self) -> Result<Vec<PendingRequest>> {
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, chat_id, user_id, login, created_at FROM access_requests ORDER BY id",
            )
            .context("failed to prepare access request listing")?;
        let rows = stmt
            .query_map([], read_request)
            .context("failed to list access requests")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to read access request row")
    }

    pub fn record_latency(&self, ts: i64, rtt_ms: Option<f64>) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO latency_samples (ts, rtt_ms) VALUES (?1, ?2)",
            params![ts, rtt_ms],
        )
        .context("failed to record latency sample")?;
        Ok(())
    }

    /// Samples with `ts >= since`, oldest first.
    pub fn latency_since(&self, since: i64) -> Result<Vec<(i64, Option<f64>)>> {
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare("SELECT ts, rtt_ms FROM latency_samples WHERE ts >= ?1 ORDER BY ts")
            .context("failed to prepare latency query")?;
        let rows = stmt
            .query_map(params![since], |r| Ok((r.get(0)?, r.get(1)?)))
            .context("failed to query latency samples")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to read latency sample")
    }

    pub fn prune_latency(&self, before: i64) -> Result<()> {
        self.conn()?
            .execute("DELETE FROM latency_samples WHERE ts < ?1", params![before])
            .context("failed to prune latency samples")?;
        Ok(())
    }

    fn conn(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| anyhow!("sqlite mutex poisoned"))
    }
}

fn read_request(row: &Row<'_>) -> rusqlite::Result<PendingRequest> {
    Ok(PendingRequest {
        id: row.get::<_, i64>(0)? as u64,
        chat_id: ChatId(row.get(1)?),
        user_id: row.get::<_, i64>(2)? as u64,
        login: row.get(3)?,
        created_at_unix: row.get(4)?,
    })
}
