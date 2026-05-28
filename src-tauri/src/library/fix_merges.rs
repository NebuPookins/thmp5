use crate::db::DbPool;
use crate::fingerprint;
use crate::library::import::import_file;
use crate::models::FixMergedRecordingsStats;
use anyhow::Result;
use sqlx::Row;
use std::collections::HashMap;
use std::path::Path;
use tokio::sync::Semaphore;

/// For each MBID group (UFID:http://musicbrainz.org in raw_tags_json) that has
/// more than one local-file source, compare the Chromaprint fingerprints of
/// those sources.  Any source whose fingerprint doesn't match the first source
/// (bit error rate ≥ 0.4) is split off: its source row is deleted and the file
/// is re-imported so it lands on the correct recording.  No network calls are made.
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

    let mut conn = db.acquire("fix_merges.list_sources").await?;
    let rows = sqlx::query(
        "SELECT id, file_path, raw_tags_json FROM source
         WHERE source_type = 'local_file' AND file_path IS NOT NULL",
    )
    .fetch_all(&mut *conn)
    .await?;
    drop(conn);

    // Group sources by MBID extracted from raw_tags_json.
    type SourceEntry = (String, String); // (id, file_path)
    let mut mbid_groups: HashMap<String, Vec<SourceEntry>> = HashMap::new();

    for row in &rows {
        let id: String = row.get("id");
        let file_path: String = row.get("file_path");
        let raw_tags_json: Option<String> = row.get("raw_tags_json");

        let mbid = match extract_single_mbid(&raw_tags_json) {
            Some(m) => m,
            None => continue,
        };

        mbid_groups.entry(mbid).or_default().push((id, file_path));
    }

    for (mbid, sources) in &mbid_groups {
        if sources.len() <= 1 {
            continue;
        }
        stats.recordings_checked += 1;
        if let Err(e) =
            process_mbid_group(db, mbid, sources, acoustid_key, serializer, &mut stats).await
        {
            stats.errors.push(format!("MBID {mbid}: {e:#}"));
        }
    }

    Ok(stats)
}

/// Extract a single MBID from a source's raw_tags_json.
///
/// raw_tags_json is a JSON array of [key, value] pairs, e.g.:
/// `[["UFID:http://musicbrainz.org","some-mbid"],["TIT2","Title"],...]`
///
/// If multiple distinct MBID values are present (erroneous), only the first is
/// used for grouping.  The catalog's deduplicate_raw_tags step flags this as a
/// FileIssue at load time.
fn extract_single_mbid(raw_tags_json: &Option<String>) -> Option<String> {
    let json_str = raw_tags_json.as_ref()?;
    if json_str.is_empty() {
        return None;
    }
    let pairs: Vec<Vec<String>> = serde_json::from_str(json_str).ok()?;
    pairs
        .into_iter()
        .filter(|pair| pair.len() >= 2 && pair[0] == "UFID:http://musicbrainz.org")
        .find_map(|pair| {
            let v = pair[1].clone();
            if v.is_empty() {
                None
            } else {
                Some(v)
            }
        })
}

async fn process_mbid_group(
    db: &DbPool,
    mbid: &str,
    sources: &[(String, String)],
    acoustid_key: Option<&str>,
    serializer: &Semaphore,
    stats: &mut FixMergedRecordingsStats,
) -> Result<()> {
    // Filter to sources whose files exist on disk.
    let sources: Vec<(String, String)> = sources
        .iter()
        .filter(|(_, path)| Path::new(path).exists())
        .cloned()
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
        mbid,
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
                mbid,
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
