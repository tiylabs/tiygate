# TiyGate Migrations

TiyGate uses **two independent migration sequences**:

* `config` — providers, routes, api_keys, config_epoch, settings,
  audit_log
* `log` — request_logs, schema_version

The split enforces design doc §4.3: configuration tables and
log tables can be evolved independently (different retention
windows, different scaling profiles).

## Running migrations

### At startup (default)

The `App::new()` path runs both sequences on every startup. The
runner is idempotent — applying an already-applied migration is
a no-op. Migrations are *only* run by the process that first
wins the write race; the others see the bookkeeping table
already populated and skip.

```bash
tiygate                       # migrations run automatically
```

### As a CLI subcommand

```
tiygate migrate                # apply pending migrations and exit
tiygate migrate status         # print applied migrations
```

The CLI form is useful in CI / pre-deployment hooks where you
want to verify schema state without starting the gateway.

## Layout

```
crates/store/migrations/
├── config/
│   └── 20260101000001_init.sql
└── log/
    └── 20260101000001_init.sql
```

Each `*.sql` file is a single migration. The numeric prefix is
the version; sqlx-style lexicographic ordering is used. Multiple
statements per file are split on `;` and executed in order;
each split statement is recorded as a row in `_migrations` after
the file is applied successfully.

## Versioning policy

* The first migration in a sequence is `20260101000001`. New
  migrations append a higher version (e.g. `20260102000001`).
* We do **not** rewrite history. Once a migration is in
  production, do not modify it — add a new one.
* A new column on an existing table is a new migration with an
  `ALTER TABLE … ADD COLUMN …` statement. SQLite supports this
  even without a separate schema-revision table.

## Backend portability

The same SQL is intended to run on both SQLite (default) and
PostgreSQL (Phase 5). Where syntax differs (e.g. `AUTOINCREMENT`
vs. `BIGSERIAL`), prefer the SQL standard form (`INTEGER
PRIMARY KEY` works on both).

The Phase 4 code only enables the `sqlite` driver. The `DbKind`
enum in `crates/store/src/db.rs` is ready to add a `Postgres`
variant; flipping the build feature will enable it without
touching the migration code.

## Rollback

TiyGate does not auto-rollback. To roll back a migration:

1. Add a *new* migration that reverts the change (e.g. drop the
   column, restore the old data from a backup).
2. Deploy the new migration forward.

This avoids the unsafe "roll back to version N" semantic that
other migration frameworks offer. The `_migrations` table
records what was applied and when; a `migrate status` run
shows the current state.

## Verifying state

* `tiygate migrate status` prints a table:
  ```
  sequence   version               applied_at
  config     20260101000001        2026-06-11T05:01:00Z
  log        20260101000001        2026-06-11T05:01:00Z
  ```
* In psql / sqlite3:
  ```sql
  SELECT sequence, version, applied_at FROM _migrations ORDER BY sequence, version;
  ```
* For request log retention:
  ```sql
  SELECT COUNT(*) FROM request_logs WHERE ts < datetime('now', '-30 days');
  -- should be 0 after a successful retention pass.
  ```

## Implementation

* Migration runner: `crates/store/src/db.rs`
* Migrations: `crates/store/migrations/{config,log}/*.sql`
* CLI: `crates/server/src/cli.rs` (subcommands `migrate`,
  `migrate-status`) and `crates/server/src/main.rs` (the
  dispatch in `main()`).
