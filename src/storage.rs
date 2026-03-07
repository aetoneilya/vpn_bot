use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::state::PendingCreateRequest;

pub struct SqliteStore {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertPendingResult {
    Created(u64),
    Existing(u64),
}

impl SqliteStore {
    fn conn_lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| anyhow::anyhow!("sqlite mutex poisoned"))
    }

    pub fn new(db_path: &str) -> Result<Self> {
        log::info!("opening sqlite database at {}", db_path);
        let conn = Connection::open(db_path)
            .with_context(|| format!("failed to open sqlite db: {db_path}"))?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS pending_requests (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                requester_chat_id INTEGER NOT NULL,
                requester_user_id INTEGER NOT NULL,
                custom_email TEXT,
                created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_pending_requests_requester_user_id
            ON pending_requests (requester_user_id);
            "#,
        )
        .context("failed to initialize sqlite schema")?;
        log::info!("sqlite schema initialized");

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn insert_request(&self, request: &PendingCreateRequest) -> Result<InsertPendingResult> {
        let conn = self.conn_lock()?;
        conn.execute(
            r#"
            INSERT OR IGNORE INTO pending_requests (requester_chat_id, requester_user_id, custom_email)
            VALUES (?1, ?2, ?3)
            "#,
            params![
                request.requester_chat_id.0,
                request.requester_user_id as i64,
                request.custom_email.as_deref()
            ],
        )
        .context("failed to insert pending request")?;

        if conn.changes() > 0 {
            let id = conn.last_insert_rowid() as u64;
            log::info!(
                "sqlite insert pending_request id={} requester_user_id={}",
                id,
                request.requester_user_id
            );
            return Ok(InsertPendingResult::Created(id));
        }

        let existing_id = conn
            .query_row(
                "SELECT id FROM pending_requests WHERE requester_user_id = ?1 LIMIT 1",
                params![request.requester_user_id as i64],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .context("failed to query existing pending request by user id")?
            .map(|v| v as u64)
            .context("pending request was not inserted and existing request not found")?;

        log::info!(
            "sqlite pending_request already exists id={} requester_user_id={}",
            existing_id,
            request.requester_user_id
        );
        Ok(InsertPendingResult::Existing(existing_id))
    }

    pub fn take_request(&self, request_id: u64) -> Result<Option<PendingCreateRequest>> {
        let mut conn = self.conn_lock()?;
        let tx = conn.transaction().context("failed to start sqlite tx")?;

        let row = {
            let mut stmt = tx
                .prepare(
                    r#"
                    SELECT requester_chat_id, requester_user_id, custom_email
                    FROM pending_requests
                    WHERE id = ?1
                    "#,
                )
                .context("failed to prepare select pending request")?;

            stmt.query_row(params![request_id as i64], |r| {
                Ok(PendingCreateRequest {
                    requester_chat_id: teloxide::types::ChatId(r.get::<_, i64>(0)?),
                    requester_user_id: r.get::<_, i64>(1)? as u64,
                    custom_email: r.get::<_, Option<String>>(2)?,
                })
            })
            .optional()
            .context("failed to query pending request")?
        };

        if row.is_some() {
            tx.execute(
                "DELETE FROM pending_requests WHERE id = ?1",
                params![request_id as i64],
            )
            .context("failed to delete pending request")?;
            log::info!("sqlite take pending_request id={}", request_id);
        } else {
            log::debug!("sqlite pending_request id={} not found", request_id);
        }

        tx.commit().context("failed to commit sqlite tx")?;
        Ok(row)
    }

    pub fn list_requests(&self) -> Result<Vec<StoredPendingRequest>> {
        let conn = self.conn_lock()?;
        let mut stmt = conn
            .prepare(
                r#"
                SELECT id, requester_chat_id, requester_user_id, custom_email, created_at
                FROM pending_requests
                ORDER BY id ASC
                "#,
            )
            .context("failed to prepare list pending requests")?;

        let rows = stmt
            .query_map([], |r| {
                Ok(StoredPendingRequest {
                    id: r.get::<_, i64>(0)? as u64,
                    request: PendingCreateRequest {
                        requester_chat_id: teloxide::types::ChatId(r.get::<_, i64>(1)?),
                        requester_user_id: r.get::<_, i64>(2)? as u64,
                        custom_email: r.get::<_, Option<String>>(3)?,
                    },
                    created_at_unix: r.get::<_, i64>(4)?,
                })
            })
            .context("failed to map pending requests")?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("failed to read pending request row")?);
        }
        Ok(out)
    }
}

#[derive(Debug, Clone)]
pub struct StoredPendingRequest {
    pub id: u64,
    pub request: PendingCreateRequest,
    pub created_at_unix: i64,
}
