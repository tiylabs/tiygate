use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use chrono::Utc;
use serde_json::json;
use sqlx::Row;
use tokio::task::{JoinHandle, JoinSet};
use tracing::{info, warn};

use super::client::{ClientError, PayloadArchiveClient};
use super::{build_object_meta, gzip_compress, ArchivePayload, PayloadArchiveManifest};
use crate::archive::ArchiveStatus;
use crate::db::DbPool;

#[derive(Debug, Clone)]
pub struct PayloadArchiveWorkerConfig {
    pub interval: Duration,
    pub batch_size: usize,
    pub concurrency: usize,
    pub max_retries: i32,
    pub stale_after: Duration,
}

impl Default for PayloadArchiveWorkerConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(60),
            batch_size: 100,
            concurrency: 4,
            max_retries: 5,
            stale_after: Duration::from_secs(10 * 60),
        }
    }
}

pub struct PayloadArchiveHandle {
    handle: JoinHandle<()>,
}

impl PayloadArchiveHandle {
    pub async fn stop(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }
}

pub fn spawn(
    pool: Arc<DbPool>,
    client: Arc<dyn PayloadArchiveClient>,
    config: PayloadArchiveWorkerConfig,
) -> PayloadArchiveHandle {
    let handle = tokio::spawn(async move {
        info!(
            interval_secs = config.interval.as_secs(),
            batch_size = config.batch_size,
            concurrency = config.concurrency,
            max_retries = config.max_retries,
            "payload archive task started"
        );
        let mut tick = tokio::time::interval(config.interval);
        loop {
            tick.tick().await;
            if let Err(e) = archive_once(pool.clone(), client.clone(), &config).await {
                warn!(error = %e, "payload archive pass failed");
            }
        }
    });
    PayloadArchiveHandle { handle }
}

pub async fn archive_once(
    pool: Arc<DbPool>,
    client: Arc<dyn PayloadArchiveClient>,
    config: &PayloadArchiveWorkerConfig,
) -> Result<(), sqlx::Error> {
    let batch = claim_batch(
        pool.as_ref(),
        config.batch_size,
        config.stale_after,
        config.max_retries,
    )
    .await?;

    let concurrency = config.concurrency.max(1);
    for chunk in batch.chunks(concurrency) {
        let mut joinset = JoinSet::new();
        for payload in chunk.iter().cloned() {
            let client = client.clone();
            joinset.spawn(async move { upload_payload(client, payload).await });
        }

        while let Some(joined) = joinset.join_next().await {
            match joined {
                Ok(Ok((request_id, manifest))) => {
                    let manifest_json =
                        serde_json::to_string(&manifest).unwrap_or_else(|_| "{}".to_string());
                    if let Err(e) =
                        finalize_success(pool.as_ref(), &request_id, &manifest_json).await
                    {
                        warn!(error = %e, request_id, "payload archive finalize failed");
                    }
                }
                Ok(Err((request_id, e))) => {
                    let message = e.to_string();
                    warn!(
                        request_id,
                        archive_error = %message,
                        "payload archive upload failed"
                    );
                    if let Err(db_err) =
                        mark_failed(pool.as_ref(), &request_id, &message, config.max_retries).await
                    {
                        warn!(
                            error = %db_err,
                            request_id,
                            archive_error = %message,
                            "payload archive failure mark failed"
                        );
                    }
                }
                Err(e) => {
                    warn!(error = %e, "payload archive task join failed");
                }
            }
        }
    }
    Ok(())
}

async fn upload_payload(
    client: Arc<dyn PayloadArchiveClient>,
    payload: ArchivePayload,
) -> Result<(String, PayloadArchiveManifest), (String, ClientError)> {
    let mut objects = BTreeMap::new();
    for (kind, key, original) in payload.iter(client.prefix()) {
        let compressed = gzip_compress(&original)
            .map_err(|e| (payload.request_id.clone(), ClientError::Archive(e)))?;
        let meta = build_object_meta(kind, &original, &compressed, key.clone());
        let original_size = meta.original_size.to_string();
        let compressed_size = meta.compressed_size.to_string();
        let metadata = vec![
            ("request-id", payload.request_id.as_str()),
            ("object-kind", kind.as_str()),
            ("sha256", meta.sha256_hex.as_str()),
            ("original-size", original_size.as_str()),
            ("compressed-size", compressed_size.as_str()),
        ];
        client
            .put_object(
                &key,
                Bytes::from(compressed),
                &meta.content_type,
                &meta.content_encoding,
                metadata,
            )
            .await
            .map_err(|e| (payload.request_id.clone(), e))?;
        objects.insert(kind.as_str().to_string(), meta);
    }
    Ok((
        payload.request_id.clone(),
        PayloadArchiveManifest {
            request_id: payload.request_id,
            objects,
        },
    ))
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.and_then(|value| if value.is_empty() { None } else { Some(value) })
}

fn parsed_object(
    json_fields: Vec<(&str, Option<String>)>,
    string_fields: Vec<(&str, Option<String>)>,
) -> Option<String> {
    let mut object = serde_json::Map::new();
    for (key, value) in json_fields {
        if let Some(value) = value {
            let parsed = serde_json::from_str(&value).unwrap_or_else(|_| json!(value));
            object.insert(key.to_string(), parsed);
        }
    }
    for (key, value) in string_fields {
        if let Some(value) = value {
            object.insert(key.to_string(), json!(value));
        }
    }
    if object.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(object).to_string())
    }
}

async fn claim_batch(
    pool: &DbPool,
    batch_size: usize,
    stale_after: Duration,
    max_retries: i32,
) -> Result<Vec<ArchivePayload>, sqlx::Error> {
    let stale_before = (Utc::now()
        - chrono::Duration::from_std(stale_after)
            .unwrap_or_else(|_| chrono::Duration::minutes(10)))
    .to_rfc3339();
    let rows = sqlx::query(
        "SELECT p.request_id, l.raw_envelope_json, l.redacted_headers_json, \
                p.egress_method, p.egress_path, p.egress_headers_json, p.egress_body, \
                p.upstream_status, p.upstream_resp_headers_json, p.upstream_resp_body, \
                p.client_resp_headers_json, p.client_resp_body, \
                p.sse_parsed_json, p.client_sse_parsed_json \
         FROM request_payloads p \
         JOIN request_logs l ON l.request_id = p.request_id \
         WHERE p.payload_archive_attempts < $1 \
           AND l.raw_envelope_json IS NOT NULL \
           AND l.redacted_headers_json IS NOT NULL \
           AND p.egress_headers_json IS NOT NULL \
           AND p.egress_body IS NOT NULL \
           AND p.upstream_resp_headers_json IS NOT NULL \
           AND p.upstream_resp_body IS NOT NULL \
           AND p.client_resp_headers_json IS NOT NULL \
           AND p.client_resp_body IS NOT NULL \
           AND (p.payload_archive_status = 'archive_ready' \
                OR (p.payload_archive_status = 'uploading' AND (p.payload_archive_locked_at IS NULL OR p.payload_archive_locked_at < $2))) \
         ORDER BY p.captured_at ASC \
         LIMIT $3",
    )
    .bind(max_retries as i64)
    .bind(&stale_before)
    .bind(batch_size as i64)
    .fetch_all(pool.any())
    .await?;

    let now = Utc::now().to_rfc3339();
    let mut payloads = Vec::new();
    for r in rows {
        let request_id: String = r.get("request_id");
        let result = sqlx::query(
            "UPDATE request_payloads SET payload_archive_status = 'uploading', payload_archive_locked_at = $2, payload_archive_last_error = NULL \
             WHERE request_id = $1 \
               AND payload_archive_attempts < $3 \
               AND (payload_archive_status = 'archive_ready' \
                    OR (payload_archive_status = 'uploading' AND (payload_archive_locked_at IS NULL OR payload_archive_locked_at < $4)))",
        )
        .bind(&request_id)
        .bind(&now)
        .bind(max_retries as i64)
        .bind(&stale_before)
        .execute(pool.any())
        .await?;
        if result.rows_affected() == 0 {
            continue;
        }

        let cg_req_parsed =
            parsed_object(vec![("headers", r.get("redacted_headers_json"))], vec![]);
        let gp_req_parsed = parsed_object(
            vec![("headers", r.get("egress_headers_json"))],
            vec![
                ("method", non_empty(r.get("egress_method"))),
                ("path", non_empty(r.get("egress_path"))),
            ],
        );
        let pg_rsp_parsed = parsed_object(
            vec![
                ("headers", r.get("upstream_resp_headers_json")),
                ("body", r.get("sse_parsed_json")),
            ],
            vec![(
                "status",
                r.get::<Option<i64>, _>("upstream_status")
                    .map(|v| v.to_string()),
            )],
        );
        let gc_rsp_parsed = parsed_object(
            vec![
                ("headers", r.get("client_resp_headers_json")),
                ("body", r.get("client_sse_parsed_json")),
            ],
            vec![],
        );

        payloads.push(ArchivePayload {
            request_id,
            cg_req_raw: r.get("raw_envelope_json"),
            cg_req_parsed,
            gp_req_raw: r.get("egress_body"),
            gp_req_parsed,
            pg_rsp_raw: r.get("upstream_resp_body"),
            pg_rsp_parsed,
            gc_rsp_raw: r.get("client_resp_body"),
            gc_rsp_parsed,
        });
    }
    Ok(payloads)
}

async fn finalize_success(
    pool: &DbPool,
    request_id: &str,
    manifest_json: &str,
) -> Result<(), sqlx::Error> {
    let now = Utc::now().to_rfc3339();
    let mut tx = pool.any().begin().await?;
    sqlx::query(
        "UPDATE request_payloads SET \
            egress_headers_json = NULL, egress_body = NULL, \
            upstream_resp_headers_json = NULL, upstream_resp_body = NULL, \
            client_resp_headers_json = NULL, client_resp_body = NULL, \
            sse_parsed_json = NULL, client_sse_parsed_json = NULL, \
            payload_archive_status = $2, payload_archive_locked_at = NULL, \
            payload_archived_at = $3, payload_archive_manifest_json = $4 \
         WHERE request_id = $1",
    )
    .bind(request_id)
    .bind(ArchiveStatus::Uploaded.as_str())
    .bind(&now)
    .bind(manifest_json)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE request_logs SET raw_envelope_json = NULL, redacted_headers_json = NULL \
         WHERE request_id = $1",
    )
    .bind(request_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn mark_failed(
    pool: &DbPool,
    request_id: &str,
    error: &str,
    max_retries: i32,
) -> Result<(), sqlx::Error> {
    let row =
        sqlx::query("SELECT payload_archive_attempts FROM request_payloads WHERE request_id = $1")
            .bind(request_id)
            .fetch_optional(pool.any())
            .await?;
    let attempts = row
        .map(|r| r.get::<i64, _>("payload_archive_attempts") as i32)
        .unwrap_or(0)
        + 1;
    let status = if attempts >= max_retries {
        ArchiveStatus::Failed.as_str()
    } else {
        ArchiveStatus::ArchiveReady.as_str()
    };
    sqlx::query(
        "UPDATE request_payloads SET payload_archive_status = $2, payload_archive_attempts = $3, \
            payload_archive_last_error = $4, payload_archive_locked_at = NULL \
         WHERE request_id = $1",
    )
    .bind(request_id)
    .bind(status)
    .bind(attempts as i64)
    .bind(error)
    .execute(pool.any())
    .await?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::db;

    async fn insert_payload(pool: &DbPool, request_id: &str, status: &str, attempts: i32) {
        insert_payload_row(pool, request_id, status, attempts, true).await;
    }

    async fn insert_payload_row(
        pool: &DbPool,
        request_id: &str,
        status: &str,
        attempts: i32,
        with_log: bool,
    ) {
        if with_log {
            sqlx::query(
                "INSERT INTO request_logs (\
                    request_id, ts, virtual_model, ingress_protocol, status, raw_envelope_json, redacted_headers_json) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(request_id)
            .bind(Utc::now().to_rfc3339())
            .bind("gpt-test")
            .bind("http")
            .bind("ok")
            .bind(format!(r#"{{"request_id":"{request_id}"}}"#))
            .bind(format!(r#"{{"x-request":"{request_id}"}}"#))
            .execute(pool.any())
            .await
            .expect("insert log");
        }

        sqlx::query(
            "INSERT INTO request_payloads (\
                request_id, egress_method, egress_path, egress_headers_json, egress_body, \
                upstream_status, upstream_resp_headers_json, upstream_resp_body, \
                client_resp_headers_json, client_resp_body, sse_parsed_json, client_sse_parsed_json, captured_at, \
                payload_archive_status, payload_archive_attempts) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
        )
        .bind(request_id)
        .bind("POST")
        .bind("/v1/chat/completions")
        .bind(format!(r#"{{"x-egress":"{request_id}"}}"#))
        .bind(format!("req-{request_id}"))
        .bind(200_i64)
        .bind(format!(r#"{{"x-upstream":"{request_id}"}}"#))
        .bind(format!("rsp-{request_id}"))
        .bind(format!(r#"{{"x-client":"{request_id}"}}"#))
        .bind(format!("client-rsp-{request_id}"))
        .bind(format!(r#"{{"upstream":"parsed-{request_id}"}}"#))
        .bind(format!(r#"{{"client":"parsed-{request_id}"}}"#))
        .bind(Utc::now().to_rfc3339())
        .bind(status)
        .bind(attempts as i64)
        .execute(pool.any())
        .await
        .expect("insert payload");
    }

    #[tokio::test]
    async fn claim_batch_only_claims_archive_ready_and_stale_uploading() {
        let pool = db::open_pool("sqlite::memory:").await.expect("pool");
        db::run_migrations(&pool).await.expect("migrate");
        insert_payload_row(&pool, "ready", "archive_ready", 0, true).await;
        insert_payload_row(&pool, "pending", "pending", 0, true).await;
        insert_payload_row(&pool, "stale", "uploading", 0, true).await;
        let stale_at = (Utc::now() - chrono::Duration::minutes(30)).to_rfc3339();
        sqlx::query(
            "UPDATE request_payloads SET payload_archive_locked_at = $2 WHERE request_id = $1",
        )
        .bind("stale")
        .bind(stale_at)
        .execute(pool.any())
        .await
        .expect("mark stale");

        let batch = claim_batch(&pool, 10, Duration::from_secs(60), 5)
            .await
            .expect("claim");
        let ready_payload = batch
            .iter()
            .find(|p| p.request_id == "ready")
            .expect("ready payload");
        let objects = ready_payload.iter("archive-prefix");
        assert_eq!(objects.len(), 8);
        let kinds = objects
            .iter()
            .map(|(kind, _, _)| kind.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                "cg_req_raw",
                "cg_req_parsed",
                "gp_req_raw",
                "gp_req_parsed",
                "pg_rsp_raw",
                "pg_rsp_parsed",
                "gc_rsp_raw",
                "gc_rsp_parsed",
            ]
        );
        let cg_parsed = serde_json::from_str::<serde_json::Value>(
            ready_payload.cg_req_parsed.as_deref().expect("cg parsed"),
        )
        .expect("cg parsed json");
        assert_eq!(cg_parsed["headers"]["x-request"], "ready");
        let gp_parsed = serde_json::from_str::<serde_json::Value>(
            ready_payload.gp_req_parsed.as_deref().expect("gp parsed"),
        )
        .expect("gp parsed json");
        assert_eq!(gp_parsed["headers"]["x-egress"], "ready");
        assert_eq!(gp_parsed["method"], "POST");
        let pg_parsed = serde_json::from_str::<serde_json::Value>(
            ready_payload.pg_rsp_parsed.as_deref().expect("pg parsed"),
        )
        .expect("pg parsed json");
        assert_eq!(pg_parsed["headers"]["x-upstream"], "ready");
        assert_eq!(pg_parsed["status"], "200");
        let gc_parsed = serde_json::from_str::<serde_json::Value>(
            ready_payload.gc_rsp_parsed.as_deref().expect("gc parsed"),
        )
        .expect("gc parsed json");
        assert_eq!(gc_parsed["headers"]["x-client"], "ready");

        let mut ids = batch.into_iter().map(|p| p.request_id).collect::<Vec<_>>();
        ids.sort();
        assert_eq!(ids, vec!["ready".to_string(), "stale".to_string()]);

        let pending_status: String = sqlx::query_scalar(
            "SELECT payload_archive_status FROM request_payloads WHERE request_id = 'pending'",
        )
        .fetch_one(pool.any())
        .await
        .expect("pending status");
        assert_eq!(pending_status, "pending");
    }

    #[tokio::test]
    async fn mark_failed_returns_to_archive_ready_until_max_retries() {
        let pool = db::open_pool("sqlite::memory:").await.expect("pool");
        db::run_migrations(&pool).await.expect("migrate");
        insert_payload(&pool, "retry", "uploading", 0).await;

        mark_failed(&pool, "retry", "boom", 2)
            .await
            .expect("mark retryable failure");
        let row = sqlx::query(
            "SELECT payload_archive_status, payload_archive_attempts, payload_archive_last_error \
             FROM request_payloads WHERE request_id = 'retry'",
        )
        .fetch_one(pool.any())
        .await
        .expect("retry row");
        assert_eq!(
            row.get::<String, _>("payload_archive_status"),
            "archive_ready"
        );
        assert_eq!(row.get::<i64, _>("payload_archive_attempts"), 1);
        assert_eq!(row.get::<String, _>("payload_archive_last_error"), "boom");

        mark_failed(&pool, "retry", "boom again", 2)
            .await
            .expect("mark terminal failure");
        let status: String = sqlx::query_scalar(
            "SELECT payload_archive_status FROM request_payloads WHERE request_id = 'retry'",
        )
        .fetch_one(pool.any())
        .await
        .expect("terminal status");
        assert_eq!(status, "failed");
    }

    #[tokio::test]
    async fn claim_batch_skips_rows_until_request_log_exists() {
        let pool = db::open_pool("sqlite::memory:").await.expect("pool");
        db::run_migrations(&pool).await.expect("migrate");
        insert_payload_row(&pool, "orphan", "archive_ready", 0, false).await;

        let batch = claim_batch(&pool, 10, Duration::from_secs(60), 5)
            .await
            .expect("claim");
        assert!(batch.is_empty());

        let status: String = sqlx::query_scalar(
            "SELECT payload_archive_status FROM request_payloads WHERE request_id = 'orphan'",
        )
        .fetch_one(pool.any())
        .await
        .expect("status");
        assert_eq!(status, "archive_ready");
    }

    #[tokio::test]
    async fn finalize_success_clears_headers_and_bodies() {
        let pool = db::open_pool("sqlite::memory:").await.expect("pool");
        db::run_migrations(&pool).await.expect("migrate");
        insert_payload_row(&pool, "done", "uploading", 0, true).await;

        finalize_success(&pool, "done", "{\"request_id\":\"done\"}")
            .await
            .expect("finalize");
        let row = sqlx::query(
            "SELECT p.egress_headers_json, p.egress_body, p.upstream_resp_headers_json, p.upstream_resp_body, \
                p.client_resp_headers_json, p.client_resp_body, p.sse_parsed_json, p.client_sse_parsed_json, \
                p.payload_archive_status, p.payload_archive_manifest_json, \
                l.raw_envelope_json, l.redacted_headers_json \
             FROM request_payloads p LEFT JOIN request_logs l ON l.request_id = p.request_id \
             WHERE p.request_id = 'done'",
        )
        .fetch_one(pool.any())
        .await
        .expect("finalized row");
        assert!(row
            .get::<Option<String>, _>("egress_headers_json")
            .is_none());
        assert!(row.get::<Option<String>, _>("egress_body").is_none());
        assert!(row
            .get::<Option<String>, _>("upstream_resp_headers_json")
            .is_none());
        assert!(row.get::<Option<String>, _>("upstream_resp_body").is_none());
        assert!(row
            .get::<Option<String>, _>("client_resp_headers_json")
            .is_none());
        assert!(row.get::<Option<String>, _>("client_resp_body").is_none());
        assert!(row.get::<Option<String>, _>("sse_parsed_json").is_none());
        assert!(row
            .get::<Option<String>, _>("client_sse_parsed_json")
            .is_none());
        assert_eq!(row.get::<String, _>("payload_archive_status"), "uploaded");
        assert_eq!(
            row.get::<String, _>("payload_archive_manifest_json"),
            "{\"request_id\":\"done\"}"
        );
        assert!(row.get::<Option<String>, _>("raw_envelope_json").is_none());
        assert!(row
            .get::<Option<String>, _>("redacted_headers_json")
            .is_none());
    }
}
