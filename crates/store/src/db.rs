//! DB pool factory + migration runner.
//!
//! Supports two backends: SQLite (zero-dep, default for dev/test) and
//! PostgreSQL (production). The backend is selected by the URL
//! scheme in the `database_url` config:
//!
//! * `sqlite://path/to.db` — or `sqlite::memory:` for in-process
//!   tests. Internally mapped to `sqlite://…`.
//! * `postgres://…` / `postgresql://…` — production deployments.
//!
//! Both backends share the same `DbPool` wrapper backed by
//! `sqlx::AnyPool`. Migrations are loaded at *runtime* from
//! backend-specific directories under `crates/store/migrations/`
//! to avoid the compile-time DB connection that `sqlx::migrate!()`
//! requires. Two independent migration sequences are tracked in a
//! shared `_migrations` table (one row per `(sequence, version)` pair).

use std::path::Path;
use std::sync::Once;

use thiserror::Error;
use tracing::info;

pub use sqlx::any::AnyRow;

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

/// Ensure sqlx `Any` drivers are installed exactly once per process.
fn install_any_drivers() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        sqlx::any::install_default_drivers();
    });
}

/// Pool wrapper — carries a `DbKind` tag alongside the runtime
/// `AnyPool` so migration and query layers can branch on the
/// backend when necessary (e.g. choosing the right migration
/// directory).
#[derive(Clone)]
pub struct DbPool {
    kind: DbKind,
    inner: sqlx::AnyPool,
}

impl DbPool {
    /// Access the underlying `AnyPool`.
    pub fn any(&self) -> &sqlx::AnyPool {
        &self.inner
    }

    /// Which backend this pool was opened for.
    pub fn kind(&self) -> DbKind {
        self.kind
    }
}

/// Open a connection pool to the database referenced by `url`.
///
/// For SQLite we configure `journal_mode=WAL` and a 5-second busy
/// timeout. For PostgreSQL we use the default `PgConnectOptions`
/// with statement-level trace logging.
pub async fn open_pool(url: &str) -> Result<DbPool, DbError> {
    install_any_drivers();
    let kind = DbKind::from_url(url)?;
    // AnyPool::connect parses the URL, delegates to the matching
    // driver (SQLite or Postgres), and applies its own defaults.
    // For SQLite we append query parameters to set WAL journal mode,
    // busy timeout, and create-if-missing behaviour. These are
    // parsed by the SQLite driver via the URL query string.
    let connect_url = match kind {
        DbKind::Sqlite => {
            // For in-memory databases, force shared cache so all
            // connections in the pool see the same tables. Use a
            // unique name so concurrent test pools are isolated.
            if url.contains(":memory:") {
                use std::sync::atomic::{AtomicU64, Ordering};
                static COUNTER: AtomicU64 = AtomicU64::new(0);
                let n = COUNTER.fetch_add(1, Ordering::Relaxed);
                format!("sqlite:file:memdb_{n}?mode=memory&cache=shared")
            } else if url.contains('?') {
                format!("{url}&mode=rwc")
            } else {
                format!("{url}?mode=rwc")
            }
        }
        DbKind::Postgres => url.to_string(),
    };
    let pool = sqlx::any::AnyPoolOptions::new()
        .connect(&connect_url)
        .await?;

    // Apply SQLite pragmas after connect.
    if kind == DbKind::Sqlite {
        sqlx::query("PRAGMA journal_mode = WAL")
            .execute(&pool)
            .await
            .ok();
        sqlx::query("PRAGMA busy_timeout = 5000")
            .execute(&pool)
            .await
            .ok();
    }

    Ok(DbPool { kind, inner: pool })
}

/// Run all pending migrations for both the *config* and *log*
/// sequences against `pool`. Idempotent — safe to call on every
/// startup.
pub async fn run_migrations(pool: &DbPool) -> Result<(), DbError> {
    let any = pool.any();

    // Bootstrap the _migrations bookkeeping table.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS _migrations (\
         sequence TEXT NOT NULL,\
         version BIGINT NOT NULL,\
         applied_at TEXT NOT NULL,\
         PRIMARY KEY (sequence, version))",
    )
    .execute(any)
    .await?;

    let (config_dir, log_dir) = migration_dirs(pool.kind());
    apply_sequence(any, CONFIG_SEQUENCE, config_dir).await?;
    apply_sequence(any, LOG_SEQUENCE, log_dir).await?;
    info!("migrations applied: config + log");
    Ok(())
}

/// Return the migration directories for a given backend.
fn migration_dirs(kind: DbKind) -> (&'static str, &'static str) {
    match kind {
        DbKind::Sqlite => ("migrations/config", "migrations/log"),
        DbKind::Postgres => ("migrations/postgres/config", "migrations/postgres/log"),
    }
}

async fn apply_sequence(pool: &sqlx::AnyPool, sequence: &str, dir: &str) -> Result<(), DbError> {
    let entries = read_migration_dir(dir)?;
    for MigrationFile { version, sql } in entries {
        let already: Option<(String,)> = sqlx::query_as(
            "SELECT applied_at FROM _migrations WHERE sequence = $1 AND version = $2",
        )
        .bind(sequence)
        .bind(version)
        .fetch_optional(pool)
        .await?;
        if already.is_some() {
            continue;
        }

        for stmt in split_sql_statements(&sql) {
            sqlx::query(&stmt).execute(pool).await.map_err(|e| {
                DbError::Migration(format!(
                    "sequence={sequence} version={version} stmt={stmt}: {e}"
                ))
            })?;
        }
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query("INSERT INTO _migrations (sequence, version, applied_at) VALUES ($1, $2, $3)")
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
pub async fn list_applied(pool: &DbPool) -> Result<Vec<(String, i64, String)>, DbError> {
    let rows: Vec<(String, i64, String)> = sqlx::query_as(
        "SELECT sequence, version, applied_at FROM _migrations ORDER BY sequence, version",
    )
    .fetch_all(pool.any())
    .await?;
    Ok(rows)
}

struct MigrationFile {
    version: i64,
    sql: String,
}

fn read_migration_dir(dir: &str) -> Result<Vec<MigrationFile>, DbError> {
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

/// Naïve SQL splitter.
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
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
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
        let cfg = read_migration_dir("migrations/config").expect("config migrations");
        let log = read_migration_dir("migrations/log").expect("log migrations");
        assert!(!cfg.is_empty(), "config migrations empty");
        assert!(!log.is_empty(), "log migrations empty");
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

    #[test]
    fn read_postgres_migration_dir_reads_real_files() {
        let cfg = read_migration_dir("migrations/postgres/config").expect("pg config migrations");
        let log = read_migration_dir("migrations/postgres/log").expect("pg log migrations");
        assert!(!cfg.is_empty(), "pg config migrations empty");
        assert!(!log.is_empty(), "pg log migrations empty");
        for w in cfg.windows(2) {
            assert!(
                w[0].version < w[1].version,
                "pg config versions not increasing"
            );
        }
        for w in log.windows(2) {
            assert!(
                w[0].version < w[1].version,
                "pg log versions not increasing"
            );
        }
    }

    #[test]
    fn migration_dirs_returns_correct_paths() {
        assert_eq!(
            migration_dirs(DbKind::Sqlite),
            ("migrations/config", "migrations/log")
        );
        assert_eq!(
            migration_dirs(DbKind::Postgres),
            ("migrations/postgres/config", "migrations/postgres/log")
        );
    }

    #[tokio::test]
    async fn sqlite_open_pool_and_migrate() {
        let pool = open_pool("sqlite::memory:").await.expect("open pool");
        assert_eq!(pool.kind(), DbKind::Sqlite);
        run_migrations(&pool).await.expect("migrations");
        let applied = list_applied(&pool).await.expect("list applied");
        assert!(!applied.is_empty());
    }
}
