use anyhow::Result;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    SqlitePool,
};
use std::path::Path;

/// Number of concurrent workers (and DB pool connections) to use.
/// Computed once as `max(1, min(cpu_count - 1, 6))`.
pub fn worker_count() -> u32 {
    std::thread::available_parallelism()
        .map(|n| (n.get() as u32).saturating_sub(1).clamp(1, 6))
        .unwrap_or(4)
}

pub async fn init_pool(db_path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);

    // Reserve extra connections beyond import workers so UI commands
    // (e.g. list_recordings) can always acquire one during a scan.
    let pool = SqlitePoolOptions::new()
        .max_connections(worker_count() + 2)
        .connect_with(opts)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}
