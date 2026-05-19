use crate::db::DbPool;
use crate::fingerprint;
use crate::library::import::import_file;
use crate::models::FixMergedRecordingsStats;
use anyhow::Result;
use sqlx::Row;
use std::path::Path;
use tokio::sync::Semaphore;

/// For each recording that has more than one local-file source, compare the
/// Chromaprint fingerprints of those sources.  Any source whose fingerprint
/// doesn't match the first source (bit error rate ≥ 0.4) is split off: its
/// source row is deleted and the file is re-imported so it lands on the
/// correct recording.  No network calls are made.
pub async fn fix_merged_recordings(
    db: &DbPool,
    acoustid_key: Option<&str>,
    serializer: &Semaphore,
) -> Result<FixMergedRecordingsStats> {
    let mut stats = FixMergedRecordingsStats {
        recordings_checked: 0,
        recordings_split: 0,
        sources_reimported: 0,
        errors: Vec::new(),
    };

    let mut conn = db.acquire("fix_merges.list_recordings").await?;
    let recording_ids: Vec<String> = sqlx::query_scalar(
        "SELECT recording_id FROM source
         WHERE source_type = 'local_file'
         GROUP BY recording_id
         HAVING COUNT(*) > 1",
    )
    .fetch_all(&mut *conn)
    .await?;
    drop(conn);

    for recording_id in &recording_ids {
        stats.recordings_checked += 1;
        if let Err(e) =
            process_recording(db, recording_id, acoustid_key, serializer, &mut stats).await
        {
            stats
                .errors
                .push(format!("recording {recording_id}: {e:#}"));
        }
    }

    Ok(stats)
}

async fn process_recording(
    db: &DbPool,
    recording_id: &str,
    acoustid_key: Option<&str>,
    serializer: &Semaphore,
    stats: &mut FixMergedRecordingsStats,
) -> Result<()> {
    let mut conn = db
        .acquire(format!("fix_merges.process recording_id={recording_id}"))
        .await?;

    let rows = sqlx::query(
        "SELECT id, file_path FROM source
         WHERE recording_id = ? AND source_type = 'local_file' AND file_path IS NOT NULL",
    )
    .bind(recording_id)
    .fetch_all(&mut *conn)
    .await?;
    drop(conn);

    if rows.len() <= 1 {
        return Ok(());
    }

    // Collect (source_id, file_path) for sources whose files exist on disk.
    let sources: Vec<(String, String)> = rows
        .into_iter()
        .map(|r| (r.get::<String, _>("id"), r.get::<String, _>("file_path")))
        .filter(|(_, path)| Path::new(path).exists())
        .collect();

    if sources.len() <= 1 {
        return Ok(());
    }

    // Generate the Chromaprint fingerprint for the reference source (first one).
    let (ref_id, ref_path) = &sources[0];
    let ref_path_buf = std::path::PathBuf::from(ref_path);
    let ref_fp: Vec<u32> =
        tokio::task::spawn_blocking(move || fingerprint::raw_fingerprint(Path::new(&ref_path_buf)))
            .await
            .map_err(|e| anyhow::anyhow!("Blocking task panicked: {e}"))??;

    tracing::debug!(
        recording_id,
        reference_source = %ref_id,
        reference_path = %ref_path,
        "Using as fingerprint reference"
    );

    // Compare each other source against the reference.
    let mut to_split: Vec<(String, String)> = Vec::new();

    for (source_id, file_path) in &sources[1..] {
        let cmp_path = std::path::PathBuf::from(file_path);
        let ref_fp_clone = ref_fp.clone();
        let is_same = tokio::task::spawn_blocking(move || {
            fingerprint::raw_fingerprint(Path::new(&cmp_path))
                .map(|fp| fingerprint::ber(&ref_fp_clone, &fp) < 0.4)
        })
        .await
        .map_err(|e| anyhow::anyhow!("Blocking task panicked: {e}"))??;

        if !is_same {
            tracing::info!(
                recording_id,
                source_id = %source_id,
                path = %file_path,
                "Fingerprint mismatch — will split"
            );
            to_split.push((source_id.clone(), file_path.clone()));
        }
    }

    if to_split.is_empty() {
        return Ok(());
    }

    for (source_id, file_path) in to_split {
        let mut conn = db
            .acquire(format!("fix_merges.delete_source source_id={source_id}"))
            .await?;
        sqlx::query("DELETE FROM source WHERE id = ?")
            .bind(&source_id)
            .execute(&mut *conn)
            .await?;
        drop(conn);

        match import_file(db, Path::new(&file_path), acoustid_key, serializer).await {
            Ok(_) => {
                stats.sources_reimported += 1;
                tracing::info!(path = %file_path, "Re-imported split source");
            }
            Err(e) => {
                stats
                    .errors
                    .push(format!("{file_path}: re-import failed: {e:#}"));
            }
        }
    }

    stats.recordings_split += 1;
    Ok(())
}
