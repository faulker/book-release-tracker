//! Database pool initialization and migrations.
//!
//! `queries.rs` holds all typed query fns; `models.rs` mirrors the schema.

pub mod models;
pub mod queries;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use thiserror::Error;

/// Errors produced while opening the database or running migrations.
#[derive(Debug, Error)]
pub enum DbError {
    /// Failed to open/connect to the sqlite file (or in-memory db).
    #[error("could not open database at {path}: {source}")]
    Connect {
        path: String,
        #[source]
        source: sqlx::Error,
    },
    /// A migration failed to apply.
    #[error("migration failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

/// Opens a sqlite pool for `path` (creating the file if it doesn't exist) and
/// applies all pending migrations. Pass `":memory:"` for an ephemeral,
/// process-local database (used by tests); a single connection is kept alive
/// in that case since each new sqlite in-memory connection is otherwise a
/// separate, empty database.
pub async fn init(path: &str) -> Result<SqlitePool, DbError> {
    let is_memory = path == ":memory:";

    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(if is_memory { 1 } else { 5 })
        .connect_with(options)
        .await
        .map_err(|source| DbError::Connect {
            path: path.to_string(),
            source,
        })?;

    sqlx::migrate!("./migrations").run(&pool).await?;

    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_apply_cleanly_in_memory() {
        let pool = init(":memory:").await.expect("init in-memory db");

        let tables: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .fetch_all(&pool)
                .await
                .expect("list tables");
        let names: Vec<_> = tables.into_iter().map(|(n,)| n).collect();

        assert!(names.contains(&"series".to_string()));
        assert!(names.contains(&"owned_books".to_string()));
        assert!(names.contains(&"known_releases".to_string()));
        assert!(names.contains(&"series_overrides".to_string()));
        assert!(names.contains(&"check_runs".to_string()));
    }

    #[tokio::test]
    async fn migrations_apply_cleanly_against_tempfile() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("test.db");

        let pool = init(path.to_str().expect("utf8 path"))
            .await
            .expect("init file-backed db");

        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM series")
            .fetch_one(&pool)
            .await
            .expect("query series table");
        assert_eq!(count.0, 0);

        pool.close().await;
        assert!(path.exists());
    }
}
