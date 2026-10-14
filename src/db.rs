//! Database connection management and migrations.

use crate::error::Result;

use anyhow::Context as _;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

/// Connect a SQLite pool with foreign-key enforcement ON.
///
/// SQLite enforces `ON DELETE CASCADE` only when `PRAGMA foreign_keys = ON`,
/// which is a **per-connection** setting (it defaults to OFF). Every pool that
/// opens a database whose schema relies on cascades — the per-agent `agent.db`
/// and the instance `spacebot.db` — must go through here, or hard deletes will
/// silently leave orphaned rows behind (the "graveyard": `memory_vectors`,
/// `human_identities`, `associations`, `memory_nodes`, `memory_edges`).
///
/// Do not replace this with a bare `SqlitePool::connect` — that re-introduces
/// the inert-cascade bug. See `migrations/20260819000001_fk_cascade_cleanup.sql`.
pub async fn connect_sqlite(url: &str) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str(url)
        .with_context(|| format!("invalid SQLite url: {url}"))?
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .with_context(|| format!("failed to connect to SQLite: {url}"))?;
    Ok(pool)
}

/// Database connections bundle for per-agent databases.
pub struct Db {
    /// SQLite pool for relational data.
    pub sqlite: SqlitePool,

    /// Redb database for key-value config.
    pub redb: Arc<redb::Database>,
}

impl Db {
    /// Connect to all databases and run migrations.
    pub async fn connect(data_dir: &Path) -> Result<Self> {
        // SQLite — per-agent agent.db. If an old spacebot.db exists from
        // before the rename, move it to agent.db.
        let agent_db = data_dir.join("agent.db");
        let legacy_db = data_dir.join("spacebot.db");
        if legacy_db.exists() && !agent_db.exists() {
            std::fs::rename(&legacy_db, &agent_db).with_context(|| {
                format!(
                    "failed to rename legacy per-agent DB {} -> {}",
                    legacy_db.display(),
                    agent_db.display()
                )
            })?;
        }
        let sqlite_url = format!("sqlite:{}?mode=rwc", agent_db.display());
        // Foreign keys ON: the memory satellite tables (vectors, identities,
        // associations, graph nodes/edges) rely on ON DELETE CASCADE to reclaim
        // their rows when a memory is hard-deleted. A bare connect would leave
        // the pragma OFF and leak orphans. See connect_sqlite.
        let sqlite = connect_sqlite(&sqlite_url)
            .await
            .with_context(|| "failed to connect to SQLite")?;

        // Run migrations
        sqlx::migrate!("./migrations")
            .run(&sqlite)
            .await
            .with_context(|| "failed to run database migrations")?;

        // Redb
        let redb_path = data_dir.join("config.redb");
        let redb = redb::Database::create(&redb_path)
            .with_context(|| format!("failed to create redb at: {}", redb_path.display()))?;

        Ok(Self {
            sqlite,
            redb: Arc::new(redb),
        })
    }

    /// Close all database connections gracefully.
    pub async fn close(self) {
        self.sqlite.close().await;
        // Redb closes automatically when dropped
    }
}

/// Connect to the instance-level spacebot database and run its migrations.
///
/// The instance database lives at `{instance_dir}/data/spacebot.db` and holds
/// data shared across all agents: tasks, projects, repos, worktrees. This
/// replaces per-agent task and project tables.
///
/// If an old `tasks.db` exists from before the rename, it is moved to
/// `spacebot.db` first.
pub async fn connect_instance_db(data_dir: &Path) -> Result<SqlitePool> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("failed to create data directory: {}", data_dir.display()))?;

    let db_path = data_dir.join("spacebot.db");
    let legacy_tasks_db = data_dir.join("tasks.db");
    if legacy_tasks_db.exists() && !db_path.exists() {
        std::fs::rename(&legacy_tasks_db, &db_path).with_context(|| {
            format!(
                "failed to rename legacy tasks.db -> spacebot.db at {}",
                data_dir.display()
            )
        })?;
    }
    let url = format!("sqlite:{}?mode=rwc", db_path.display());

    // Foreign keys ON, same rationale as the per-agent pool: the instance DB
    // schema carries ON DELETE CASCADE constraints that must actually fire.
    let pool = connect_sqlite(&url).await.with_context(|| {
        format!(
            "failed to connect to instance database: {}",
            db_path.display()
        )
    })?;

    sqlx::migrate!("./migrations/global")
        .run(&pool)
        .await
        .with_context(|| "failed to run instance database migrations")?;

    Ok(pool)
}
