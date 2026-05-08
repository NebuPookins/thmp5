use crate::db::DbPool;
use crate::library::import::get_or_create_artist;
use crate::library::scanner;
use crate::models::{ArtistFixStats, CompoundArtistCheck};
use sqlx::Connection;
use std::path::Path;

pub async fn check_artist_compound(
    db: &DbPool,
    artist_id: &str,
) -> Result<CompoundArtistCheck, String> {
    let mut conn = db
        .acquire(format!("check_artist_compound artist_id={artist_id}"))
        .await
        .map_err(|e| e.to_string())?;

    let artist_name: Option<String> = sqlx::query_scalar("SELECT name FROM artist WHERE id = ?")
        .bind(artist_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| e.to_string())?;

    let artist_name = artist_name.ok_or_else(|| "Artist not found".to_string())?;

    let paths: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT s.file_path
         FROM source s
         JOIN recording_artist ra ON ra.recording_id = s.recording_id
         WHERE ra.artist_id = ?
           AND s.source_type = 'local_file'
           AND s.file_path IS NOT NULL
         ORDER BY s.file_path",
    )
    .bind(artist_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    // Release the connection before slow I/O
    drop(conn);

    if paths.is_empty() {
        return Ok(CompoundArtistCheck {
            is_compound: false,
            evidence_count: 0,
            total_sources_checked: 0,
            individual_artist_names: vec![],
            source_examples: vec![],
        });
    }

    let mut individual_names: Vec<String> = Vec::new();
    let mut evidence_count = 0usize;

    for path_str in &paths {
        let path = Path::new(path_str);
        let (meta, _tags) = tokio::task::spawn_blocking({
            let owned = path.to_owned();
            move || {
                let meta = scanner::read_metadata(&owned).ok();
                let _tags = scanner::list_all_tags(&owned).ok();
                (meta, _tags)
            }
        })
        .await
        .map_err(|e| format!("Blocking task failed: {e}"))?;

        if let Some(meta) = meta {
            if let Some(artists) = &meta.meta.artists {
                let names: Vec<&str> = artists
                    .split(';')
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .collect();
                if names.len() >= 2 {
                    evidence_count += 1;
                    for name in names {
                        let n = name.to_string();
                        if !individual_names.contains(&n) {
                            individual_names.push(n);
                        }
                    }
                }
            }
        }
    }

    // Remove the original artist name from the detected names
    individual_names.retain(|n| n.to_lowercase() != artist_name.to_lowercase());

    let source_examples: Vec<String> = paths.iter().take(3).cloned().collect();

    Ok(CompoundArtistCheck {
        is_compound: evidence_count > 0 && !individual_names.is_empty(),
        evidence_count,
        total_sources_checked: paths.len(),
        individual_artist_names: individual_names,
        source_examples,
    })
}

pub async fn apply_artist_fix(
    db: &DbPool,
    artist_id: &str,
    individual_artist_names: &[String],
) -> Result<ArtistFixStats, String> {
    let mut conn = db
        .acquire(format!("apply_artist_fix artist_id={artist_id}"))
        .await
        .map_err(|e| e.to_string())?;
    conn.set_busy_timeout(std::time::Duration::from_secs(30))
        .await
        .map_err(|e| e.to_string())?;
    let mut tx = conn.begin().await.map_err(|e| e.to_string())?;

    // 1. Ensure individual artists exist
    let individual_ids: Vec<String> = {
        let mut ids = Vec::new();
        for name in individual_artist_names {
            let id = get_or_create_artist(&mut tx, name)
                .await
                .map_err(|e| e.to_string())?;
            ids.push(id);
        }
        ids
    };

    // 2. Update recording_artist
    let recording_ids: Vec<String> =
        sqlx::query_scalar("SELECT recording_id FROM recording_artist WHERE artist_id = ?")
            .bind(artist_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;

    let recordings_updated = recording_ids.len();

    for recording_id in &recording_ids {
        sqlx::query("DELETE FROM recording_artist WHERE recording_id = ? AND artist_id = ?")
            .bind(recording_id)
            .bind(artist_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;

        let max_pos: Option<i64> =
            sqlx::query_scalar("SELECT MAX(position) FROM recording_artist WHERE recording_id = ?")
                .bind(recording_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| e.to_string())?
                .flatten();

        let next_pos = max_pos.unwrap_or(-1) + 1;

        for (i, ind_id) in individual_ids.iter().enumerate() {
            let exists: bool = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM recording_artist WHERE recording_id = ? AND artist_id = ?",
            )
            .bind(recording_id)
            .bind(ind_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| e.to_string())?
                > 0;

            if !exists {
                sqlx::query(
                    "INSERT INTO recording_artist (recording_id, artist_id, position, role) VALUES (?, ?, ?, 'main')",
                )
                .bind(recording_id)
                .bind(ind_id)
                .bind(next_pos + i as i64)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
            }
        }
    }

    // 3. Update release_group_artist
    let rg_ids: Vec<String> =
        sqlx::query_scalar("SELECT release_group_id FROM release_group_artist WHERE artist_id = ?")
            .bind(artist_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;

    let release_groups_updated = rg_ids.len();

    for rg_id in &rg_ids {
        sqlx::query(
            "DELETE FROM release_group_artist WHERE release_group_id = ? AND artist_id = ?",
        )
        .bind(rg_id)
        .bind(artist_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

        let max_pos: Option<i64> = sqlx::query_scalar(
            "SELECT MAX(position) FROM release_group_artist WHERE release_group_id = ?",
        )
        .bind(rg_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| e.to_string())?
        .flatten();

        let next_pos = max_pos.unwrap_or(-1) + 1;

        for (i, ind_id) in individual_ids.iter().enumerate() {
            let exists: bool = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM release_group_artist WHERE release_group_id = ? AND artist_id = ?",
            )
            .bind(rg_id)
            .bind(ind_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| e.to_string())?
                > 0;

            if !exists {
                sqlx::query(
                    "INSERT INTO release_group_artist (release_group_id, artist_id, position, role) VALUES (?, ?, ?, 'main')",
                )
                .bind(rg_id)
                .bind(ind_id)
                .bind(next_pos + i as i64)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
            }
        }
    }

    // 4. Delete the compound artist
    sqlx::query("DELETE FROM artist WHERE id = ?")
        .bind(artist_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;

    Ok(ArtistFixStats {
        recordings_updated,
        release_groups_updated,
        compound_artist_deleted: true,
    })
}
