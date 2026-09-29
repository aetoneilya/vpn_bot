//! SQLite persistence: pending access requests, latency samples and user complaints.

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

            -- User-reported problems. Location is stored as operator/region only, never the raw IP.
            CREATE TABLE IF NOT EXISTS complaints (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at    INTEGER NOT NULL,
                user_id       INTEGER NOT NULL,
                chat_id       INTEGER NOT NULL,
                login         TEXT,
                category      TEXT NOT NULL,
                profile       TEXT,
                network       TEXT,
                site          TEXT,
                comment       TEXT,
                platform      TEXT,
                client_rtt_ms REAL,
                country       TEXT,
                region        TEXT,
                city          TEXT,
                asn           INTEGER,
                operator      TEXT,
                status        TEXT NOT NULL DEFAULT 'open',
                resolved_at   INTEGER
            );
            CREATE INDEX IF NOT EXISTS idx_complaints_user ON complaints (user_id, created_at);
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

    pub fn insert_complaint(&self, c: &crate::complaints::NewComplaint) -> Result<u64> {
        let conn = self.conn()?;
        conn.execute(
            r#"INSERT INTO complaints (created_at, user_id, chat_id, login, category, profile, network,
                   site, comment, platform, client_rtt_ms, country, region, city, asn, operator)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)"#,
            params![
                chrono::Utc::now().timestamp(),
                c.user_id as i64,
                c.chat_id.0,
                c.login,
                c.category.code(),
                c.profile,
                c.network,
                c.site,
                c.comment,
                c.platform,
                c.client_rtt_ms,
                c.geo.country,
                c.geo.region,
                c.geo.city,
                c.geo.asn,
                c.geo.operator,
            ],
        )
        .context("failed to insert complaint")?;
        Ok(conn.last_insert_rowid() as u64)
    }

    pub fn complaints_since(&self, user_id: u64, since: i64) -> Result<u64> {
        let count: i64 = self
            .conn()?
            .query_row(
                "SELECT COUNT(*) FROM complaints WHERE user_id = ?1 AND created_at >= ?2",
                params![user_id as i64, since],
                |r| r.get(0),
            )
            .context("failed to count complaints")?;
        Ok(count as u64)
    }

    pub fn complaint(&self, id: u64) -> Result<Option<crate::complaints::Complaint>> {
        self.conn()?
            .query_row(
                &format!("{COMPLAINT_SELECT} WHERE id = ?1"),
                params![id as i64],
                read_complaint,
            )
            .optional()
            .context("failed to query complaint")
    }

    pub fn open_complaints(&self) -> Result<Vec<crate::complaints::Complaint>> {
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare(&format!(
                "{COMPLAINT_SELECT} WHERE status = 'open' ORDER BY id"
            ))
            .context("failed to prepare complaints listing")?;
        let rows = stmt
            .query_map([], read_complaint)
            .context("failed to list complaints")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to read complaint row")
    }

    /// Marks a complaint resolved; returns false if it was not open.
    pub fn resolve_complaint(&self, id: u64) -> Result<bool> {
        let changed = self
            .conn()?
            .execute(
                "UPDATE complaints SET status = 'resolved', resolved_at = ?2 WHERE id = ?1 AND status = 'open'",
                params![id as i64, chrono::Utc::now().timestamp()],
            )
            .context("failed to resolve complaint")?;
        Ok(changed > 0)
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

const COMPLAINT_SELECT: &str = "SELECT id, created_at, user_id, chat_id, login, category, profile, network, site, comment, platform, client_rtt_ms, country, region, city, asn, operator FROM complaints";

fn read_complaint(row: &Row<'_>) -> rusqlite::Result<crate::complaints::Complaint> {
    use crate::complaints::{Category, Complaint, NewComplaint};
    use crate::geo::GeoInfo;
    Ok(Complaint {
        id: row.get::<_, i64>(0)? as u64,
        created_at: row.get(1)?,
        details: NewComplaint {
            user_id: row.get::<_, i64>(2)? as u64,
            chat_id: ChatId(row.get(3)?),
            login: row.get(4)?,
            category: Category::from_code(&row.get::<_, String>(5)?),
            profile: row.get(6)?,
            network: row.get(7)?,
            site: row.get(8)?,
            comment: row.get(9)?,
            platform: row.get(10)?,
            client_rtt_ms: row.get(11)?,
            geo: GeoInfo {
                country: row.get(12)?,
                region: row.get(13)?,
                city: row.get(14)?,
                asn: row.get(15)?,
                operator: row.get(16)?,
            },
        },
    })
}
