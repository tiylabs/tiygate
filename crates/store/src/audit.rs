//! Audit log — every admin write operation should leave a trace.
//!
//! The audit log is a separate table from `request_logs` so a
//! security review can read write history without trawling
//! per-request data.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use thiserror::Error;

use crate::db::{AnyRow, DbPool};

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
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO audit_log (actor, action, target_type, target_id, details_json, ts) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(actor)
    .bind(action)
    .bind(target_type)
    .bind(target_id)
    .bind(&details_str)
    .bind(&now)
    .fetch_one(pool.any())
    .await?;
    Ok(id)
}

/// Fetch a page of audit entries, newest first, plus the total row count.
pub async fn list_page(
    pool: &DbPool,
    limit: i64,
    offset: i64,
) -> Result<(Vec<AuditEntry>, i64), AuditError> {
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log")
        .fetch_one(pool.any())
        .await?;
    let rows: Vec<AnyRow> = sqlx::query(
        "SELECT id, actor, action, target_type, target_id, details_json, ts \
         FROM audit_log ORDER BY id DESC LIMIT $1 OFFSET $2",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(pool.any())
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(row_to_entry(row)?);
    }
    Ok((out, total))
}

fn row_to_entry(row: AnyRow) -> Result<AuditEntry, AuditError> {
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
    Ok(AuditEntry {
        id: row.get("id"),
        actor: row.get("actor"),
        action: row.get("action"),
        target_type: row.get("target_type"),
        target_id: row.get("target_id"),
        details,
        ts,
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::db;

    #[tokio::test]
    async fn record_and_list_page_round_trip() {
        let pool = db::open_pool("sqlite::memory:").await.expect("pool");
        db::run_migrations(&pool).await.expect("migrate");
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
        let (entries, total) = list_page(&pool, 10, 0).await.expect("list");
        assert_eq!(total, 1);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].actor, "admin");
        assert_eq!(entries[0].action, "create");
        assert_eq!(entries[0].target_type, "provider");
        assert_eq!(entries[0].target_id, "openai");
    }

    #[tokio::test]
    async fn list_page_returns_total_and_offset_page() {
        let pool = db::open_pool("sqlite::memory:").await.expect("pool");
        db::run_migrations(&pool).await.expect("migrate");
        for n in 0..3 {
            record(
                &pool,
                "admin",
                "update",
                "provider",
                &format!("provider-{n}"),
                &serde_json::json!({"name": format!("Provider {n}")}),
            )
            .await
            .expect("record");
        }

        let (first_page, total) = list_page(&pool, 2, 0).await.expect("first page");
        assert_eq!(total, 3);
        assert_eq!(first_page.len(), 2);
        assert_eq!(first_page[0].target_id, "provider-2");
        assert_eq!(first_page[1].target_id, "provider-1");

        let (second_page, total) = list_page(&pool, 2, 2).await.expect("second page");
        assert_eq!(total, 3);
        assert_eq!(second_page.len(), 1);
        assert_eq!(second_page[0].target_id, "provider-0");
    }
}
