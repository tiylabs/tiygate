//! Audit log — every admin write operation should leave a trace.
//!
//! The audit log is a separate table from `request_logs` so a
//! security review can read write history without trawling
//! per-request data.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use thiserror::Error;

use crate::db::DbPool;

#[derive(Debug, Error)]
pub enum AuditError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: i64,
    pub actor: String,
    pub action: String,
    pub target_type: String,
    pub target_id: String,
    pub details: serde_json::Value,
    pub ts: DateTime<Utc>,
}

/// Record an admin write operation. Best-effort: failures are
/// logged but do not fail the calling operation.
pub async fn record(
    pool: &DbPool,
    actor: &str,
    action: &str,
    target_type: &str,
    target_id: &str,
    details: &serde_json::Value,
) -> Result<i64, AuditError> {
    let now = Utc::now().to_rfc3339();
    let details_str = serde_json::to_string(details)?;
    let res = sqlx::query(
        "INSERT INTO audit_log (actor, action, target_type, target_id, details_json, ts) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(actor)
    .bind(action)
    .bind(target_type)
    .bind(target_id)
    .bind(&details_str)
    .bind(&now)
    .execute(pool.sqlite())
    .await?;
    Ok(res.last_insert_rowid())
}

/// Fetch the most recent audit entries, newest first. Used by the
/// admin UI / CLI for accountability.
pub async fn list_recent(pool: &DbPool, limit: i64) -> Result<Vec<AuditEntry>, AuditError> {
    let rows = sqlx::query(
        "SELECT id, actor, action, target_type, target_id, details_json, ts \
         FROM audit_log ORDER BY id DESC LIMIT ?1",
    )
    .bind(limit)
    .fetch_all(pool.sqlite())
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let details_str: String = row.get("details_json");
        let details: serde_json::Value = if details_str.is_empty() {
            serde_json::Value::Object(Default::default())
        } else {
            serde_json::from_str(&details_str)?
        };
        let ts_str: String = row.get("ts");
        let ts = chrono::DateTime::parse_from_rfc3339(&ts_str)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        out.push(AuditEntry {
            id: row.get("id"),
            actor: row.get("actor"),
            action: row.get("action"),
            target_type: row.get("target_type"),
            target_id: row.get("target_id"),
            details,
            ts,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[tokio::test]
    async fn record_and_list_round_trip() {
        let pool = db::open_pool("sqlite::memory:").await.expect("pool");
        db::run_migrations(pool.sqlite()).await.expect("migrate");
        let id = record(
            &pool,
            "admin",
            "create",
            "provider",
            "openai",
            &serde_json::json!({"name": "OpenAI"}),
        )
        .await
        .expect("record");
        assert!(id > 0);
        let entries = list_recent(&pool, 10).await.expect("list");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].actor, "admin");
        assert_eq!(entries[0].action, "create");
        assert_eq!(entries[0].target_type, "provider");
        assert_eq!(entries[0].target_id, "openai");
    }
}
