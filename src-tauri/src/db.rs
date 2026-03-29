use anyhow::Result;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    SqlitePool,
};
use std::path::Path;
use std::time::Duration;

/// Number of concurrent workers (and DB pool connections) to use.
/// Computed once as `max(1, min(cpu_count - 1, 6))`.
pub fn worker_count() -> u32 {
    std::thread::available_parallelism()
        .map(|n| (n.get() as u32).saturating_sub(1).clamp(1, 6))
        .unwrap_or(4)
}

fn pool_connection_count() -> u32 {
    // The importer can keep several connections busy while the UI also fires
    // bursts of list/detail queries together. Keep a modest reserve so those
    // short-lived UI commands do not starve behind import work.
    (worker_count() + 6).clamp(8, 16)
}

pub async fn init_pool(db_path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);

    // Reserve enough connections for import workers plus concurrent UI reads.
    let pool = SqlitePoolOptions::new()
        .max_connections(pool_connection_count())
        .acquire_timeout(Duration::from_secs(30))
        .connect_with(opts)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}
