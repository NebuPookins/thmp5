use crate::db::DbPool;
use crate::models::{
    ArtistDetail, ArtistReleaseGroup, ArtistRow, GuestAppearanceReleaseGroup, GuestAppearanceTrack,
    LibrarySummary, MediumDetail, MissingTrackDetail, RecordingArtistInfo, RecordingDetail,
    RecordingRow, ReleaseCompleteness, ReleaseDetail, ReleaseGroupDetail, ReleaseGroupRow,
    ReleaseInfo, SmartPlaylistResult, SourceDetail, SourceDisagreementGroup, TrackDetail,
};
use crate::query::LimitUnit;
use anyhow::Result;
use nonempty::NonEmpty;
use sqlx::{Row, SqliteConnection};
use std::collections::BTreeMap;

pub struct SqlCatalog {
    db: DbPool,
}

impl SqlCatalog {
    pub fn new(db: DbPool) -> Self {
        Self { db }
    }
}

/// CTEs shared by list_recordings and evaluate_smart_playlist.
const RECORDING_CTES: &str = "
  ph_agg AS (
    SELECT recording_id,
           COUNT(*)       AS play_count,
           MAX(played_at) AS last_played
    FROM play_history
    GROUP BY recording_id
  ),
  src_primary AS (
    SELECT recording_id, MIN(file_path) AS file_path
    FROM source
    WHERE source_type = 'local_file' AND file_path IS NOT NULL
    GROUP BY recording_id
  ),
  tags_agg AS (
    SELECT recording_id, GROUP_CONCAT(tag, char(0)) AS tags_raw
    FROM (SELECT recording_id, tag FROM recording_tag ORDER BY recording_id, tag)
    GROUP BY recording_id
  ),
  source_paths_agg AS (
    SELECT recording_id, GROUP_CONCAT(file_path, char(0)) AS source_paths_raw
    FROM (
      SELECT recording_id, file_path FROM source
      WHERE source_type = 'local_file' AND file_path IS NOT NULL
      ORDER BY recording_id, file_path
    )
    GROUP BY recording_id
  ),
  artists_agg AS (
    SELECT recording_id, GROUP_CONCAT(artist_id, char(0)) AS artist_ids_raw
    FROM (
      SELECT recording_id, artist_id
      FROM recording_artist
      ORDER BY recording_id, position
    )
    GROUP BY recording_id
  ),
  releases_agg AS (
    SELECT recording_id, GROUP_CONCAT(entry, char(0)) AS releases_raw
    FROM (
      SELECT t2.recording_id,
             rg2.id || char(1) ||
             rg2.title || char(1) ||
             COALESCE(CAST(t2.position AS TEXT), '') || char(1) ||
             COALESCE(CAST(m2.position AS TEXT), '') || char(1) ||
             COALESCE(CAST((SELECT MAX(m3.position) FROM medium m3 WHERE m3.release_id = rel2.id) AS TEXT), '') AS entry
      FROM track t2
      JOIN medium m2         ON m2.id = t2.medium_id
      JOIN release rel2      ON rel2.id = m2.release_id
      JOIN release_group rg2 ON rg2.id = rel2.release_group_id
      ORDER BY t2.recording_id, rg2.title, m2.position, t2.position
    )
    GROUP BY recording_id
  )
";

fn parse_recording_row(row: &sqlx::sqlite::SqliteRow) -> RecordingRow {
    RecordingRow {
        id: row.get("id"),
        title: row.get("title"),
        duration_ms: row.get("duration_ms"),
        primary_artist_id: row.get("primary_artist_id"),
        artist_credit_name: row.get("artist_credit_name"),
        genre: row.get("genre"),
        rating: row.get("rating"),
        play_count: row.get::<Option<i64>, _>("play_count").unwrap_or(0),
        last_played: row.get("last_played"),
        primary_source_id: row.get("primary_source_id"),
        primary_source_path: row.get("primary_source_path"),
        tags: {
            let raw: Option<String> = row.get("tags_raw");
            raw.map(|r| {
                r.split('\0')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
        },
        artist_ids: {
            let raw: Option<String> = row.get("artist_ids_raw");
            raw.map(|r| {
                r.split('\0')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
        },
        source_paths: {
            let raw: Option<String> = row.get("source_paths_raw");
            raw.map(|r| {
                r.split('\0')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
        },
        releases: {
            let raw: Option<String> = row.get("releases_raw");
            raw.map(|r| {
                r.split('\0')
                    .filter(|s| !s.is_empty())
                    .map(|entry| {
                        let mut parts = entry.splitn(5, '\x01');
                        let release_group_id = parts.next().unwrap_or("").to_string();
                        let release_group_title = parts.next().unwrap_or("").to_string();
                        let track_position =
                            parts.next().and_then(
                                |s| {
                                    if s.is_empty() {
                                        None
                                    } else {
                                        s.parse().ok()
                                    }
                                },
                            );
                        let disc_position =
                            parts.next().and_then(
                                |s| {
                                    if s.is_empty() {
                                        None
                                    } else {
                                        s.parse().ok()
                                    }
                                },
                            );
                        let disc_total =
                            parts.next().and_then(
                                |s| {
                                    if s.is_empty() {
                                        None
                                    } else {
                                        s.parse().ok()
                                    }
                                },
                            );
                        ReleaseInfo {
                            release_group_id,
                            release_group_title,
                            track_position,
                            disc_position,
                            disc_total,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
        },
    }
}

async fn backfill_track_totals_for_release(
    conn: &mut SqliteConnection,
    release_id: &str,
) -> Result<bool> {
    let candidates = sqlx::query(
        "SELECT s.id, s.file_path
         FROM source s
         JOIN track t ON t.recording_id = s.recording_id
         JOIN medium m ON m.id = t.medium_id
         WHERE m.release_id = ?
           AND s.source_type = 'local_file'
           AND s.file_path IS NOT NULL
           AND s.track_total IS NULL",
    )
    .bind(release_id)
    .fetch_all(&mut *conn)
    .await?;

    if candidates.is_empty() {
        return Ok(false);
    }

    let mut updated = 0u32;
    for row in &candidates {
        let path: String = row.get("file_path");
        let sid: String = row.get("id");
        match crate::library::scanner::read_metadata(std::path::Path::new(&path)) {
            Ok(result) => {
                if let Some(tt) = result.meta.track_total {
                    let n = sqlx::query("UPDATE source SET track_total = ? WHERE id = ?")
                        .bind(tt as i64)
                        .bind(&sid)
                        .execute(&mut *conn)
                        .await?
                        .rows_affected();
                    if n > 0 {
                        updated += 1;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to read tags for track_total backfill ({}): {e:#}",
                    path
                );
            }
        }
    }

    Ok(updated > 0)
}

impl super::CatalogReader for SqlCatalog {
    async fn list_recordings(&self) -> Result<Vec<RecordingRow>> {
        let mut conn = self.db.acquire("catalog.list_recordings").await?;

        let rows = sqlx::query(&format!(
            "WITH {RECORDING_CTES}
             SELECT
                r.id,
                r.title,
                r.duration_ms,
                a.id                             AS primary_artist_id,
                COALESCE(ra.credited_as, a.name) AS artist_credit_name,
                r.genre,
                ur.stars                         AS rating,
                COALESCE(ph.play_count, 0)       AS play_count,
                ph.last_played,
                ps.id                            AS primary_source_id,
                sp.file_path                     AS primary_source_path,
                ta.tags_raw,
                spa.source_paths_raw,
                aa.artist_ids_raw,
                ragg.releases_raw
             FROM recording r
             LEFT JOIN recording_artist ra  ON ra.recording_id = r.id AND ra.position = 0
             LEFT JOIN artist a              ON a.id = ra.artist_id
             LEFT JOIN user_rating ur        ON ur.recording_id = r.id
             LEFT JOIN ph_agg ph             ON ph.recording_id = r.id
             LEFT JOIN src_primary sp        ON sp.recording_id = r.id
             LEFT JOIN source ps             ON ps.file_path = sp.file_path
             LEFT JOIN tags_agg ta           ON ta.recording_id = r.id
             LEFT JOIN source_paths_agg spa  ON spa.recording_id = r.id
             LEFT JOIN artists_agg aa        ON aa.recording_id = r.id
             LEFT JOIN releases_agg ragg     ON ragg.recording_id = r.id
             ORDER BY lower(a.sort_name), lower(r.title)"
        ))
        .fetch_all(&mut *conn)
        .await?;

        Ok(rows.iter().map(parse_recording_row).collect())
    }

    async fn list_artists(&self) -> Result<Vec<ArtistRow>> {
        let mut conn = self.db.acquire("catalog.list_artists").await?;

        let rows = sqlx::query(
            "SELECT
                a.id,
                a.name,
                a.sort_name,
                COUNT(DISTINCT rga.release_group_id) AS release_group_count,
                COUNT(DISTINCT ra.recording_id)      AS recording_count,
                AVG(ur.stars)                        AS rating,
                MAX(ph.played_at)                    AS last_played
             FROM artist a
             LEFT JOIN recording_artist ra      ON ra.artist_id = a.id
             LEFT JOIN release_group_artist rga ON rga.artist_id = a.id
             LEFT JOIN user_rating ur           ON ur.recording_id = ra.recording_id
             LEFT JOIN play_history ph          ON ph.recording_id = ra.recording_id
             GROUP BY a.id
             ORDER BY lower(a.sort_name)",
        )
        .fetch_all(&mut *conn)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| ArtistRow {
                id: row.get("id"),
                name: row.get("name"),
                sort_name: row.get("sort_name"),
                release_group_count: row
                    .get::<Option<i64>, _>("release_group_count")
                    .unwrap_or(0),
                recording_count: row.get::<Option<i64>, _>("recording_count").unwrap_or(0),
                rating: row.get("rating"),
                last_played: row.get("last_played"),
            })
            .collect())
    }

    async fn list_release_groups(
        &self,
        artist_id: Option<&str>,
        search: Option<&str>,
    ) -> Result<Vec<ReleaseGroupRow>> {
        let mut conn = self
            .db
            .acquire(format!(
                "catalog.list_release_groups artist_id={} search={}",
                artist_id.unwrap_or("<all>"),
                search.unwrap_or("<none>")
            ))
            .await?;

        let rows = sqlx::query(
            "SELECT
                rg.id,
                rg.title,
                COALESCE(rg.artist_credit_text, rga.credited_as, a.name) AS artist_credit_name,
                a.id                                                     AS primary_artist_id,
                COUNT(DISTINCT rel.id)                                   AS release_count,
                COUNT(DISTINCT t.recording_id)                           AS recording_count,
                MIN(rel.release_date)                                    AS release_date,
                (
                    SELECT AVG(track_ratings.stars)
                    FROM (
                        SELECT DISTINCT t2.recording_id, ur2.stars
                        FROM release rel2
                        JOIN medium m2
                            ON m2.release_id = rel2.id
                        JOIN track t2
                            ON t2.medium_id = m2.id
                        JOIN user_rating ur2
                            ON ur2.recording_id = t2.recording_id
                        WHERE rel2.release_group_id = rg.id
                    ) AS track_ratings
                )                                                        AS rating,
                (
                    SELECT MAX(ph2.played_at)
                    FROM release rel2
                    JOIN medium m2
                        ON m2.release_id = rel2.id
                    JOIN track t2
                        ON t2.medium_id = m2.id
                    JOIN play_history ph2
                        ON ph2.recording_id = t2.recording_id
                    WHERE rel2.release_group_id = rg.id
                )                                                        AS last_played
             FROM release_group rg
             LEFT JOIN release_group_artist rga
                 ON rga.release_group_id = rg.id AND rga.position = 0
             LEFT JOIN artist a
                 ON a.id = rga.artist_id
             LEFT JOIN release rel
                 ON rel.release_group_id = rg.id
             LEFT JOIN medium m
                 ON m.release_id = rel.id
             LEFT JOIN track t
                 ON t.medium_id = m.id
             WHERE (
                   ? IS NULL
                   OR a.id = ?
                   OR EXISTS (
                       SELECT 1 FROM recording_artist ra2
                       JOIN track t2 ON t2.recording_id = ra2.recording_id
                       JOIN medium m2 ON m2.id = t2.medium_id
                       JOIN release rel2 ON rel2.id = m2.release_id
                       WHERE rel2.release_group_id = rg.id AND ra2.artist_id = ?
                   )
               )
               AND (
                   ? IS NULL
                   OR lower(rg.title) LIKE '%' || lower(?) || '%'
                   OR lower(COALESCE(rg.artist_credit_text, rga.credited_as, a.name, '')) LIKE '%' || lower(?) || '%'
               )
             GROUP BY rg.id
             ORDER BY lower(COALESCE(a.sort_name, a.name, '')), lower(rg.title)",
        )
        .bind(artist_id)
        .bind(artist_id)
        .bind(artist_id)
        .bind(search)
        .bind(search)
        .bind(search)
        .fetch_all(&mut *conn)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| ReleaseGroupRow {
                id: row.get("id"),
                title: row.get("title"),
                artist_credit_name: row.get("artist_credit_name"),
                primary_artist_id: row.get("primary_artist_id"),
                release_count: row.get::<Option<i64>, _>("release_count").unwrap_or(0),
                recording_count: row.get::<Option<i64>, _>("recording_count").unwrap_or(0),
                release_date: row.get("release_date"),
                rating: row.get("rating"),
                last_played: row.get("last_played"),
            })
            .collect())
    }

    async fn get_library_summary(&self) -> Result<LibrarySummary> {
        let mut conn = self.db.acquire("catalog.get_library_summary").await?;

        let recording_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM recording")
            .fetch_one(&mut *conn)
            .await?;
        let artist_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artist")
            .fetch_one(&mut *conn)
            .await?;
        let release_group_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM release_group")
            .fetch_one(&mut *conn)
            .await?;
        let source_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM source")
            .fetch_one(&mut *conn)
            .await?;

        Ok(LibrarySummary {
            recording_count,
            artist_count,
            release_group_count,
            source_count,
        })
    }

    async fn list_all_tags(&self) -> Result<Vec<String>> {
        let mut conn = self.db.acquire("catalog.list_all_tags").await?;
        let tags =
            sqlx::query_scalar::<_, String>("SELECT DISTINCT tag FROM recording_tag ORDER BY tag")
                .fetch_all(&mut *conn)
                .await?;
        Ok(tags)
    }

    async fn evaluate_smart_playlist(&self, query: &str) -> Result<SmartPlaylistResult> {
        let parsed = crate::query::parse(query)?;
        let (where_sql, limit) = crate::query::compile(&parsed);

        let full_sql = format!(
            "WITH {RECORDING_CTES}
             SELECT
                r.id,
                r.title,
                r.duration_ms,
                a.id                             AS primary_artist_id,
                COALESCE(ra.credited_as, a.name) AS artist_credit_name,
                r.genre,
                ur.stars                         AS rating,
                COALESCE(ph.play_count, 0)       AS play_count,
                ph.last_played,
                ps.id                            AS primary_source_id,
                sp.file_path                     AS primary_source_path,
                ta.tags_raw,
                spa.source_paths_raw,
                aa.artist_ids_raw,
                ragg.releases_raw
             FROM recording r
             LEFT JOIN recording_artist ra  ON ra.recording_id = r.id AND ra.position = 0
             LEFT JOIN artist a              ON a.id = ra.artist_id
             LEFT JOIN user_rating ur        ON ur.recording_id = r.id
             LEFT JOIN ph_agg ph             ON ph.recording_id = r.id
             LEFT JOIN src_primary sp        ON sp.recording_id = r.id
             LEFT JOIN source ps             ON ps.file_path = sp.file_path
             LEFT JOIN tags_agg ta           ON ta.recording_id = r.id
             LEFT JOIN source_paths_agg spa  ON spa.recording_id = r.id
             LEFT JOIN artists_agg aa        ON aa.recording_id = r.id
             LEFT JOIN releases_agg ragg     ON ragg.recording_id = r.id
             WHERE r.id IN (
                 SELECT spv.id FROM smart_playlist_view spv WHERE {where_sql}
             )
             ORDER BY RANDOM()"
        );

        tracing::debug!(sql = %full_sql, "Evaluating smart playlist");

        let mut conn = self
            .db
            .acquire(format!("catalog.evaluate_smart_playlist query={query}"))
            .await?;
        let rows = sqlx::query(&full_sql)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| anyhow::anyhow!("Query error: {e}"))?;

        let mut recordings: Vec<_> = rows
            .into_iter()
            .map(|row| parse_recording_row(&row))
            .collect();

        if let Some(lim) = limit {
            match lim.unit {
                LimitUnit::Tracks => {
                    recordings.truncate(lim.value as usize);
                }
                LimitUnit::Minutes | LimitUnit::Hours => {
                    let target_ms = match lim.unit {
                        LimitUnit::Minutes => lim.value * 60_000,
                        LimitUnit::Hours => lim.value * 3_600_000,
                        LimitUnit::Tracks => unreachable!(),
                    };
                    let mut accumulated: i64 = 0;
                    recordings.retain(|r| {
                        if accumulated >= target_ms {
                            return false;
                        }
                        accumulated += r.duration_ms.unwrap_or(0);
                        true
                    });
                }
            }
        }

        let total_duration_ms = recordings.iter().map(|r| r.duration_ms.unwrap_or(0)).sum();

        Ok(SmartPlaylistResult {
            recordings,
            total_duration_ms,
            sql: full_sql,
        })
    }

    async fn get_artist_detail(&self, id: &str) -> Result<Option<ArtistDetail>> {
        let mut conn = self
            .db
            .acquire(format!("catalog.get_artist_detail id={id}"))
            .await?;

        let artist_row = sqlx::query(
            "SELECT a.id, a.name, a.sort_name, a.mbid,
                    AVG(ur.stars) AS rating,
                    MAX(ph.played_at) AS last_played,
                    COUNT(DISTINCT ra.recording_id) AS recording_count
             FROM artist a
             LEFT JOIN recording_artist ra ON ra.artist_id = a.id
             LEFT JOIN user_rating ur ON ur.recording_id = ra.recording_id
             LEFT JOIN play_history ph ON ph.recording_id = ra.recording_id
             WHERE a.id = ?
             GROUP BY a.id",
        )
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;

        let Some(artist_row) = artist_row else {
            return Ok(None);
        };

        let rg_rows = sqlx::query(
            "SELECT rg.id, rg.title, rg.rg_type,
                    MIN(rel.release_date) AS release_date,
                    COUNT(DISTINCT t.recording_id) AS recording_count,
                    (SELECT AVG(ur2.stars)
                     FROM release rel2
                     JOIN medium m2 ON m2.release_id = rel2.id
                     JOIN track t2 ON t2.medium_id = m2.id
                     JOIN user_rating ur2 ON ur2.recording_id = t2.recording_id
                     WHERE rel2.release_group_id = rg.id) AS rating,
                    COALESCE(rg.artist_credit_text, rga.credited_as, a2.name) AS artist_credit_name,
                    a2.id AS primary_artist_id
             FROM release_group rg
             JOIN release_group_artist rga ON rga.release_group_id = rg.id AND rga.position = 0
             JOIN artist a2 ON a2.id = rga.artist_id
             LEFT JOIN release rel ON rel.release_group_id = rg.id
             LEFT JOIN medium m ON m.release_id = rel.id
             LEFT JOIN track t ON t.medium_id = m.id
             WHERE rga.artist_id = ?
             GROUP BY rg.id
             ORDER BY rg.title",
        )
        .bind(id)
        .fetch_all(&mut *conn)
        .await?;

        let release_groups: Vec<ArtistReleaseGroup> = rg_rows
            .into_iter()
            .map(|row| ArtistReleaseGroup {
                id: row.get("id"),
                title: row.get("title"),
                rg_type: row.get("rg_type"),
                release_date: row.get("release_date"),
                recording_count: row.get::<Option<i64>, _>("recording_count").unwrap_or(0),
                rating: row.get("rating"),
                primary_artist_id: row.get("primary_artist_id"),
                artist_credit_name: row.get("artist_credit_name"),
            })
            .collect();

        let ga_rg_rows = sqlx::query(
            "SELECT rg.id, rg.title, rg.rg_type,
                    MIN(rel.release_date) AS release_date,
                    COALESCE(rg.artist_credit_text, rga.credited_as, a2.name) AS artist_credit_name,
                    a2.id AS primary_artist_id
             FROM recording_artist ra
             JOIN track t ON t.recording_id = ra.recording_id
             JOIN medium m ON m.id = t.medium_id
             JOIN release rel ON rel.id = m.release_id
             JOIN release_group rg ON rg.id = rel.release_group_id
             LEFT JOIN release_group_artist rga ON rga.release_group_id = rg.id AND rga.position = 0
             LEFT JOIN artist a2 ON a2.id = rga.artist_id
             WHERE ra.artist_id = ?
               AND ra.position > 0
               AND NOT EXISTS (
                   SELECT 1 FROM release_group_artist rga2
                   WHERE rga2.release_group_id = rg.id
                     AND rga2.artist_id = ra.artist_id
                     AND rga2.position = 0
               )
             GROUP BY rg.id
             ORDER BY rg.title",
        )
        .bind(id)
        .fetch_all(&mut *conn)
        .await?;

        let mut guest_appearances = Vec::new();
        for rg_row in ga_rg_rows {
            let rg_id: String = rg_row.get("id");

            let track_rows = sqlx::query(
                "SELECT t.recording_id, r.title AS recording_title,
                        t.position AS track_position,
                        m.position AS disc_position
                 FROM recording_artist ra
                 JOIN track t ON t.recording_id = ra.recording_id
                 JOIN medium m ON m.id = t.medium_id
                 JOIN release rel ON rel.id = m.release_id
                 JOIN release_group rg ON rg.id = rel.release_group_id
                 JOIN recording r ON r.id = t.recording_id
                 WHERE ra.artist_id = ?
                   AND ra.position > 0
                   AND rg.id = ?
                   AND NOT EXISTS (
                       SELECT 1 FROM release_group_artist rga2
                       WHERE rga2.release_group_id = rg.id
                         AND rga2.artist_id = ra.artist_id
                         AND rga2.position = 0
                   )
                 ORDER BY m.position, t.position",
            )
            .bind(id)
            .bind(&rg_id)
            .fetch_all(&mut *conn)
            .await?;

            let tracks = track_rows
                .into_iter()
                .map(|trow| GuestAppearanceTrack {
                    recording_id: trow.get("recording_id"),
                    recording_title: trow.get("recording_title"),
                    track_position: trow.get("track_position"),
                    disc_position: trow.get("disc_position"),
                })
                .collect();

            guest_appearances.push(GuestAppearanceReleaseGroup {
                id: rg_id,
                title: rg_row.get("title"),
                rg_type: rg_row.get("rg_type"),
                release_date: rg_row.get("release_date"),
                primary_artist_id: rg_row.get("primary_artist_id"),
                artist_credit_name: rg_row.get("artist_credit_name"),
                tracks,
            });
        }

        Ok(Some(ArtistDetail {
            id: artist_row.get("id"),
            name: artist_row.get("name"),
            sort_name: artist_row.get("sort_name"),
            mbid: artist_row.get("mbid"),
            rating: artist_row.get("rating"),
            last_played: artist_row.get("last_played"),
            recording_count: artist_row
                .get::<Option<i64>, _>("recording_count")
                .unwrap_or(0),
            release_group_count: release_groups.len() as i64,
            release_groups,
            guest_appearances,
        }))
    }

    async fn get_release_group_detail(&self, id: &str) -> Result<Option<ReleaseGroupDetail>> {
        let mut conn = self
            .db
            .acquire(format!("catalog.get_release_group_detail id={id}"))
            .await?;

        let rg_row = sqlx::query(
            "SELECT rg.id, rg.title, rg.rg_type,
                    COALESCE(rg.artist_credit_text, rga.credited_as, a.name) AS artist_credit_name,
                    a.id AS primary_artist_id,
                    MIN(rel.release_date) AS release_date,
                    (SELECT AVG(ur2.stars)
                     FROM release rel2
                     JOIN medium m2 ON m2.release_id = rel2.id
                     JOIN track t2 ON t2.medium_id = m2.id
                     JOIN user_rating ur2 ON ur2.recording_id = t2.recording_id
                     WHERE rel2.release_group_id = rg.id) AS rating,
                    (SELECT MAX(ph2.played_at)
                     FROM release rel2
                     JOIN medium m2 ON m2.release_id = rel2.id
                     JOIN track t2 ON t2.medium_id = m2.id
                     JOIN play_history ph2 ON ph2.recording_id = t2.recording_id
                     WHERE rel2.release_group_id = rg.id) AS last_played
             FROM release_group rg
             LEFT JOIN release_group_artist rga ON rga.release_group_id = rg.id AND rga.position = 0
             LEFT JOIN artist a ON a.id = rga.artist_id
             LEFT JOIN release rel ON rel.release_group_id = rg.id
             WHERE rg.id = ?
             GROUP BY rg.id",
        )
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;

        let Some(rg_row) = rg_row else {
            return Ok(None);
        };

        let release_rows = sqlx::query(
            "SELECT rel.id AS release_id, rel.title AS release_title,
                    rel.release_date, rel.country, rel.label, rel.catalog_number
             FROM release rel
             WHERE rel.release_group_id = ?
             ORDER BY rel.release_date, rel.title",
        )
        .bind(id)
        .fetch_all(&mut *conn)
        .await?;

        let mut releases = Vec::new();
        for rel_row in release_rows {
            let release_id: String = rel_row.get("release_id");

            let medium_rows = sqlx::query(
                "SELECT m.id AS medium_id, m.position, m.format
                 FROM medium m
                 WHERE m.release_id = ?
                 ORDER BY m.position",
            )
            .bind(&release_id)
            .fetch_all(&mut *conn)
            .await?;

            let mut mediums = Vec::new();
            for med_row in medium_rows {
                let medium_id: String = med_row.get("medium_id");

                let track_rows = sqlx::query(
                    "SELECT t.id, t.position, t.title, t.duration_ms,
                            r.id AS recording_id, r.title AS recording_title,
                            COALESCE(ra.credited_as, a.name) AS artist_credit_name,
                            a.id AS primary_artist_id,
                            CASE WHEN s.id IS NOT NULL THEN 1 ELSE 0 END AS has_source,
                            s.id AS primary_source_id
                     FROM track t
                     JOIN recording r ON r.id = t.recording_id
                     LEFT JOIN recording_artist ra ON ra.recording_id = r.id AND ra.position = 0
                     LEFT JOIN artist a ON a.id = ra.artist_id
                     LEFT JOIN source s ON s.id = (
                         SELECT s2.id FROM source s2
                         WHERE s2.recording_id = r.id
                         ORDER BY CASE s2.source_type WHEN 'local_file' THEN 0 ELSE 1 END, s2.file_path
                         LIMIT 1
                     )
                     WHERE t.medium_id = ?
                     ORDER BY t.position",
                )
                .bind(&medium_id)
                .fetch_all(&mut *conn)
                .await?;

                let tracks = track_rows
                    .into_iter()
                    .map(|trow| {
                        let has_source: i64 = trow.get("has_source");
                        TrackDetail {
                            id: trow.get("id"),
                            position: trow.get("position"),
                            title: trow.get("title"),
                            duration_ms: trow.get("duration_ms"),
                            recording_id: trow.get("recording_id"),
                            recording_title: trow.get("recording_title"),
                            artist_credit_name: trow.get("artist_credit_name"),
                            primary_artist_id: trow.get("primary_artist_id"),
                            has_source: has_source != 0,
                            primary_source_id: trow.get("primary_source_id"),
                        }
                    })
                    .collect();

                mediums.push(MediumDetail {
                    id: medium_id,
                    position: med_row.get("position"),
                    format: med_row.get("format"),
                    tracks,
                });
            }

            let completeness: ReleaseCompleteness = {
                let stats = sqlx::query(
                    "WITH release_source_stats AS (
                        SELECT
                            t.id AS track_id,
                            MAX(CASE WHEN s.id IS NOT NULL THEN 1 ELSE 0 END) AS has_source,
                            MAX(s.track_total) AS source_track_total
                        FROM medium m
                        JOIN track t ON t.medium_id = m.id
                        LEFT JOIN source s ON s.recording_id = t.recording_id
                        WHERE m.release_id = ?
                        GROUP BY t.id
                    )
                    SELECT
                        COUNT(DISTINCT source_track_total) AS distinct_track_totals,
                        MAX(source_track_total) AS consensus_track_total,
                        COUNT(*) AS total_tracks,
                        COALESCE(SUM(has_source), 0) AS tracks_with_sources
                    FROM release_source_stats",
                )
                .bind(&release_id)
                .fetch_one(&mut *conn)
                .await?;

                let mut distinct: i64 = stats.get("distinct_track_totals");
                let mut consensus: Option<i64> = stats.get("consensus_track_total");
                let total_tracks: i64 = stats.get("total_tracks");
                let with_sources: i64 = stats.get("tracks_with_sources");

                if distinct == 0 && total_tracks > 0 {
                    if backfill_track_totals_for_release(&mut *conn, &release_id).await? {
                        let stats2 = sqlx::query(
                            "WITH release_source_stats AS (
                                SELECT
                                    t.id AS track_id,
                                    MAX(CASE WHEN s.id IS NOT NULL THEN 1 ELSE 0 END) AS has_source,
                                    MAX(s.track_total) AS source_track_total
                                FROM medium m
                                JOIN track t ON t.medium_id = m.id
                                LEFT JOIN source s ON s.recording_id = t.recording_id
                                WHERE m.release_id = ?
                                GROUP BY t.id
                            )
                            SELECT
                                COUNT(DISTINCT source_track_total) AS distinct_track_totals,
                                MAX(source_track_total) AS consensus_track_total,
                                COALESCE(SUM(has_source), 0) AS tracks_with_sources
                            FROM release_source_stats",
                        )
                        .bind(&release_id)
                        .fetch_one(&mut *conn)
                        .await?;

                        distinct = stats2.get("distinct_track_totals");
                        consensus = stats2.get("consensus_track_total");
                    }
                }

                match (total_tracks, distinct, consensus) {
                    (0, _, _) => ReleaseCompleteness::Unknown {
                        reason: "No tracks on this release.".to_string(),
                        disagreement_groups: Vec::new(),
                    },
                    (_, 0, _) => ReleaseCompleteness::Unknown {
                        reason: "No source files provide track_total information.".to_string(),
                        disagreement_groups: Vec::new(),
                    },
                    (_, 1, Some(0)) => ReleaseCompleteness::Unknown {
                        reason: "Track total is zero.".to_string(),
                        disagreement_groups: Vec::new(),
                    },
                    (_, 1, _) if with_sources >= consensus.unwrap_or(0) => {
                        ReleaseCompleteness::Complete
                    }
                    (_, 1, _) if with_sources < total_tracks => {
                        let missing_vec: Vec<MissingTrackDetail> = sqlx::query(
                            "SELECT t.position AS track_position,
                                    m.position AS disc_position,
                                    COALESCE(t.title, r.title) AS title,
                                    r.id AS recording_id
                             FROM medium m
                             JOIN track t ON t.medium_id = m.id
                             JOIN recording r ON r.id = t.recording_id
                             WHERE m.release_id = ?
                               AND NOT EXISTS (SELECT 1 FROM source s WHERE s.recording_id = r.id)
                             ORDER BY m.position, t.position",
                        )
                        .bind(&release_id)
                        .fetch_all(&mut *conn)
                        .await?
                        .into_iter()
                        .map(|row| MissingTrackDetail {
                            disc_position: row.get("disc_position"),
                            track_position: row.get("track_position"),
                            title: row.get("title"),
                            recording_id: Some(row.get("recording_id")),
                        })
                        .collect();

                        let missing = NonEmpty::from_vec(missing_vec).unwrap_or_else(|| {
                            panic!(
                                "incomplete arm reached but no missing tracks found. \
                                 release_id={release_id}, total_tracks={total_tracks}, \
                                 with_sources={with_sources}, consensus={consensus:?}, distinct={distinct}"
                            )
                        });

                        ReleaseCompleteness::Incomplete {
                            missing_tracks: missing,
                        }
                    }
                    (_, 1, _) => {
                        let consensus_val = consensus.unwrap_or(0);

                        let missing_vec: Vec<MissingTrackDetail> = sqlx::query(
                            "WITH RECURSIVE positions(n) AS (
                                SELECT 1
                                UNION ALL
                                SELECT n + 1 FROM positions WHERE n < ?
                            )
                            SELECT m.position AS disc_position,
                                   p.n AS track_position
                            FROM medium m
                            CROSS JOIN positions p
                            WHERE m.release_id = ? AND p.n <= ?
                              AND NOT EXISTS (
                                  SELECT 1 FROM track t
                                  WHERE t.medium_id = m.id AND t.position = p.n
                              )
                            ORDER BY m.position, p.n",
                        )
                        .bind(consensus_val)
                        .bind(&release_id)
                        .bind(consensus_val)
                        .fetch_all(&mut *conn)
                        .await?
                        .into_iter()
                        .map(|row| MissingTrackDetail {
                            disc_position: row.get("disc_position"),
                            track_position: row.get("track_position"),
                            title: String::new(),
                            recording_id: None,
                        })
                        .collect();

                        let missing = NonEmpty::from_vec(missing_vec).unwrap_or_else(|| {
                            panic!(
                                "phantom-track arm reached but no missing positions found. \
                                 release_id={release_id}, total_tracks={total_tracks}, \
                                 with_sources={with_sources}, consensus={consensus:?}, distinct={distinct}"
                            )
                        });

                        ReleaseCompleteness::Incomplete {
                            missing_tracks: missing,
                        }
                    }
                    (_, _, _) => {
                        let source_data = sqlx::query(
                            "SELECT s.file_path, s.track_total,
                                    m.position AS disc_position,
                                    (SELECT MAX(m3.position) FROM medium m3
                                     WHERE m3.release_id = m.release_id) AS disc_total
                             FROM medium m
                             JOIN track t ON t.medium_id = m.id
                             JOIN source s ON s.recording_id = t.recording_id
                             WHERE m.release_id = ? AND s.track_total IS NOT NULL
                             ORDER BY s.track_total, m.position, s.file_path",
                        )
                        .bind(&release_id)
                        .fetch_all(&mut *conn)
                        .await?;

                        let release_disc_total: Option<i64> =
                            source_data.first().and_then(|r| r.get("disc_total"));

                        struct SourceEntry {
                            file_path: Option<String>,
                            track_total: i64,
                            disc_position: Option<i64>,
                        }

                        let entries: Vec<SourceEntry> = source_data
                            .iter()
                            .map(|row| SourceEntry {
                                file_path: row.get("file_path"),
                                track_total: row.get("track_total"),
                                disc_position: row.get("disc_position"),
                            })
                            .collect();

                        let mut by_tt: BTreeMap<i64, Vec<String>> = BTreeMap::new();
                        for e in &entries {
                            if let Some(ref fp) = e.file_path {
                                by_tt.entry(e.track_total).or_default().push(fp.clone());
                            }
                        }

                        let unanimous_completeness: Option<ReleaseCompleteness> = 'check: {
                            match release_disc_total {
                                Some(dt) if dt > 1 => dt,
                                _ => break 'check None,
                            };

                            let mut disc_map: BTreeMap<i64, BTreeMap<i64, usize>> = BTreeMap::new();
                            for e in &entries {
                                if let Some(dp) = e.disc_position {
                                    *disc_map
                                        .entry(dp)
                                        .or_default()
                                        .entry(e.track_total)
                                        .or_default() += 1;
                                }
                            }

                            if disc_map.is_empty() {
                                break 'check None;
                            }

                            let disc_actuals: Vec<(i64, i64)> = sqlx::query(
                                "SELECT m.position, COUNT(t.id) AS track_count
                                 FROM medium m
                                 JOIN track t ON t.medium_id = m.id
                                 WHERE m.release_id = ?
                                 GROUP BY m.position
                                 ORDER BY m.position",
                            )
                            .bind(&release_id)
                            .fetch_all(&mut *conn)
                            .await?
                            .iter()
                            .map(|row| (row.get("position"), row.get("track_count")))
                            .collect();

                            if !disc_map.values().all(|tts| tts.len() == 1) {
                                tracing::warn!(
                                    release_id,
                                    ?disc_map,
                                    "multi-disc unanimity: intra-disc disagreement",
                                );

                                let mode_claims: BTreeMap<i64, i64> = disc_map
                                    .iter()
                                    .map(|(&disc, tts)| {
                                        let mode = tts
                                            .iter()
                                            .max_by_key(|&(_, &c)| c)
                                            .map(|(&v, _)| v)
                                            .unwrap_or(0);
                                        (disc, mode)
                                    })
                                    .collect();

                                let modes_match = disc_actuals.iter().all(|(pos, actual_count)| {
                                    mode_claims
                                        .get(pos)
                                        .map(|&claimed| claimed == *actual_count)
                                        .unwrap_or(false)
                                });

                                if modes_match {
                                    disc_map = disc_map
                                        .into_iter()
                                        .map(|(disc, tts)| {
                                            let mode = mode_claims.get(&disc).copied().unwrap_or(0);
                                            let mut new_tts = BTreeMap::new();
                                            new_tts
                                                .insert(mode, tts.get(&mode).copied().unwrap_or(0));
                                            (disc, new_tts)
                                        })
                                        .collect();
                                } else {
                                    break 'check None;
                                }
                            }

                            let all_discs_match = disc_actuals.iter().all(|(pos, actual_count)| {
                                disc_map
                                    .get(pos)
                                    .and_then(|tts| tts.keys().next())
                                    .map(|&claimed| claimed == *actual_count)
                                    .unwrap_or(false)
                            });

                            if all_discs_match {
                                if with_sources >= total_tracks {
                                    break 'check Some(ReleaseCompleteness::Complete);
                                }

                                let missing_vec: Vec<MissingTrackDetail> = sqlx::query(
                                    "SELECT t.position AS track_position,
                                                m.position AS disc_position,
                                                COALESCE(t.title, r.title) AS title,
                                                r.id AS recording_id
                                         FROM medium m
                                         JOIN track t ON t.medium_id = m.id
                                         JOIN recording r ON r.id = t.recording_id
                                         WHERE m.release_id = ?
                                           AND NOT EXISTS (
                                               SELECT 1 FROM source s
                                               WHERE s.recording_id = r.id
                                           )
                                         ORDER BY m.position, t.position",
                                )
                                .bind(&release_id)
                                .fetch_all(&mut *conn)
                                .await?
                                .into_iter()
                                .map(|row| MissingTrackDetail {
                                    disc_position: row.get("disc_position"),
                                    track_position: row.get("track_position"),
                                    title: row.get("title"),
                                    recording_id: Some(row.get("recording_id")),
                                })
                                .collect();

                                let missing = NonEmpty::from_vec(missing_vec).unwrap_or_else(|| {
                                    panic!(
                                        "unanimous multi-disc incomplete reached but no missing \
                                         tracks. release_id={release_id}, total_tracks={total_tracks}, \
                                         with_sources={with_sources}"
                                    )
                                });

                                break 'check Some(ReleaseCompleteness::Incomplete {
                                    missing_tracks: missing,
                                });
                            }

                            let has_deficit = disc_actuals.iter().any(|(pos, actual_count)| {
                                disc_map
                                    .get(pos)
                                    .and_then(|tts| tts.keys().next())
                                    .map(|&claimed| claimed < *actual_count)
                                    .unwrap_or(false)
                            });

                            if has_deficit {
                                tracing::warn!(
                                    release_id,
                                    ?disc_map,
                                    ?disc_actuals,
                                    "multi-disc unanimity: source claims below DB count",
                                );
                                break 'check None;
                            }

                            let mut all_missing: Vec<MissingTrackDetail> = Vec::new();

                            let type_a: Vec<MissingTrackDetail> = sqlx::query(
                                "SELECT t.position AS track_position,
                                        m.position AS disc_position,
                                        COALESCE(t.title, r.title) AS title,
                                        r.id AS recording_id
                                 FROM medium m
                                 JOIN track t ON t.medium_id = m.id
                                 JOIN recording r ON r.id = t.recording_id
                                 WHERE m.release_id = ?
                                   AND NOT EXISTS (SELECT 1 FROM source s WHERE s.recording_id = r.id)
                                 ORDER BY m.position, t.position",
                            )
                            .bind(&release_id)
                            .fetch_all(&mut *conn)
                            .await?
                            .into_iter()
                            .map(|row| MissingTrackDetail {
                                disc_position: row.get("disc_position"),
                                track_position: row.get("track_position"),
                                title: row.get("title"),
                                recording_id: Some(row.get("recording_id")),
                            })
                            .collect();

                            all_missing.extend(type_a);

                            for (pos, actual_count) in &disc_actuals {
                                if let Some(claimed_count) =
                                    disc_map.get(pos).and_then(|tts| tts.keys().next())
                                {
                                    if *claimed_count > *actual_count {
                                        let phantom_positions: Vec<i64> = sqlx::query(
                                            "WITH RECURSIVE positions(n) AS (
                                                SELECT 1
                                                UNION ALL
                                                SELECT n + 1 FROM positions WHERE n < ?
                                            )
                                            SELECT p.n AS track_position
                                            FROM positions p
                                            WHERE NOT EXISTS (
                                                SELECT 1 FROM track t
                                                JOIN medium m2 ON t.medium_id = m2.id
                                                WHERE m2.release_id = ?
                                                  AND m2.position = ?
                                                  AND t.position = p.n
                                            )
                                            ORDER BY p.n",
                                        )
                                        .bind(claimed_count)
                                        .bind(&release_id)
                                        .bind(pos)
                                        .fetch_all(&mut *conn)
                                        .await?
                                        .iter()
                                        .map(|row| row.get("track_position"))
                                        .collect();

                                        for tp in phantom_positions {
                                            all_missing.push(MissingTrackDetail {
                                                disc_position: *pos,
                                                track_position: tp,
                                                title: String::new(),
                                                recording_id: None,
                                            });
                                        }
                                    }
                                }
                            }

                            let missing_count = all_missing.len();
                            let missing = NonEmpty::from_vec(all_missing).unwrap_or_else(|| {
                                panic!(
                                    "phantom path reached but no missing tracks. \
                                     release_id={release_id}, disc_actuals={disc_actuals:?}, \
                                     disc_map={disc_map:?}"
                                )
                            });

                            tracing::warn!(
                                release_id,
                                ?disc_map,
                                ?disc_actuals,
                                missing_count,
                                "multi-disc unanimity: phantom tracks detected",
                            );

                            break 'check Some(ReleaseCompleteness::Incomplete {
                                missing_tracks: missing,
                            });
                        };

                        if let Some(c) = unanimous_completeness {
                            c
                        } else {
                            let mut groups: Vec<(String, Vec<String>)> = Vec::new();
                            let mut parts: Vec<String> = Vec::new();

                            if let Some(dt) = release_disc_total {
                                if dt > 1 {
                                    let mut disc_map: BTreeMap<i64, BTreeMap<i64, usize>> =
                                        BTreeMap::new();
                                    let mut all_paths: Vec<String> = Vec::new();
                                    for e in &entries {
                                        if let Some(ref fp) = e.file_path {
                                            all_paths.push(fp.clone());
                                        }
                                        if let Some(dp) = e.disc_position {
                                            *disc_map
                                                .entry(dp)
                                                .or_default()
                                                .entry(e.track_total)
                                                .or_default() += 1;
                                        }
                                    }
                                    all_paths.sort();
                                    all_paths.dedup();

                                    if !disc_map.is_empty() {
                                        tracing::warn!(
                                            release_id,
                                            ?disc_map,
                                            "multi-disc disagreement display: producing single layout from merged claims",
                                        );

                                        let per_disc: Vec<String> = disc_map
                                            .iter()
                                            .map(|(_, tts)| {
                                                let best = tts
                                                    .iter()
                                                    .max_by_key(|&(_, &c)| c)
                                                    .map(|(v, _)| v.to_string())
                                                    .unwrap_or_default();
                                                best
                                            })
                                            .collect();

                                        let cnt = all_paths.len();
                                        let desc = format!(
                                            "{} disc{} with {} track{}",
                                            dt,
                                            if dt == 1 { "" } else { "s" },
                                            per_disc.join("+"),
                                            if cnt == 1 { "" } else { "s" }
                                        );
                                        let claim_word = if cnt == 1 { "claims" } else { "claim" };
                                        parts.push(format!(
                                            "{} from {} source{}",
                                            desc,
                                            cnt,
                                            if cnt == 1 { "" } else { "s" }
                                        ));
                                        groups.push((
                                            format!("{} source {} {}", cnt, claim_word, desc),
                                            all_paths,
                                        ));
                                    } else {
                                        for (tt, paths) in &by_tt {
                                            let cnt = paths.len();
                                            let claim_word =
                                                if cnt == 1 { "claims" } else { "claim" };
                                            let desc = format!(
                                                "{} disc with {} track{}",
                                                dt,
                                                tt,
                                                if *tt == 1 { "" } else { "s" }
                                            );
                                            parts.push(format!(
                                                "{} from {} source{}",
                                                tt,
                                                cnt,
                                                if cnt == 1 { "" } else { "s" }
                                            ));
                                            groups.push((
                                                format!("{} source {} {}", cnt, claim_word, desc),
                                                paths.clone(),
                                            ));
                                        }
                                    }
                                } else {
                                    for (tt, paths) in &by_tt {
                                        let cnt = paths.len();
                                        let claim_word = if cnt == 1 { "claims" } else { "claim" };
                                        let desc = format!(
                                            "1 disc with {} track{}",
                                            tt,
                                            if *tt == 1 { "" } else { "s" }
                                        );
                                        parts.push(format!(
                                            "{} from {} source{}",
                                            tt,
                                            cnt,
                                            if cnt == 1 { "" } else { "s" }
                                        ));
                                        groups.push((
                                            format!("{} source {} {}", cnt, claim_word, desc),
                                            paths.clone(),
                                        ));
                                    }
                                }
                            } else {
                                for (tt, paths) in &by_tt {
                                    let cnt = paths.len();
                                    let claim_word = if cnt == 1 { "claims" } else { "claim" };
                                    let desc = format!(
                                        "(no total disc info) {} track{}",
                                        tt,
                                        if *tt == 1 { "" } else { "s" }
                                    );
                                    parts.push(format!(
                                        "{} from {} source{} (no disc info)",
                                        tt,
                                        cnt,
                                        if cnt == 1 { "" } else { "s" }
                                    ));
                                    groups.push((
                                        format!("{} source {} {}", cnt, claim_word, desc),
                                        paths.clone(),
                                    ));
                                }
                            }

                            ReleaseCompleteness::Unknown {
                                reason: format!(
                                    "Sources disagree on total track count: {}",
                                    parts.join(", ")
                                ),
                                disagreement_groups: groups
                                    .into_iter()
                                    .map(|(description, source_paths)| SourceDisagreementGroup {
                                        description,
                                        source_paths,
                                    })
                                    .collect(),
                            }
                        }
                    }
                }
            };

            releases.push(ReleaseDetail {
                id: release_id,
                title: rel_row.get("release_title"),
                release_date: rel_row.get("release_date"),
                country: rel_row.get("country"),
                label: rel_row.get("label"),
                catalog_number: rel_row.get("catalog_number"),
                mediums,
                completeness,
            });
        }

        Ok(Some(ReleaseGroupDetail {
            id: rg_row.get("id"),
            title: rg_row.get("title"),
            rg_type: rg_row.get("rg_type"),
            artist_credit_name: rg_row.get("artist_credit_name"),
            primary_artist_id: rg_row.get("primary_artist_id"),
            rating: rg_row.get("rating"),
            last_played: rg_row.get("last_played"),
            release_date: rg_row.get("release_date"),
            releases,
        }))
    }

    async fn get_recording_detail(&self, id: &str) -> Result<Option<RecordingDetail>> {
        let mut conn = self
            .db
            .acquire(format!("catalog.get_recording_detail id={id}"))
            .await?;

        let row = sqlx::query(
            "SELECT r.id, r.title, r.duration_ms, r.genre, r.bpm, r.comment,
                    r.artist_credit_text, r.mbid, r.acoustid,
                    COALESCE(ra.credited_as, a.name) AS artist_credit_name,
                    a.id AS primary_artist_id,
                    ur.stars AS rating,
                    COALESCE(ph.play_count, 0) AS play_count,
                    ph.last_played
             FROM recording r
             LEFT JOIN recording_artist ra ON ra.recording_id = r.id AND ra.position = 0
             LEFT JOIN artist a ON a.id = ra.artist_id
             LEFT JOIN user_rating ur ON ur.recording_id = r.id
             LEFT JOIN (
                 SELECT recording_id, COUNT(*) AS play_count, MAX(played_at) AS last_played
                 FROM play_history GROUP BY recording_id
             ) ph ON ph.recording_id = r.id
             WHERE r.id = ?",
        )
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let release_rows = sqlx::query(
            "SELECT rg.id AS release_group_id, rg.title AS release_group_title,
                    t.position AS track_position, m.position AS disc_position,
                    (SELECT MAX(m3.position) FROM medium m3 WHERE m3.release_id = rel.id) AS disc_total
             FROM track t
             JOIN medium m ON m.id = t.medium_id
             JOIN release rel ON rel.id = m.release_id
             JOIN release_group rg ON rg.id = rel.release_group_id
             WHERE t.recording_id = ?
             ORDER BY rg.title, m.position, t.position",
        )
        .bind(id)
        .fetch_all(&mut *conn)
        .await?;

        let releases = release_rows
            .into_iter()
            .map(|rrow| ReleaseInfo {
                release_group_id: rrow.get("release_group_id"),
                release_group_title: rrow.get("release_group_title"),
                track_position: rrow.get("track_position"),
                disc_position: rrow.get("disc_position"),
                disc_total: rrow.get("disc_total"),
            })
            .collect();

        let artist_rows = sqlx::query(
            "SELECT a.id AS artist_id, a.name,
                    ra.position, ra.role, ra.credited_as
             FROM recording_artist ra
             JOIN artist a ON a.id = ra.artist_id
             WHERE ra.recording_id = ?
             ORDER BY ra.position",
        )
        .bind(id)
        .fetch_all(&mut *conn)
        .await?;

        let artists = artist_rows
            .into_iter()
            .map(|rrow| RecordingArtistInfo {
                artist_id: rrow.get("artist_id"),
                name: rrow.get("name"),
                position: rrow.get("position"),
                role: rrow.get("role"),
                credited_as: rrow.get("credited_as"),
            })
            .collect();

        let source_rows = sqlx::query(
            "SELECT s.id, s.source_type, s.file_path, s.format, s.duration_ms,
                    s.replay_gain_track_db, s.replay_gain_track_peak
             FROM source s
             WHERE s.recording_id = ?
             ORDER BY s.source_type, s.file_path",
        )
        .bind(id)
        .fetch_all(&mut *conn)
        .await?;

        let mut sources = Vec::new();
        for srow in source_rows {
            let file_path: Option<String> = srow.get("file_path");
            let source_type: String = srow.get("source_type");

            let tags = if let Some(ref path_str) = file_path {
                if source_type == "local_file" {
                    crate::library::scanner::list_all_tags(std::path::Path::new(path_str))
                        .unwrap_or_default()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };

            sources.push(SourceDetail {
                id: srow.get("id"),
                source_type,
                file_path,
                format: srow.get("format"),
                duration_ms: srow.get("duration_ms"),
                replay_gain_track_db: srow.get("replay_gain_track_db"),
                replay_gain_track_peak: srow.get("replay_gain_track_peak"),
                tags,
            });
        }

        Ok(Some(RecordingDetail {
            id: row.get("id"),
            title: row.get("title"),
            duration_ms: row.get("duration_ms"),
            genre: row.get("genre"),
            bpm: row.get("bpm"),
            comment: row.get("comment"),
            artist_credit_name: row.get("artist_credit_name"),
            primary_artist_id: row.get("primary_artist_id"),
            artist_credit_text: row.get("artist_credit_text"),
            mbid: row.get("mbid"),
            acoustid: row.get("acoustid"),
            rating: row.get("rating"),
            play_count: row.get::<Option<i64>, _>("play_count").unwrap_or(0),
            last_played: row.get("last_played"),
            artists,
            releases,
            sources,
        }))
    }
}
