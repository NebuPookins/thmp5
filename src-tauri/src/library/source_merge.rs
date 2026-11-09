//! Handling of sources whose audio file no longer exists on disk: finding them, summarising the
//! user data that hangs off them, and moving that data onto another source (or deleting it).
//!
//! All user data (`play_history`, `source_rating`, `playlist_track`, `source_file_issue`)
//! references `source.id`, and recordings are derived in memory from sources. So merging a dead
//! source into a live one and deleting the dead row is all it takes to fold one recording into
//! another.

use crate::db::DbPool;
use anyhow::{Context, Result};
use sqlx::SqliteConnection;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::Path;
use std::time::Instant;

/// What the user has attached to a source; this is what deleting the source would lose.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SourceUserData {
    pub plays: i64,
    pub stars: Option<i64>,
    pub playlist_entries: i64,
}

impl SourceUserData {
    /// Human-readable description of what removing the source would lose.
    pub fn describe(&self) -> String {
        let plays = (self.plays > 0).then(|| match self.plays {
            1 => "1 play".to_string(),
            n => format!("{n} plays"),
        });
        let rating = self.stars.map(|s| format!("rated {s}★"));
        let playlists = (self.playlist_entries > 0).then(|| match self.playlist_entries {
            1 => "in 1 playlist".to_string(),
            n => format!("in {n} playlists"),
        });
        let parts: Vec<String> = [plays, rating, playlists].into_iter().flatten().collect();
        if parts.is_empty() {
            "no play history or rating".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// A local source whose file was found to be missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSource {
    pub id: String,
    pub file_path: String,
    pub user_data: SourceUserData,
}

impl MissingSource {
    /// The message shown for this source's `MissingFile` issue.
    pub fn issue_message(&self) -> String {
        format!("File not found ({})", self.user_data.describe())
    }
}

/// Whether the file at `path` is definitely gone. Only a "not found" answer counts: any other
/// error (permissions, an unmounted drive that errors differently, ...) is logged and the file
/// is treated as present, so nothing is ever offered for removal on weak evidence.
pub fn file_is_missing(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(_) => false,
        Err(e) if e.kind() == ErrorKind::NotFound => true,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "Could not stat file; treating it as present");
            false
        }
    }
}

/// What is known about a source's file, as checked against the database and the disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceFileState {
    NoSuchSource,
    /// Not a local file (e.g. YouTube), or a local file row without a path.
    NotLocalFile,
    FileExists {
        file_path: String,
    },
    FileMissing {
        file_path: String,
    },
}

/// Look up `source_id` and check its file on disk (blocking I/O runs off the async runtime).
pub async fn source_file_state(
    conn: &mut SqliteConnection,
    source_id: &str,
) -> Result<SourceFileState> {
    let row = sqlx::query!(
        "SELECT source_type, file_path FROM source WHERE id = ?",
        source_id
    )
    .fetch_optional(&mut *conn)
    .await
    .with_context(|| format!("Failed to look up source {source_id}"))?;
    let Some(row) = row else {
        return Ok(SourceFileState::NoSuchSource);
    };
    let Some(file_path) = row.file_path.filter(|_| row.source_type == "local_file") else {
        return Ok(SourceFileState::NotLocalFile);
    };
    let path = file_path.clone();
    let missing = tokio::task::spawn_blocking(move || file_is_missing(Path::new(&path)))
        .await
        .context("File existence check panicked")?;
    Ok(if missing {
        SourceFileState::FileMissing { file_path }
    } else {
        SourceFileState::FileExists { file_path }
    })
}

/// The user data attached to a source.
pub async fn user_data_of(conn: &mut SqliteConnection, source_id: &str) -> Result<SourceUserData> {
    let plays = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM play_history WHERE source_id = ?"#,
        source_id
    )
    .fetch_one(&mut *conn)
    .await
    .with_context(|| format!("Failed to count plays of source {source_id}"))?;
    let stars = sqlx::query_scalar!(
        "SELECT stars FROM source_rating WHERE source_id = ?",
        source_id
    )
    .fetch_optional(&mut *conn)
    .await
    .with_context(|| format!("Failed to load rating of source {source_id}"))?;
    let playlist_entries = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM playlist_track WHERE source_id = ?"#,
        source_id
    )
    .fetch_one(&mut *conn)
    .await
    .with_context(|| format!("Failed to count playlist entries of source {source_id}"))?;
    Ok(SourceUserData {
        plays,
        stars,
        playlist_entries,
    })
}

/// The user data of each of `source_ids`, in one batched query. Sources with no user data are
/// present with the default (zero) value.
async fn user_data_of_many(
    conn: &mut SqliteConnection,
    source_ids: &[&str],
) -> Result<HashMap<String, SourceUserData>> {
    let ids_json = serde_json::to_string(source_ids).context("Failed to encode source ids")?;
    let plays = sqlx::query!(
        r#"SELECT source_id AS "source_id!", COUNT(*) AS "n!: i64" FROM play_history
           WHERE source_id IN (SELECT value FROM json_each(?)) GROUP BY source_id"#,
        ids_json
    )
    .fetch_all(&mut *conn)
    .await
    .context("Failed to count plays of missing sources")?;
    let ratings = sqlx::query!(
        r#"SELECT source_id AS "source_id!", stars FROM source_rating
           WHERE source_id IN (SELECT value FROM json_each(?))"#,
        ids_json
    )
    .fetch_all(&mut *conn)
    .await
    .context("Failed to load ratings of missing sources")?;
    let playlist_entries = sqlx::query!(
        r#"SELECT source_id AS "source_id!", COUNT(*) AS "n!: i64" FROM playlist_track
           WHERE source_id IN (SELECT value FROM json_each(?)) GROUP BY source_id"#,
        ids_json
    )
    .fetch_all(&mut *conn)
    .await
    .context("Failed to count playlist entries of missing sources")?;

    let mut data: HashMap<String, SourceUserData> = HashMap::new();
    for r in plays {
        data.entry(r.source_id).or_default().plays = r.n;
    }
    for r in ratings {
        data.entry(r.source_id).or_default().stars = Some(r.stars);
    }
    for r in playlist_entries {
        data.entry(r.source_id).or_default().playlist_entries = r.n;
    }
    Ok(data)
}

/// Every local source whose file is missing from disk, with the user data it carries.
pub async fn find_missing_sources(db: &DbPool) -> Result<Vec<MissingSource>> {
    let rows = {
        let mut conn = db.acquire("library.find_missing_sources.list").await?;
        sqlx::query!(
            r#"SELECT id AS "id!", file_path AS "file_path!" FROM source
               WHERE source_type = 'local_file' AND file_path IS NOT NULL"#
        )
        .fetch_all(&mut *conn)
        .await
        .context("Failed to list local sources")?
    };
    let checked = rows.len();
    let started = Instant::now();
    let missing_rows = tokio::task::spawn_blocking(move || {
        rows.into_iter()
            .filter(|r| file_is_missing(Path::new(&r.file_path)))
            .collect::<Vec<_>>()
    })
    .await
    .context("Missing-file scan panicked")?;
    tracing::debug!(
        checked,
        missing = missing_rows.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "Scanned local sources for missing files"
    );
    if missing_rows.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<&str> = missing_rows.iter().map(|r| r.id.as_str()).collect();
    let mut user_data = {
        let mut conn = db.acquire("library.find_missing_sources.user_data").await?;
        user_data_of_many(&mut conn, &ids).await?
    };
    Ok(missing_rows
        .into_iter()
        .map(|row| MissingSource {
            user_data: user_data.remove(&row.id).unwrap_or_default(),
            id: row.id,
            file_path: row.file_path,
        })
        .collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeSummary {
    pub plays_moved: u64,
    pub playlist_entries_moved: u64,
}

/// Move all user data from `dead` onto `target` and delete the `dead` source row.
///
/// Run inside a transaction under the write serializer. Plays and playlist entries move as they
/// are; for a rating clash the more recently updated one wins (ties go to the target); notes are
/// concatenated, target first.
pub async fn merge_source_into(
    tx: &mut SqliteConnection,
    dead: &str,
    target: &str,
) -> Result<MergeSummary> {
    anyhow::ensure!(dead != target, "Cannot merge source {dead} into itself");

    let plays_moved = sqlx::query!(
        "UPDATE play_history SET source_id = ? WHERE source_id = ?",
        target,
        dead
    )
    .execute(&mut *tx)
    .await
    .context("Failed to move play history")?
    .rows_affected();

    let playlist_entries_moved = sqlx::query!(
        "UPDATE playlist_track SET source_id = ? WHERE source_id = ?",
        target,
        dead
    )
    .execute(&mut *tx)
    .await
    .context("Failed to move playlist entries")?
    .rows_affected();

    sqlx::query!(
        "INSERT INTO source_rating (source_id, stars, updated_at)
         SELECT ?, stars, updated_at FROM source_rating WHERE source_id = ?
         ON CONFLICT(source_id) DO UPDATE SET
             stars = excluded.stars,
             updated_at = excluded.updated_at
         WHERE excluded.updated_at > source_rating.updated_at",
        target,
        dead
    )
    .execute(&mut *tx)
    .await
    .context("Failed to merge ratings")?;

    sqlx::query!(
        "INSERT INTO source_file_issue (source_id, note)
         SELECT ?, note FROM source_file_issue WHERE source_id = ?
         ON CONFLICT(source_id) DO UPDATE SET
             note = source_file_issue.note || char(10) || char(10) || excluded.note,
             updated_at = datetime('now')",
        target,
        dead
    )
    .execute(&mut *tx)
    .await
    .context("Failed to merge notes")?;

    delete_source(tx, dead).await?;
    Ok(MergeSummary {
        plays_moved,
        playlist_entries_moved,
    })
}

/// Delete a source row; foreign keys cascade its plays, rating, playlist entries and note.
pub async fn delete_source(conn: &mut SqliteConnection, source_id: &str) -> Result<()> {
    let deleted = sqlx::query!("DELETE FROM source WHERE id = ?", source_id)
        .execute(&mut *conn)
        .await
        .with_context(|| format!("Failed to delete source {source_id}"))?
        .rows_affected();
    anyhow::ensure!(
        deleted == 1,
        "Expected to delete exactly one source row for {source_id}, deleted {deleted}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init_pool;
    use sqlx::Acquire;
    use tempfile::TempDir;

    async fn insert_source(db: &DbPool, id: &str, path: &str) {
        let mut conn = db.acquire("test.insert_source").await.unwrap();
        sqlx::query!(
            "INSERT INTO source (id, source_type, file_path) VALUES (?, 'local_file', ?)",
            id,
            path
        )
        .execute(&mut *conn)
        .await
        .unwrap();
    }

    async fn add_plays(db: &DbPool, id: &str, n: u32) {
        let mut conn = db.acquire("test.add_plays").await.unwrap();
        for _ in 0..n {
            sqlx::query!("INSERT INTO play_history (source_id) VALUES (?)", id)
                .execute(&mut *conn)
                .await
                .unwrap();
        }
    }

    async fn set_rating(db: &DbPool, id: &str, stars: i64, updated_at: &str) {
        let mut conn = db.acquire("test.set_rating").await.unwrap();
        sqlx::query!(
            "INSERT INTO source_rating (source_id, stars, updated_at) VALUES (?, ?, ?)",
            id,
            stars,
            updated_at
        )
        .execute(&mut *conn)
        .await
        .unwrap();
    }

    async fn set_note(db: &DbPool, id: &str, note: &str) {
        let mut conn = db.acquire("test.set_note").await.unwrap();
        sqlx::query!(
            "INSERT INTO source_file_issue (source_id, note) VALUES (?, ?)",
            id,
            note
        )
        .execute(&mut *conn)
        .await
        .unwrap();
    }

    async fn add_to_playlist(db: &DbPool, id: &str) {
        let mut conn = db.acquire("test.add_to_playlist").await.unwrap();
        sqlx::query!("INSERT INTO playlist (name, kind) VALUES ('p', 'static')")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query!(
            "INSERT INTO playlist_track (playlist_id, source_id, position)
             SELECT id, ?, 0 FROM playlist WHERE name = 'p'",
            id
        )
        .execute(&mut *conn)
        .await
        .unwrap();
    }

    async fn rating_of(db: &DbPool, id: &str) -> Option<(i64, String)> {
        let mut conn = db.acquire("test.rating_of").await.unwrap();
        sqlx::query!(
            r#"SELECT stars, updated_at AS "updated_at!" FROM source_rating WHERE source_id = ?"#,
            id
        )
        .fetch_optional(&mut *conn)
        .await
        .unwrap()
        .map(|r| (r.stars, r.updated_at))
    }

    async fn note_of(db: &DbPool, id: &str) -> Option<String> {
        let mut conn = db.acquire("test.note_of").await.unwrap();
        sqlx::query_scalar!("SELECT note FROM source_file_issue WHERE source_id = ?", id)
            .fetch_optional(&mut *conn)
            .await
            .unwrap()
    }

    async fn source_exists(db: &DbPool, id: &str) -> bool {
        let mut conn = db.acquire("test.source_exists").await.unwrap();
        sqlx::query_scalar!("SELECT id FROM source WHERE id = ?", id)
            .fetch_optional(&mut *conn)
            .await
            .unwrap()
            .is_some()
    }

    async fn merge(db: &DbPool, dead: &str, target: &str) -> MergeSummary {
        let mut conn = db.acquire("test.merge").await.unwrap();
        let mut tx = conn.begin().await.unwrap();
        let summary = merge_source_into(&mut tx, dead, target).await.unwrap();
        tx.commit().await.unwrap();
        summary
    }

    async fn setup() -> (TempDir, DbPool) {
        let tmp = TempDir::new().unwrap();
        let db = init_pool(&tmp.path().join("test.db")).await.unwrap();
        insert_source(&db, "dead", "/gone/dead.mp3").await;
        insert_source(&db, "live", "/here/live.mp3").await;
        (tmp, db)
    }

    #[tokio::test]
    async fn merge_moves_plays_and_playlist_entries_and_deletes_dead_source() {
        let (_tmp, db) = setup().await;
        add_plays(&db, "dead", 3).await;
        add_plays(&db, "live", 1).await;
        add_to_playlist(&db, "dead").await;

        let summary = merge(&db, "dead", "live").await;

        assert_eq!(summary.plays_moved, 3);
        assert_eq!(summary.playlist_entries_moved, 1);
        let mut conn = db.acquire("test.check").await.unwrap();
        let live = user_data_of(&mut conn, "live").await.unwrap();
        assert_eq!(live.plays, 4);
        assert_eq!(live.playlist_entries, 1);
        drop(conn);
        assert!(!source_exists(&db, "dead").await);
        assert!(source_exists(&db, "live").await);
    }

    #[tokio::test]
    async fn merge_moves_rating_when_only_dead_source_is_rated() {
        let (_tmp, db) = setup().await;
        set_rating(&db, "dead", 4, "2020-01-01 00:00:00").await;

        merge(&db, "dead", "live").await;

        assert_eq!(
            rating_of(&db, "live").await.map(|(stars, _)| stars),
            Some(4)
        );
    }

    #[tokio::test]
    async fn merge_keeps_the_more_recent_rating_when_dead_is_newer() {
        let (_tmp, db) = setup().await;
        set_rating(&db, "dead", 5, "2022-01-01 00:00:00").await;
        set_rating(&db, "live", 2, "2021-01-01 00:00:00").await;

        merge(&db, "dead", "live").await;

        assert_eq!(rating_of(&db, "live").await.map(|(s, _)| s), Some(5));
    }

    #[tokio::test]
    async fn merge_keeps_target_rating_when_target_is_newer_or_tied() {
        let (_tmp, db) = setup().await;
        set_rating(&db, "dead", 5, "2021-01-01 00:00:00").await;
        set_rating(&db, "live", 2, "2022-01-01 00:00:00").await;
        merge(&db, "dead", "live").await;
        assert_eq!(rating_of(&db, "live").await.map(|(s, _)| s), Some(2));

        let (_tmp, db) = setup().await;
        set_rating(&db, "dead", 5, "2021-01-01 00:00:00").await;
        set_rating(&db, "live", 2, "2021-01-01 00:00:00").await;
        merge(&db, "dead", "live").await;
        assert_eq!(rating_of(&db, "live").await.map(|(s, _)| s), Some(2));
    }

    #[tokio::test]
    async fn merge_concatenates_notes_target_first() {
        let (_tmp, db) = setup().await;
        set_note(&db, "dead", "from dead").await;
        set_note(&db, "live", "from live").await;

        merge(&db, "dead", "live").await;

        assert_eq!(
            note_of(&db, "live").await.as_deref(),
            Some("from live\n\nfrom dead")
        );
    }

    #[tokio::test]
    async fn merge_moves_note_when_only_dead_source_has_one() {
        let (_tmp, db) = setup().await;
        set_note(&db, "dead", "only dead").await;

        merge(&db, "dead", "live").await;

        assert_eq!(note_of(&db, "live").await.as_deref(), Some("only dead"));
    }

    #[tokio::test]
    async fn merging_a_source_into_itself_is_an_error() {
        let (_tmp, db) = setup().await;
        let mut conn = db.acquire("test.self_merge").await.unwrap();
        let mut tx = conn.begin().await.unwrap();
        assert!(merge_source_into(&mut tx, "dead", "dead").await.is_err());
        drop(tx);
        drop(conn);
        assert!(source_exists(&db, "dead").await);
    }

    #[tokio::test]
    async fn delete_source_removes_its_user_data() {
        let (_tmp, db) = setup().await;
        add_plays(&db, "dead", 2).await;
        set_rating(&db, "dead", 3, "2020-01-01 00:00:00").await;
        set_note(&db, "dead", "n").await;
        add_to_playlist(&db, "dead").await;

        let mut conn = db.acquire("test.delete").await.unwrap();
        delete_source(&mut conn, "dead").await.unwrap();
        drop(conn);

        assert!(!source_exists(&db, "dead").await);
        assert_eq!(rating_of(&db, "dead").await, None);
        assert_eq!(note_of(&db, "dead").await, None);
        let mut conn = db.acquire("test.check").await.unwrap();
        let data = user_data_of(&mut conn, "dead").await.unwrap();
        assert_eq!(data, SourceUserData::default());
    }

    #[tokio::test]
    async fn source_file_state_reports_missing_and_existing_files() {
        let (tmp, db) = setup().await;
        let real = tmp.path().join("real.mp3");
        std::fs::write(&real, b"x").unwrap();
        insert_source(&db, "real", &real.to_string_lossy()).await;

        let mut conn = db.acquire("test.state").await.unwrap();
        assert_eq!(
            source_file_state(&mut conn, "dead").await.unwrap(),
            SourceFileState::FileMissing {
                file_path: "/gone/dead.mp3".into()
            }
        );
        assert!(matches!(
            source_file_state(&mut conn, "real").await.unwrap(),
            SourceFileState::FileExists { .. }
        ));
        assert_eq!(
            source_file_state(&mut conn, "nope").await.unwrap(),
            SourceFileState::NoSuchSource
        );
    }

    #[tokio::test]
    async fn find_missing_sources_lists_only_missing_files_with_their_user_data() {
        let (tmp, db) = setup().await;
        let real = tmp.path().join("real.mp3");
        std::fs::write(&real, b"x").unwrap();
        insert_source(&db, "real", &real.to_string_lossy()).await;
        add_plays(&db, "dead", 2).await;
        set_rating(&db, "dead", 4, "2020-01-01 00:00:00").await;

        let missing = find_missing_sources(&db).await.unwrap();
        let ids: Vec<&str> = missing.iter().map(|m| m.id.as_str()).collect();

        assert!(ids.contains(&"dead"));
        assert!(ids.contains(&"live"));
        assert!(!ids.contains(&"real"));
        let dead = missing.iter().find(|m| m.id == "dead").unwrap();
        assert_eq!(dead.user_data.plays, 2);
        assert_eq!(dead.user_data.stars, Some(4));
    }

    #[test]
    fn describe_summarises_what_would_be_lost() {
        assert_eq!(
            SourceUserData::default().describe(),
            "no play history or rating"
        );
        let data = SourceUserData {
            plays: 3,
            stars: Some(4),
            playlist_entries: 1,
        };
        assert_eq!(data.describe(), "3 plays, rated 4★, in 1 playlist");
    }
}
