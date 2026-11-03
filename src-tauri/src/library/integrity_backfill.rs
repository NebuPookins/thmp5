//! Background job that decodes library files imported before the integrity check existed
//! (`source.integrity_checked_at IS NULL`) and records whether their audio is damaged.
//!
//! Work is resumable by construction: each file's verdict is committed on its own, so a run
//! that is interrupted loses nothing and the next start picks up the remaining rows.

use super::import::{integrity_columns, set_io_priority_idle};
use crate::audio_probe::{scan_audio, AudioScan};
use crate::db::DbPool;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// How often (in files) an info-level progress line is logged.
const PROGRESS_LOG_INTERVAL: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingFile {
    source_id: String,
    path: PathBuf,
}

/// Outcome counts of one backfill run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackfillSummary {
    /// Files whose scan completed (clean, damaged, or not decodable here).
    pub checked: usize,
    /// Subset of `checked` found to be damaged.
    pub damaged: usize,
    /// Files that could not be scanned (missing, unreadable); left unmarked.
    pub failed: usize,
    pub elapsed: Duration,
}

async fn pending_files(db: &DbPool) -> Result<Vec<PendingFile>> {
    let mut conn = db.acquire("integrity_backfill.list").await?;
    let rows = sqlx::query!(
        r#"SELECT id AS "id!", file_path AS "file_path!"
           FROM source
           WHERE source_type = 'local_file'
             AND file_path IS NOT NULL
             AND integrity_checked_at IS NULL
           ORDER BY id"#
    )
    .fetch_all(&mut *conn)
    .await
    .context("Failed to list sources awaiting an integrity check")?;
    Ok(rows
        .into_iter()
        .map(|r| PendingFile {
            source_id: r.id,
            path: PathBuf::from(r.file_path),
        })
        .collect())
}

async fn scan_at_idle_priority(path: PathBuf) -> Result<AudioScan> {
    tokio::task::spawn_blocking(move || {
        let _ = thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Min);
        set_io_priority_idle();
        scan_audio(&path).with_context(|| format!("Failed to scan {}", path.display()))
    })
    .await
    .context("Integrity scan task panicked")?
}

/// Record a verdict. Only touches `corruption` and `integrity_checked_at`, and only while the
/// row is still unchecked, so a concurrent import/rescan that already recorded a verdict wins.
async fn record_verdict(
    db: &DbPool,
    serializer: &Semaphore,
    file: &PendingFile,
    scan: &AudioScan,
) -> Result<()> {
    let (_, corruption) = integrity_columns(Some(scan.integrity));
    let _permit = serializer
        .acquire()
        .await
        .context("Failed to acquire write serializer for integrity verdict")?;
    let mut conn = db
        .acquire(format!(
            "integrity_backfill.store path={}",
            file.path.display()
        ))
        .await?;
    sqlx::query!(
        "UPDATE source
         SET corruption = ?, integrity_checked_at = datetime('now')
         WHERE id = ? AND integrity_checked_at IS NULL",
        corruption,
        file.source_id,
    )
    .execute(&mut *conn)
    .await
    .with_context(|| {
        format!(
            "Failed to store integrity verdict for {}",
            file.path.display()
        )
    })?;
    Ok(())
}

/// Progress of a backfill run, reported before the first file and after each one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct BackfillProgress {
    pub done: usize,
    pub total: usize,
    /// Damaged files found so far in this run.
    pub damaged: usize,
}

/// Check every unchecked local file once, one at a time. `on_progress` is called before the
/// first file and after each one. Per-file failures are logged and skipped; the work
/// list is fetched once, so a bad file is never retried within a run.
pub async fn run_backfill(
    db: &DbPool,
    serializer: &Semaphore,
    on_progress: impl Fn(BackfillProgress),
) -> Result<BackfillSummary> {
    let started = Instant::now();
    let files = pending_files(db).await?;
    let total = files.len();
    if total == 0 {
        tracing::debug!("Integrity backfill: nothing to check");
        return Ok(BackfillSummary::default());
    }
    tracing::info!(files = total, "Integrity backfill starting");
    on_progress(BackfillProgress {
        done: 0,
        total,
        damaged: 0,
    });

    let mut summary = BackfillSummary::default();
    for (index, file) in files.iter().enumerate() {
        match scan_at_idle_priority(file.path.clone()).await {
            Ok(scan) => match record_verdict(db, serializer, file, &scan).await {
                Ok(()) => {
                    summary.checked += 1;
                    if let Some(description) = scan.integrity.description() {
                        summary.damaged += 1;
                        tracing::warn!(
                            path = %file.path.display(),
                            source_id = %file.source_id,
                            %description,
                            "Integrity backfill: damaged audio found"
                        );
                    }
                }
                Err(e) => {
                    summary.failed += 1;
                    tracing::warn!(
                        path = %file.path.display(),
                        source_id = %file.source_id,
                        "Integrity backfill: could not store verdict: {e:#}"
                    );
                }
            },
            Err(e) => {
                summary.failed += 1;
                tracing::warn!(
                    path = %file.path.display(),
                    source_id = %file.source_id,
                    "Integrity backfill: skipping file: {e:#}"
                );
            }
        }
        let done = index + 1;
        on_progress(BackfillProgress {
            done,
            total,
            damaged: summary.damaged,
        });
        if done % PROGRESS_LOG_INTERVAL == 0 {
            tracing::info!(
                done,
                total,
                damaged = summary.damaged,
                failed = summary.failed,
                "Integrity backfill progress"
            );
        }
    }

    let summary = BackfillSummary {
        elapsed: started.elapsed(),
        ..summary
    };
    tracing::info!(
        checked = summary.checked,
        damaged = summary.damaged,
        failed = summary.failed,
        elapsed_secs = summary.elapsed.as_secs(),
        "Integrity backfill finished"
    );
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init_pool;
    use tempfile::TempDir;

    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tone-2s.mp3");

    async fn insert_source(db: &DbPool, id: &str, path: &std::path::Path, checked: bool) {
        let path_str = path.to_string_lossy().to_string();
        let mut conn = db.acquire("test.insert_source").await.unwrap();
        sqlx::query!(
            "INSERT INTO source (id, source_type, file_path, duration_ms, integrity_checked_at)
             VALUES (?, 'local_file', ?, 2000, CASE WHEN ? THEN datetime('now') END)",
            id,
            path_str,
            checked,
        )
        .execute(&mut *conn)
        .await
        .unwrap();
    }

    /// `(integrity_checked_at is set, corruption)`
    async fn state_of(db: &DbPool, id: &str) -> (bool, Option<String>) {
        let mut conn = db.acquire("test.state_of").await.unwrap();
        let row = sqlx::query!(
            "SELECT integrity_checked_at, corruption FROM source WHERE id = ?",
            id
        )
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        (row.integrity_checked_at.is_some(), row.corruption)
    }

    async fn setup() -> (TempDir, DbPool, Semaphore) {
        let tmp = TempDir::new().unwrap();
        let pool = init_pool(&tmp.path().join("test.db")).await.unwrap();
        (tmp, pool, Semaphore::new(1))
    }

    #[tokio::test]
    async fn unchecked_clean_file_is_marked_checked_without_corruption() {
        let (_tmp, db, serializer) = setup().await;
        insert_source(&db, "a", std::path::Path::new(FIXTURE), false).await;

        let summary = run_backfill(&db, &serializer, |_| {}).await.unwrap();

        assert_eq!(summary.checked, 1);
        assert_eq!(summary.damaged, 0);
        assert_eq!(state_of(&db, "a").await, (true, None));
    }

    #[tokio::test]
    async fn damaged_file_gets_a_corruption_description() {
        let (tmp, db, serializer) = setup().await;
        let mut bytes = std::fs::read(FIXTURE).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid..mid + 1_500]
            .iter_mut()
            .enumerate()
            .for_each(|(i, b)| *b = (i as u8).wrapping_mul(37) & 0x7F);
        let damaged = tmp.path().join("damaged.mp3");
        std::fs::write(&damaged, bytes).unwrap();
        insert_source(&db, "d", &damaged, false).await;

        let summary = run_backfill(&db, &serializer, |_| {}).await.unwrap();

        assert_eq!(summary.damaged, 1);
        let (checked, corruption) = state_of(&db, "d").await;
        assert!(checked);
        assert!(corruption.is_some());
    }

    #[tokio::test]
    async fn already_checked_rows_are_left_alone() {
        let (tmp, db, serializer) = setup().await;
        // The path does not exist: if the row were processed it would count as a failure.
        insert_source(&db, "c", &tmp.path().join("gone.mp3"), true).await;

        let summary = run_backfill(&db, &serializer, |_| {}).await.unwrap();

        assert_eq!(summary.checked + summary.failed, 0);
        assert_eq!(pending_files(&db).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn missing_file_is_skipped_and_stays_unchecked() {
        let (tmp, db, serializer) = setup().await;
        insert_source(&db, "m", &tmp.path().join("gone.mp3"), false).await;
        insert_source(&db, "ok", std::path::Path::new(FIXTURE), false).await;

        let summary = run_backfill(&db, &serializer, |_| {}).await.unwrap();

        assert_eq!((summary.checked, summary.failed), (1, 1));
        assert_eq!(state_of(&db, "m").await, (false, None));
        assert_eq!(state_of(&db, "ok").await, (true, None));
    }
}
