//! DB pool factory + migration runner.
//!
//! Phase 4 supports two backends: SQLite (zero-dep, default) and
//! PostgreSQL (production). The backend is selected by the URL
//! scheme in the `database_url` config:
//!
//! * `sqlite://path/to.db` — or `sqlite::memory:` for in-process
//!   tests. Internally mapped to `sqlite://…`.
//! * `postgres://…` — production deployments; uses the
//!   `postgres` sqlx feature already enabled at the workspace level.
//!
//! Migrations are loaded at *runtime* from
//! `crates/store/migrations/{config,log}` to avoid the compile-time
//! DB connection that `sqlx::migrate!()` requires. Two independent
//! migration sequences are tracked in a shared `_migrations` table
//! (one row per `(sequence, version)` pair).

use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool};
use sqlx::ConnectOptions;
use thiserror::Error;
use tracing::info;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("sqlx error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("unsupported database URL: {0}")]
    UnsupportedUrl(String),
    #[error("migration error: {0}")]
    Migration(String),
    #[error("migration file read error: {0}")]
    Io(#[from] std::io::Error),
}

const CONFIG_SEQUENCE: &str = "config";
const LOG_SEQUENCE: &str = "log";

/// Database connection kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbKind {
    Sqlite,
    Postgres,
}

impl DbKind {
    pub fn from_url(url: &str) -> Result<Self, DbError> {
        if url.starts_with("sqlite:") {
            Ok(Self::Sqlite)
        } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            Ok(Self::Postgres)
        } else {
            Err(DbError::UnsupportedUrl(url.to_string()))
        }
    }
}

/// Pool wrapper — the Phase 4 build only enables the `sqlite` driver,
/// so we return a concrete `SqlitePool`. The wrapper is named to
/// leave room for a future enum that also carries a `PgPool`
/// (Postgres is wired through `DbKind` but not yet implemented in
/// Phase 4 — see [`open_pool`]).
#[derive(Clone)]
pub struct DbPool {
    inner: SqlitePool,
}

impl DbPool {
    pub fn sqlite(&self) -> &SqlitePool {
        &self.inner
    }

    pub fn kind(&self) -> DbKind {
        DbKind::Sqlite
    }
}

/// Open a connection pool to the database referenced by `url`.
///
/// For SQLite we set `journal_mode=WAL` (better concurrency under
/// sustained writes) and a 5-second busy timeout. PostgreSQL
/// deployments are reserved for Phase 5+; the URL is accepted but
/// the call returns [`DbError::UnsupportedUrl`].
pub async fn open_pool(url: &str) -> Result<DbPool, DbError> {
    let kind = DbKind::from_url(url)?;
    match kind {
        DbKind::Sqlite => {
            let opts: SqliteConnectOptions = url.parse::<SqliteConnectOptions>()?;
            let opts = opts
                .create_if_missing(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                .busy_timeout(Duration::from_secs(5))
                .log_statements(tracing::log::LevelFilter::Trace);
            let pool = sqlx::SqlitePool::connect_with(opts).await?;
            Ok(DbPool { inner: pool })
        }
        DbKind::Postgres => Err(DbError::UnsupportedUrl(format!(
            "postgres backend reserved for Phase 5+; current build supports sqlite: ({url})"
        ))),
    }
}

/// Run all pending migrations for both the *config* and *log*
/// sequences against `pool`. Idempotent — safe to call on every
/// startup.
pub async fn run_migrations(pool: &sqlx::SqlitePool) -> Result<(), DbError> {
    // Bootstrap the _migrations bookkeeping table.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS _migrations (\
         sequence TEXT NOT NULL,\
         version INTEGER NOT NULL,\
         applied_at TEXT NOT NULL,\
         PRIMARY KEY (sequence, version))",
    )
    .execute(pool)
    .await?;

    apply_sequence(pool, CONFIG_SEQUENCE, "migrations/config").await?;
    apply_sequence(pool, LOG_SEQUENCE, "migrations/log").await?;
    info!("migrations applied: config + log");
    Ok(())
}

async fn apply_sequence(pool: &sqlx::SqlitePool, sequence: &str, dir: &str) -> Result<(), DbError> {
    // Discover available .sql files in `dir`, sorted lexicographically.
    // Each file is treated as one migration; the version is the file's
    // numeric prefix (e.g. 20260101000001 in
    // 20260101000001_init.sql). Skipped if the version is already
    // recorded in `_migrations`.
    let entries = read_migration_dir(dir)?;
    for MigrationFile { version, sql } in entries {
        let already: Option<(String,)> = sqlx::query_as(
            "SELECT applied_at FROM _migrations WHERE sequence = ?1 AND version = ?2",
        )
        .bind(sequence)
        .bind(version)
        .fetch_optional(pool)
        .await?;
        if already.is_some() {
            continue;
        }

        // Each migration file may contain multiple `;`-separated
        // statements. sqlx's `execute` only handles one statement,
        // so we split on `;` and run them in order. This is good
        // enough for the Phase 4 SQL — production migrations would
        // carry explicit transaction wrappers.
        for stmt in split_sql_statements(&sql) {
            sqlx::query(&stmt).execute(pool).await.map_err(|e| {
                DbError::Migration(format!(
                    "sequence={sequence} version={version} stmt={stmt}: {e}"
                ))
            })?;
        }
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query("INSERT INTO _migrations (sequence, version, applied_at) VALUES (?1, ?2, ?3)")
            .bind(sequence)
            .bind(version)
            .bind(now)
            .execute(pool)
            .await?;
        info!(sequence, version, "migration applied");
    }
    Ok(())
}

/// Returns the list of applied migrations, ordered.
pub async fn list_applied(pool: &sqlx::SqlitePool) -> Result<Vec<(String, i64, String)>, DbError> {
    let rows: Vec<(String, i64, String)> = sqlx::query_as(
        "SELECT sequence, version, applied_at FROM _migrations ORDER BY sequence, version",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

struct MigrationFile {
    version: i64,
    sql: String,
}

fn read_migration_dir(dir: &str) -> Result<Vec<MigrationFile>, DbError> {
    // Resolve the path relative to the manifest dir so the binary
    // works regardless of CWD. We rely on the `CARGO_MANIFEST_DIR`
    // env var being set at compile time.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let abs = Path::new(manifest_dir).join(dir);
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&abs)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let file_name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if !file_name.ends_with(".sql") {
            continue;
        }
        let version: i64 = file_name
            .split('_')
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| DbError::Migration(format!("bad migration name: {file_name}")))?;
        let sql = std::fs::read_to_string(&path)?;
        out.push(MigrationFile { version, sql });
    }
    out.sort_by_key(|m| m.version);
    Ok(out)
}

/// Naïve SQL splitter. Strips line comments (`-- …`) before
/// splitting on `;`. Sufficient for our hand-written Phase 4
/// migrations; complex multi-statement procedures would warrant a
/// real SQL parser.
fn split_sql_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for line in sql.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("--") {
            continue;
        }
        buf.push_str(line);
        buf.push('\n');
        if line.trim_end().ends_with(';') {
            let stmt = buf.trim().trim_end_matches(';').to_string();
            if !stmt.is_empty() {
                out.push(stmt);
            }
            buf.clear();
        }
    }
    let tail = buf.trim();
    if !tail.is_empty() {
        out.push(tail.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_kind_detection() {
        assert_eq!(DbKind::from_url("sqlite::memory:").unwrap(), DbKind::Sqlite);
        assert_eq!(
            DbKind::from_url("sqlite:///tmp/x.db").unwrap(),
            DbKind::Sqlite
        );
        assert_eq!(
            DbKind::from_url("postgres://u@h/db").unwrap(),
            DbKind::Postgres
        );
        assert!(DbKind::from_url("mysql://").is_err());
    }

    #[test]
    fn split_sql_drops_comments_and_trailing_semicolon() {
        let sql = "-- a comment\nCREATE TABLE t (id INT);\n-- another\nINSERT INTO t VALUES (1);";
        let parts = split_sql_statements(sql);
        assert_eq!(parts.len(), 2);
        assert!(parts[0].starts_with("CREATE"));
        assert!(parts[1].starts_with("INSERT"));
    }

    #[test]
    fn read_migration_dir_reads_real_files() {
        // The two migration files we ship must be visible from the
        // manifest directory. This test fails if someone accidentally
        // moves the migrations folder.
        let cfg = read_migration_dir("migrations/config").expect("config migrations");
        let log = read_migration_dir("migrations/log").expect("log migrations");
        assert!(!cfg.is_empty(), "config migrations empty");
        assert!(!log.is_empty(), "log migrations empty");
        // Strictly increasing versions.
        for w in cfg.windows(2) {
            assert!(
                w[0].version < w[1].version,
                "config versions not increasing"
            );
        }
        for w in log.windows(2) {
            assert!(w[0].version < w[1].version, "log versions not increasing");
        }
    }
}
