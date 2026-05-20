use crate::db::DbPool;
use crate::models::{
    ArtistDetail, ArtistReleaseGroup, ArtistRow, GuestAppearanceReleaseGroup, GuestAppearanceTrack,
    LibrarySummary, MediumDetail, MissingTrackDetail, RecordingArtistInfo, RecordingDetail,
    RecordingRow, ReleaseCompleteness, ReleaseDetail, ReleaseGroupDetail, ReleaseGroupRow,
    ReleaseInfo, SmartPlaylistResult, SourceDetail, SourceTagInfo, TrackDetail,
};
use crate::query::{CmpOp, Expr, LastPlayedDir, LimitUnit, Predicate, StrOp, TimeUnit};
use anyhow::Result;
use nonempty::NonEmpty;
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

// ── Internal data structures ──────────────────────────────────────────────────

struct RawSource {
    id: String,
    source_type: String,
    file_path: Option<String>,
    format: Option<String>,
    duration_ms: Option<i64>,
    replay_gain_track_db: Option<f64>,
    replay_gain_track_peak: Option<f64>,
    tags: Vec<(String, String)>,
    rating: Option<i64>,
}

impl RawSource {
    fn rating(&self) -> Option<f64> {
        self.rating.map(|r| r as f64)
    }
}

struct MemArtist {
    id: String,
    name: String,
    sort_name: String,
}

struct MemReleaseGroup {
    id: String,
    title: String,
    artist_id: Option<String>,
    artist_credit_name: Option<String>,
    release_date: Option<String>,
}

struct MemRecording {
    id: String,
    title: String,
    duration_ms: Option<i64>,
    genre: Option<String>,
    bpm: Option<f64>,
    comment: Option<String>,
    sources: Vec<RawSource>,
    // Artist info (index 0 = primary, 1.. = guests)
    primary_artist_id: Option<String>,
    artist_credit_name: Option<String>,
    artist_ids: Vec<String>,
    all_artist_names: Vec<String>,
    // Release info
    release_group_id: Option<String>,
    track_position: Option<i64>,
    disc_position: Option<i64>,
    release_year: Option<String>,
    // User data (rating lives on sources; play data lives at recording level)
    play_count: i64,
    last_played: Option<String>,
    tags: Vec<String>,
}

impl MemRecording {
    /// Average of all source ratings for this recording.
    fn avg_rating(&self) -> Option<f64> {
        avg_f64(self.sources.iter().filter_map(|s| s.rating()))
    }
}

// ── Hierarchical rating helpers ───────────────────────────────────────────────

fn avg_f64(values: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, count) = values.fold((0.0f64, 0usize), |(s, c), v| (s + v, c + 1));
    if count == 0 {
        None
    } else {
        Some(sum / count as f64)
    }
}

/// Release rating: average of all track ratings, all tracks equally weighted.
/// In our model a track has exactly one recording, so this is the average of
/// all recording avg_ratings in the release.
fn release_avg_rating<'a>(recs: impl Iterator<Item = &'a MemRecording>) -> Option<f64> {
    avg_f64(recs.filter_map(|r| r.avg_rating()))
}

/// Release group rating: average of its release ratings.
/// We model one release per RG, so this delegates to release_avg_rating.
fn rg_avg_rating(rg_indices: &[usize], all: &[MemRecording]) -> Option<f64> {
    release_avg_rating(rg_indices.iter().map(|&i| &all[i]))
}

// ── Public catalog struct ─────────────────────────────────────────────────────

pub struct MemoryCatalog {
    recordings: Vec<MemRecording>,
    recordings_by_id: HashMap<String, usize>,
    artists: Vec<MemArtist>,
    artists_by_id: HashMap<String, usize>,
    release_groups: Vec<MemReleaseGroup>,
    rgs_by_id: HashMap<String, usize>,
    recordings_by_rg: HashMap<String, Vec<usize>>,
    recordings_by_artist: HashMap<String, Vec<usize>>,
    artist_rg_ids: HashMap<String, HashSet<String>>,
    rg_disc_totals: HashMap<String, i64>,
    playlists: HashMap<String, HashSet<String>>,
    source_count: i64,
}

impl MemoryCatalog {
    /// Returns the artist's name and sorted list of local-file source paths for all
    /// recordings associated with the given artist ID.
    pub fn source_paths_for_artist(&self, artist_id: &str) -> (Option<String>, Vec<String>) {
        let name = self
            .artists_by_id
            .get(artist_id)
            .map(|&i| self.artists[i].name.clone());
        let mut paths: Vec<String> = Vec::new();
        if let Some(rec_indices) = self.recordings_by_artist.get(artist_id) {
            for &idx in rec_indices {
                for src in &self.recordings[idx].sources {
                    if src.source_type == "local_file" {
                        if let Some(p) = &src.file_path {
                            if !paths.contains(p) {
                                paths.push(p.clone());
                            }
                        }
                    }
                }
            }
        }
        paths.sort();
        (name, paths)
    }

    pub fn source_paths_for_release_group(&self, rg_id: &str) -> Vec<String> {
        let mut paths: Vec<String> = Vec::new();
        if let Some(rec_indices) = self.recordings_by_rg.get(rg_id) {
            for &idx in rec_indices {
                for src in &self.recordings[idx].sources {
                    if src.source_type == "local_file" {
                        if let Some(p) = &src.file_path {
                            if !paths.contains(p) {
                                paths.push(p.clone());
                            }
                        }
                    }
                }
            }
        }
        paths.sort();
        paths
    }

    pub async fn load_from_db(db: &DbPool) -> Result<Self> {
        let mut conn = db.acquire("memory_catalog.load").await?;

        // ── Load source rows ──────────────────────────────────────────────────
        let source_rows = sqlx::query(
            "SELECT id, recording_id, source_type, file_path, format, duration_ms,
                    replay_gain_track_db, replay_gain_track_peak, raw_tags_json
             FROM source",
        )
        .fetch_all(&mut *conn)
        .await?;
        let source_count = source_rows.len() as i64;

        // ── Load user data ────────────────────────────────────────────────────
        let source_ratings: HashMap<String, i64> =
            sqlx::query("SELECT source_id, stars FROM source_rating")
                .fetch_all(&mut *conn)
                .await?
                .into_iter()
                .map(|r| (r.get::<String, _>("source_id"), r.get::<i64, _>("stars")))
                .collect();

        let play_data: HashMap<String, (i64, String)> = sqlx::query(
            "SELECT recording_id, COUNT(*) AS play_count, MAX(played_at) AS last_played
             FROM play_history GROUP BY recording_id",
        )
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .filter_map(|r| {
            let last: Option<String> = r.get("last_played");
            last.map(|lp| {
                (
                    r.get::<String, _>("recording_id"),
                    (r.get::<i64, _>("play_count"), lp),
                )
            })
        })
        .collect();

        let mut recording_tags: HashMap<String, Vec<String>> = HashMap::new();
        for r in
            sqlx::query("SELECT recording_id, tag FROM recording_tag ORDER BY recording_id, tag")
                .fetch_all(&mut *conn)
                .await?
        {
            recording_tags
                .entry(r.get::<String, _>("recording_id"))
                .or_default()
                .push(r.get::<String, _>("tag"));
        }

        let mut playlists: HashMap<String, HashSet<String>> = HashMap::new();
        for r in sqlx::query(
            "SELECT p.name, pt.recording_id
             FROM playlist p JOIN playlist_track pt ON pt.playlist_id = p.id",
        )
        .fetch_all(&mut *conn)
        .await?
        {
            playlists
                .entry(r.get::<String, _>("name"))
                .or_default()
                .insert(r.get::<String, _>("recording_id"));
        }

        drop(conn);

        // ── Group sources by recording_id ─────────────────────────────────────
        let mut rec_sources: HashMap<String, Vec<RawSource>> = HashMap::new();
        for row in source_rows {
            let raw_json: Option<String> = row.get("raw_tags_json");
            let tags: Vec<(String, String)> = raw_json
                .as_deref()
                .and_then(|j| serde_json::from_str(j).ok())
                .unwrap_or_default();
            let source_id: String = row.get("id");
            let rating = source_ratings.get(&source_id).copied();
            let src = RawSource {
                id: source_id,
                source_type: row.get("source_type"),
                file_path: row.get("file_path"),
                format: row.get("format"),
                duration_ms: row.get("duration_ms"),
                replay_gain_track_db: row.get("replay_gain_track_db"),
                replay_gain_track_peak: row.get("replay_gain_track_peak"),
                tags,
                rating,
            };
            rec_sources
                .entry(row.get::<String, _>("recording_id"))
                .or_default()
                .push(src);
        }

        // ── Build MemRecording, MemArtist, MemReleaseGroup ───────────────────
        let mut artist_map: HashMap<String, MemArtist> = HashMap::new();
        // rg_id -> (MemReleaseGroup, min release_date across recordings)
        let mut rg_map: HashMap<String, MemReleaseGroup> = HashMap::new();

        let mut recordings: Vec<MemRecording> = Vec::with_capacity(rec_sources.len());

        for (recording_id, mut sources) in rec_sources {
            // Stable canonical source: smallest file_path among local_file sources.
            sources.sort_by(|a, b| a.file_path.cmp(&b.file_path));
            let canonical = sources
                .iter()
                .find(|s| s.source_type == "local_file")
                .or_else(|| sources.first());
            let canonical_tags: &[(String, String)] =
                canonical.map(|s| s.tags.as_slice()).unwrap_or(&[]);

            let title = tag_first(canonical_tags, "TIT2")
                .unwrap_or("(Unknown)")
                .to_string();
            let duration_ms = sources.iter().filter_map(|s| s.duration_ms).max();
            let genre = tag_first(canonical_tags, "TCON").map(str::to_string);
            let bpm = tag_first(canonical_tags, "TBPM").and_then(|s| s.parse::<f64>().ok());
            let comment = tag_first(canonical_tags, "COMM").map(str::to_string);

            let primary_artist_name = tag_first(canonical_tags, "TPE1").map(str::to_string);
            let album_artist_name = tag_first(canonical_tags, "TPE2").map(str::to_string);

            // All artist names: TPE1 first, then semicolon-split TXXX:ARTISTS
            let mut all_artist_names: Vec<String> = Vec::new();
            if let Some(n) = &primary_artist_name {
                all_artist_names.push(n.clone());
            }
            if let Some(extra) = tag_first(canonical_tags, "TXXX:ARTISTS") {
                for part in extra.split(';') {
                    let t = part.trim();
                    if !t.is_empty() && !all_artist_names.iter().any(|x| x == t) {
                        all_artist_names.push(t.to_string());
                    }
                }
            }

            // Derive artist IDs (MBID when present, hash fallback)
            let primary_mbid = tag_first(canonical_tags, "TXXX:MusicBrainz Artist Id");
            let mut artist_ids: Vec<String> = Vec::new();
            for (i, name) in all_artist_names.iter().enumerate() {
                let id = if i == 0 {
                    primary_mbid
                        .map(str::to_string)
                        .unwrap_or_else(|| name_hash("artist", name))
                } else {
                    name_hash("artist", name)
                };
                artist_ids.push(id);
            }

            // Register artists
            let primary_sort = tag_first(canonical_tags, "TSOP").map(str::to_string);
            for (i, name) in all_artist_names.iter().enumerate() {
                let id = &artist_ids[i];
                artist_map.entry(id.clone()).or_insert_with(|| {
                    let sort_name = if i == 0 {
                        primary_sort
                            .clone()
                            .unwrap_or_else(|| derive_sort_name(name))
                    } else {
                        derive_sort_name(name)
                    };
                    MemArtist {
                        id: id.clone(),
                        name: name.clone(),
                        sort_name,
                    }
                });
            }

            let primary_artist_id = artist_ids.first().cloned();
            let artist_credit_name = primary_artist_name.clone();

            // Derive release group
            let album_title = tag_first(canonical_tags, "TALB").map(str::to_string);
            let rg_artist_name = album_artist_name
                .as_ref()
                .or(primary_artist_name.as_ref())
                .cloned();

            let release_year = tag_first(canonical_tags, "TDRC")
                .or_else(|| tag_first(canonical_tags, "TYER"))
                .map(|s| s.chars().take(4).collect::<String>());

            let release_group_id =
                album_title
                    .as_ref()
                    .zip(rg_artist_name.as_ref())
                    .map(|(album, artist)| {
                        tag_first(canonical_tags, "TXXX:MusicBrainz Release Group Id")
                            .map(str::to_string)
                            .unwrap_or_else(|| name_hash("rg", &format!("{}|{}", album, artist)))
                    });

            if let Some(rg_id) = &release_group_id {
                let rg_entry = rg_map.entry(rg_id.clone()).or_insert_with(|| {
                    let rg_artist_id = rg_artist_name.as_ref().map(|n| {
                        if primary_artist_name.as_ref() == Some(n) {
                            primary_artist_id
                                .clone()
                                .unwrap_or_else(|| name_hash("artist", n))
                        } else {
                            primary_mbid
                                .map(str::to_string)
                                .unwrap_or_else(|| name_hash("artist", n))
                        }
                    });
                    MemReleaseGroup {
                        id: rg_id.clone(),
                        title: album_title.clone().unwrap_or_default(),
                        artist_id: rg_artist_id,
                        artist_credit_name: rg_artist_name.clone(),
                        release_date: None, // filled in below
                    }
                });
                // Update release_date with minimum (earliest)
                if let Some(yr) = &release_year {
                    if !yr.is_empty() {
                        rg_entry.release_date = Some(match &rg_entry.release_date {
                            Some(existing) => existing.min(yr).clone(),
                            None => yr.clone(),
                        });
                    }
                }
            }

            // Parse TRCK
            let (track_position, _track_total) = tag_first(canonical_tags, "TRCK")
                .map(parse_trck)
                .unwrap_or((None, None));
            let disc_position = tag_first(canonical_tags, "TPOS")
                .and_then(|s| s.split('/').next())
                .and_then(|s| s.trim().parse::<i64>().ok());

            // User data
            let (play_count, last_played) = play_data
                .get(&recording_id)
                .map(|(c, d)| (*c, Some(d.clone())))
                .unwrap_or((0, None));
            let tags = recording_tags
                .get(&recording_id)
                .cloned()
                .unwrap_or_default();
            recordings.push(MemRecording {
                id: recording_id,
                title,
                duration_ms,
                genre,
                bpm,
                comment,
                sources,
                primary_artist_id,
                artist_credit_name,
                artist_ids,
                all_artist_names,
                release_group_id,
                track_position,
                disc_position,
                release_year,
                play_count,
                last_played,
                tags,
            });
        }

        // ── Build release groups list ─────────────────────────────────────────
        let mut release_groups: Vec<MemReleaseGroup> = rg_map.into_values().collect();

        // ── Build artists list ────────────────────────────────────────────────
        let mut artists: Vec<MemArtist> = artist_map.into_values().collect();
        artists.sort_by_key(|a| a.sort_name.to_lowercase());

        // ── Sort recordings by (artist sort_name, title) ──────────────────────
        // Build a temporary sort_name lookup
        let artist_sort_lookup: HashMap<String, String> = artists
            .iter()
            .map(|a| (a.id.clone(), a.sort_name.clone()))
            .collect();

        recordings.sort_by(|a, b| {
            let a_sort = a
                .primary_artist_id
                .as_ref()
                .and_then(|id| artist_sort_lookup.get(id))
                .map(|s| s.to_lowercase())
                .unwrap_or_default();
            let b_sort = b
                .primary_artist_id
                .as_ref()
                .and_then(|id| artist_sort_lookup.get(id))
                .map(|s| s.to_lowercase())
                .unwrap_or_default();
            a_sort
                .cmp(&b_sort)
                .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
        });

        // Sort release groups by (artist sort_name, title)
        release_groups.sort_by(|a, b| {
            let a_sort = a
                .artist_id
                .as_ref()
                .and_then(|id| artist_sort_lookup.get(id))
                .map(|s| s.to_lowercase())
                .unwrap_or_default();
            let b_sort = b
                .artist_id
                .as_ref()
                .and_then(|id| artist_sort_lookup.get(id))
                .map(|s| s.to_lowercase())
                .unwrap_or_default();
            a_sort
                .cmp(&b_sort)
                .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
        });

        // ── Build index maps ──────────────────────────────────────────────────
        let recordings_by_id: HashMap<String, usize> = recordings
            .iter()
            .enumerate()
            .map(|(i, r)| (r.id.clone(), i))
            .collect();

        let artists_by_id: HashMap<String, usize> = artists
            .iter()
            .enumerate()
            .map(|(i, a)| (a.id.clone(), i))
            .collect();

        let rgs_by_id: HashMap<String, usize> = release_groups
            .iter()
            .enumerate()
            .map(|(i, rg)| (rg.id.clone(), i))
            .collect();

        let mut recordings_by_rg: HashMap<String, Vec<usize>> = HashMap::new();
        let mut recordings_by_artist: HashMap<String, Vec<usize>> = HashMap::new();
        let mut artist_rg_ids: HashMap<String, HashSet<String>> = HashMap::new();
        let mut rg_disc_totals: HashMap<String, i64> = HashMap::new();

        for (i, rec) in recordings.iter().enumerate() {
            // recordings_by_rg
            if let Some(rg_id) = &rec.release_group_id {
                recordings_by_rg.entry(rg_id.clone()).or_default().push(i);
                // rg_disc_totals
                let disc = rec.disc_position.unwrap_or(1);
                let cur = rg_disc_totals.entry(rg_id.clone()).or_insert(0);
                if disc > *cur {
                    *cur = disc;
                }
                // artist_rg_ids
                for aid in &rec.artist_ids {
                    artist_rg_ids
                        .entry(aid.clone())
                        .or_default()
                        .insert(rg_id.clone());
                }
            }
            // recordings_by_artist
            for aid in &rec.artist_ids {
                recordings_by_artist.entry(aid.clone()).or_default().push(i);
            }
        }

        Ok(MemoryCatalog {
            recordings,
            recordings_by_id,
            artists,
            artists_by_id,
            release_groups,
            rgs_by_id,
            recordings_by_rg,
            recordings_by_artist,
            artist_rg_ids,
            rg_disc_totals,
            playlists,
            source_count,
        })
    }
}

// ── Tag helpers ───────────────────────────────────────────────────────────────

fn tag_first<'a>(tags: &'a [(String, String)], key: &str) -> Option<&'a str> {
    tags.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// Parse "N" or "N/M" track string; returns (position, total).
fn parse_trck(s: &str) -> (Option<i64>, Option<i64>) {
    let mut parts = s.splitn(2, '/');
    let pos = parts.next().and_then(|p| p.trim().parse::<i64>().ok());
    let total = parts.next().and_then(|p| p.trim().parse::<i64>().ok());
    (pos, total)
}

fn parse_trck_total(s: &str) -> Option<i64> {
    s.split_once('/')
        .and_then(|(_, p)| p.trim().parse::<i64>().ok())
}

/// SHA-256-based deterministic ID, prefixed with `kind`.
fn name_hash(kind: &str, key: &str) -> String {
    let mut h = Sha256::new();
    h.update(key.to_lowercase().trim().as_bytes());
    let digest = h.finalize();
    format!("mem-{}-{}", kind, hex::encode(&digest[..8]))
}

fn derive_sort_name(name: &str) -> String {
    let lower = name.to_lowercase();
    if lower.starts_with("the ") {
        format!("{}, The", &name[4..])
    } else if lower.starts_with("a ") {
        format!("{}, A", &name[2..])
    } else if lower.starts_with("an ") {
        format!("{}, An", &name[3..])
    } else {
        name.to_string()
    }
}

/// Convert a SQLite datetime string ("YYYY-MM-DD HH:MM:SS") to Unix seconds.
fn sqlite_datetime_to_unix(s: &str) -> Option<i64> {
    if s.len() < 19 {
        return None;
    }
    let year: i64 = s[0..4].parse().ok()?;
    let month: i64 = s[5..7].parse().ok()?;
    let day: i64 = s[8..10].parse().ok()?;
    let hour: i64 = s[11..13].parse().ok()?;
    let min: i64 = s[14..16].parse().ok()?;
    let sec: i64 = s[17..19].parse().ok()?;
    // Julian Day Number (Gregorian)
    let a = (14 - month) / 12;
    let y = year + 4800 - a;
    let m = month + 12 * a - 3;
    let jdn = day + (153 * m + 2) / 5 + 365 * y + y / 4 - y / 100 + y / 400 - 32045;
    let days = jdn - 2_440_588; // Unix epoch = JDN 2440588
    Some(days * 86400 + hour * 3600 + min * 60 + sec)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn id3_field_name(frame_id: &str) -> String {
    match frame_id {
        "TIT2" => "Title".into(),
        "TPE1" => "Artist".into(),
        "TPE2" => "Album Artist".into(),
        "TALB" => "Album".into(),
        "TRCK" => "Track Number".into(),
        "TPOS" => "Disc Number".into(),
        "TYER" | "TDRC" => "Year".into(),
        "TCON" => "Genre".into(),
        "TBPM" => "BPM".into(),
        "COMM" => "Comment".into(),
        "TSOP" => "Performer Sort Order".into(),
        other if other.starts_with("TXXX:") => {
            format!("User Text ({})", &other[5..])
        }
        other => other.into(),
    }
}

fn raw_tags_to_source_tags(tags: &[(String, String)]) -> Vec<SourceTagInfo> {
    tags.iter()
        .map(|(k, v)| SourceTagInfo {
            frame_id: k.clone(),
            field_name: id3_field_name(k),
            value: v.clone(),
        })
        .collect()
}

// ── Smart-playlist in-memory evaluator ───────────────────────────────────────

fn apply_cmp(op: CmpOp, a: i64, b: i64) -> bool {
    match op {
        CmpOp::Gte => a >= b,
        CmpOp::Lte => a <= b,
        CmpOp::Gt => a > b,
        CmpOp::Lt => a < b,
        CmpOp::Eq => a == b,
        CmpOp::Ne => a != b,
    }
}

fn match_str(op: StrOp, haystack: &str, needle: &str) -> bool {
    let h = haystack.to_lowercase();
    let n = needle.to_lowercase();
    match op {
        StrOp::Is => h == n,
        StrOp::Contains => h.contains(&n),
    }
}

fn eval_expr(
    expr: &Expr,
    rec: &MemRecording,
    playlists: &HashMap<String, HashSet<String>>,
    rg_avg_ratings: &HashMap<String, f64>,
    now: i64,
) -> bool {
    match expr {
        Expr::And(a, b) => {
            eval_expr(a, rec, playlists, rg_avg_ratings, now)
                && eval_expr(b, rec, playlists, rg_avg_ratings, now)
        }
        Expr::Or(a, b) => {
            eval_expr(a, rec, playlists, rg_avg_ratings, now)
                || eval_expr(b, rec, playlists, rg_avg_ratings, now)
        }
        Expr::Not(e) => !eval_expr(e, rec, playlists, rg_avg_ratings, now),
        Expr::Pred(pred) => eval_pred(pred, rec, playlists, rg_avg_ratings, now),
    }
}

fn eval_pred(
    pred: &Predicate,
    rec: &MemRecording,
    playlists: &HashMap<String, HashSet<String>>,
    rg_avg_ratings: &HashMap<String, f64>,
    now: i64,
) -> bool {
    match pred {
        Predicate::RatingNull => rec.avg_rating().is_none(),
        Predicate::Rating(op, n) => rec
            .avg_rating()
            .map(|r| apply_cmp(*op, r.round() as i64, *n))
            .unwrap_or(false),
        Predicate::AlbumRating(op, n) => rec
            .release_group_id
            .as_ref()
            .and_then(|id| rg_avg_ratings.get(id))
            .map(|&r| apply_cmp(*op, r as i64, *n))
            .unwrap_or(false),
        Predicate::PlayCount(op, n) => apply_cmp(*op, rec.play_count, *n),
        Predicate::LastPlayed(dir, n, unit) => {
            let days: i64 = match unit {
                TimeUnit::Days => *n,
                TimeUnit::Weeks => n * 7,
                TimeUnit::Months => n * 30,
                TimeUnit::Years => n * 365,
            };
            let threshold = now - days * 86400;
            match (dir, &rec.last_played) {
                (LastPlayedDir::InLast, Some(lp)) => sqlite_datetime_to_unix(lp)
                    .map(|t| t >= threshold)
                    .unwrap_or(false),
                (LastPlayedDir::InLast, None) => false,
                (LastPlayedDir::NotInLast, None) => true,
                (LastPlayedDir::NotInLast, Some(lp)) => sqlite_datetime_to_unix(lp)
                    .map(|t| t < threshold)
                    .unwrap_or(true),
            }
        }
        Predicate::InPlaylist(name) => playlists
            .get(name.as_str())
            .map(|s| s.contains(rec.id.as_str()))
            .unwrap_or(false),
        Predicate::NotInPlaylist(name) => !playlists
            .get(name.as_str())
            .map(|s| s.contains(rec.id.as_str()))
            .unwrap_or(false),
        Predicate::HasTag(tag) => rec.tags.iter().any(|t| t == tag),
        Predicate::Title(op, s) => match_str(*op, &rec.title, s),
        Predicate::Genre(op, s) => rec
            .genre
            .as_deref()
            .map(|g| match_str(*op, g, s))
            .unwrap_or(false),
        Predicate::Artist(op, s) => rec.all_artist_names.iter().any(|n| match_str(*op, n, s)),
        Predicate::Year(op, n) => rec
            .release_year
            .as_deref()
            .and_then(|y| y.parse::<i64>().ok())
            .map(|y| apply_cmp(*op, y, *n))
            .unwrap_or(false),
    }
}

// ── Completeness analysis ─────────────────────────────────────────────────────

enum DiscCompleteness {
    Complete,
    Incomplete(Vec<MissingTrackDetail>),
    Unknown(String),
}

fn disc_completeness(
    disc_pos: i64,
    present: &HashSet<i64>,
    claimed_totals: &BTreeMap<i64, usize>,
) -> DiscCompleteness {
    if claimed_totals.is_empty() {
        return DiscCompleteness::Unknown(
            "No source files provide track_total information.".into(),
        );
    }
    if claimed_totals.len() > 1 {
        let parts: Vec<String> = claimed_totals
            .iter()
            .map(|(total, count)| {
                format!(
                    "{} track{} from {} source{}",
                    total,
                    if *total == 1 { "" } else { "s" },
                    count,
                    if *count == 1 { "" } else { "s" }
                )
            })
            .collect();
        return DiscCompleteness::Unknown(format!(
            "Sources disagree on total track count: {}",
            parts.join(", ")
        ));
    }
    let (&total, _) = claimed_totals.iter().next().unwrap();
    if total == 0 {
        return DiscCompleteness::Unknown("Track total is zero.".into());
    }
    let missing: Vec<MissingTrackDetail> = (1..=total)
        .filter(|p| !present.contains(p))
        .map(|p| MissingTrackDetail {
            disc_position: disc_pos,
            track_position: p,
            title: String::new(),
            recording_id: None,
        })
        .collect();
    if missing.is_empty() {
        DiscCompleteness::Complete
    } else {
        DiscCompleteness::Incomplete(missing)
    }
}

// ── Shared conversion ─────────────────────────────────────────────────────────

fn mem_recording_to_row(rec: &MemRecording, catalog: &MemoryCatalog) -> RecordingRow {
    let primary_source = rec.sources.iter().find(|s| s.source_type == "local_file");
    let releases = rec
        .release_group_id
        .as_ref()
        .map(|rg_id| {
            let rg_title = catalog
                .rgs_by_id
                .get(rg_id)
                .map(|&idx| catalog.release_groups[idx].title.clone())
                .unwrap_or_default();
            let disc_total = catalog.rg_disc_totals.get(rg_id).copied();
            vec![ReleaseInfo {
                release_group_id: rg_id.clone(),
                release_group_title: rg_title,
                track_position: rec.track_position,
                disc_position: rec.disc_position,
                disc_total,
            }]
        })
        .unwrap_or_default();

    RecordingRow {
        id: rec.id.clone(),
        title: rec.title.clone(),
        duration_ms: rec.duration_ms,
        primary_artist_id: rec.primary_artist_id.clone(),
        artist_credit_name: rec.artist_credit_name.clone(),
        genre: rec.genre.clone(),
        rating: rec.avg_rating(),
        play_count: rec.play_count,
        last_played: rec.last_played.clone(),
        primary_source_id: primary_source.map(|s| s.id.clone()),
        primary_source_path: primary_source.and_then(|s| s.file_path.clone()),
        tags: rec.tags.clone(),
        artist_ids: rec.artist_ids.clone(),
        source_paths: rec
            .sources
            .iter()
            .filter_map(|s| s.file_path.clone())
            .collect(),
        releases,
    }
}

// ── CatalogReader implementation ──────────────────────────────────────────────

impl super::CatalogReader for MemoryCatalog {
    async fn list_recordings(&self) -> Result<Vec<RecordingRow>> {
        Ok(self
            .recordings
            .iter()
            .map(|r| mem_recording_to_row(r, self))
            .collect())
    }

    async fn list_artists(&self) -> Result<Vec<ArtistRow>> {
        Ok(self
            .artists
            .iter()
            .map(|a| {
                let rec_indices = self
                    .recordings_by_artist
                    .get(&a.id)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]);
                let rating = avg_f64(
                    rec_indices
                        .iter()
                        .filter_map(|&i| self.recordings[i].avg_rating()),
                );
                let last_played = rec_indices
                    .iter()
                    .filter_map(|&i| self.recordings[i].last_played.clone())
                    .max();
                let rg_count = self
                    .release_groups
                    .iter()
                    .filter(|rg| rg.artist_id.as_deref() == Some(a.id.as_str()))
                    .count() as i64;
                ArtistRow {
                    id: a.id.clone(),
                    name: a.name.clone(),
                    sort_name: a.sort_name.clone(),
                    release_group_count: rg_count,
                    recording_count: rec_indices.len() as i64,
                    rating,
                    last_played,
                }
            })
            .collect())
    }

    async fn list_release_groups(
        &self,
        artist_id: Option<&str>,
        search: Option<&str>,
    ) -> Result<Vec<ReleaseGroupRow>> {
        let rows: Vec<ReleaseGroupRow> = self
            .release_groups
            .iter()
            .filter(|rg| {
                // Artist filter
                if let Some(aid) = artist_id {
                    let in_rg = self
                        .artist_rg_ids
                        .get(aid)
                        .map(|s| s.contains(rg.id.as_str()))
                        .unwrap_or(false);
                    if !in_rg {
                        return false;
                    }
                }
                // Search filter
                if let Some(q) = search {
                    let q_low = q.to_lowercase();
                    let title_match = rg.title.to_lowercase().contains(&q_low);
                    let artist_match = rg
                        .artist_credit_name
                        .as_deref()
                        .map(|n| n.to_lowercase().contains(&q_low))
                        .unwrap_or(false);
                    if !title_match && !artist_match {
                        return false;
                    }
                }
                true
            })
            .map(|rg| {
                let rec_indices = self
                    .recordings_by_rg
                    .get(&rg.id)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]);
                let rating = rg_avg_rating(rec_indices, &self.recordings);
                let last_played = rec_indices
                    .iter()
                    .filter_map(|&i| self.recordings[i].last_played.clone())
                    .max();
                ReleaseGroupRow {
                    id: rg.id.clone(),
                    title: rg.title.clone(),
                    artist_credit_name: rg.artist_credit_name.clone(),
                    primary_artist_id: rg.artist_id.clone(),
                    release_count: 1,
                    recording_count: rec_indices.len() as i64,
                    release_date: rg.release_date.clone(),
                    rating,
                    last_played,
                }
            })
            .collect();
        Ok(rows)
    }

    async fn get_library_summary(&self) -> Result<LibrarySummary> {
        Ok(LibrarySummary {
            recording_count: self.recordings.len() as i64,
            artist_count: self.artists.len() as i64,
            release_group_count: self.release_groups.len() as i64,
            source_count: self.source_count,
        })
    }

    async fn list_all_tags(&self) -> Result<Vec<String>> {
        let mut all: HashSet<String> = HashSet::new();
        for rec in &self.recordings {
            all.extend(rec.tags.iter().cloned());
        }
        let mut sorted: Vec<String> = all.into_iter().collect();
        sorted.sort();
        Ok(sorted)
    }

    async fn evaluate_smart_playlist(&self, query: &str) -> Result<SmartPlaylistResult> {
        let parsed = crate::query::parse(query)?;
        let now = now_unix();

        let rg_avg_ratings: HashMap<String, f64> = self
            .recordings_by_rg
            .iter()
            .filter_map(|(rg_id, indices)| {
                rg_avg_rating(indices, &self.recordings).map(|r| (rg_id.clone(), r))
            })
            .collect();

        let mut recordings: Vec<RecordingRow> = self
            .recordings
            .iter()
            .filter(|rec| eval_expr(&parsed.expr, rec, &self.playlists, &rg_avg_ratings, now))
            .map(|rec| mem_recording_to_row(rec, self))
            .collect();

        // Shuffle (deterministic random not available without rng crate; use index-based shuffle)
        // Use a simple Fisher-Yates with a seed derived from current time
        let seed = now as u64;
        pseudo_shuffle(&mut recordings, seed);

        // Apply limit
        if let Some(lim) = &parsed.limit {
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
                    let mut acc: i64 = 0;
                    recordings.retain(|r| {
                        if acc >= target_ms {
                            return false;
                        }
                        acc += r.duration_ms.unwrap_or(0);
                        true
                    });
                }
            }
        }

        let total_duration_ms = recordings.iter().map(|r| r.duration_ms.unwrap_or(0)).sum();
        Ok(SmartPlaylistResult {
            recordings,
            total_duration_ms,
            sql: format!("(evaluated in-memory from query: {})", query),
        })
    }

    async fn get_artist_detail(&self, id: &str) -> Result<Option<ArtistDetail>> {
        let Some(&artist_idx) = self.artists_by_id.get(id) else {
            return Ok(None);
        };
        let artist = &self.artists[artist_idx];

        let rec_indices = self
            .recordings_by_artist
            .get(id)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);

        let rating = avg_f64(
            rec_indices
                .iter()
                .filter_map(|&i| self.recordings[i].avg_rating()),
        );
        let last_played = rec_indices
            .iter()
            .filter_map(|&i| self.recordings[i].last_played.clone())
            .max();

        // Primary release groups (this artist is primary RG artist)
        let primary_rg_ids: HashSet<&str> = self
            .release_groups
            .iter()
            .filter(|rg| rg.artist_id.as_deref() == Some(id))
            .map(|rg| rg.id.as_str())
            .collect();

        let mut release_groups: Vec<ArtistReleaseGroup> = self
            .release_groups
            .iter()
            .filter(|rg| rg.artist_id.as_deref() == Some(id))
            .map(|rg| {
                let rg_recs = self
                    .recordings_by_rg
                    .get(&rg.id)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]);
                let rg_rating = rg_avg_rating(rg_recs, &self.recordings);
                ArtistReleaseGroup {
                    id: rg.id.clone(),
                    title: rg.title.clone(),
                    rg_type: None,
                    release_date: rg.release_date.clone(),
                    recording_count: rg_recs.len() as i64,
                    rating: rg_rating,
                    primary_artist_id: rg.artist_id.clone(),
                    artist_credit_name: rg.artist_credit_name.clone(),
                }
            })
            .collect();
        release_groups.sort_by(|a, b| a.title.cmp(&b.title));

        // Guest appearances: recordings where artist is not position-0, in RGs they don't own
        let mut guest_rg_tracks: HashMap<String, Vec<GuestAppearanceTrack>> = HashMap::new();
        for &rec_idx in rec_indices {
            let rec = &self.recordings[rec_idx];
            // Guest = artist is in artist_ids but not at index 0
            let is_guest = rec.artist_ids.first().map(|a| a != id).unwrap_or(false)
                && rec.artist_ids.iter().any(|a| a == id);
            if !is_guest {
                continue;
            }
            if let Some(rg_id) = &rec.release_group_id {
                if primary_rg_ids.contains(rg_id.as_str()) {
                    continue;
                }
                guest_rg_tracks
                    .entry(rg_id.clone())
                    .or_default()
                    .push(GuestAppearanceTrack {
                        recording_id: rec.id.clone(),
                        recording_title: rec.title.clone(),
                        track_position: rec.track_position,
                        disc_position: rec.disc_position,
                    });
            }
        }

        let mut guest_appearances: Vec<GuestAppearanceReleaseGroup> = guest_rg_tracks
            .into_iter()
            .filter_map(|(rg_id, mut tracks)| {
                let rg = self
                    .rgs_by_id
                    .get(&rg_id)
                    .map(|&idx| &self.release_groups[idx])?;
                tracks.sort_by_key(|t| (t.disc_position, t.track_position));
                Some(GuestAppearanceReleaseGroup {
                    id: rg_id,
                    title: rg.title.clone(),
                    rg_type: None,
                    release_date: rg.release_date.clone(),
                    primary_artist_id: rg.artist_id.clone(),
                    artist_credit_name: rg.artist_credit_name.clone(),
                    tracks,
                })
            })
            .collect();
        guest_appearances.sort_by(|a, b| a.title.cmp(&b.title));

        Ok(Some(ArtistDetail {
            id: artist.id.clone(),
            name: artist.name.clone(),
            sort_name: artist.sort_name.clone(),
            mbid: None,
            rating,
            last_played,
            recording_count: rec_indices.len() as i64,
            release_group_count: release_groups.len() as i64,
            release_groups,
            guest_appearances,
        }))
    }

    async fn get_release_group_detail(&self, id: &str) -> Result<Option<ReleaseGroupDetail>> {
        let Some(&rg_idx) = self.rgs_by_id.get(id) else {
            return Ok(None);
        };
        let rg = &self.release_groups[rg_idx];

        let rec_indices = self
            .recordings_by_rg
            .get(id)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);

        let rating = rg_avg_rating(rec_indices, &self.recordings);
        let last_played = rec_indices
            .iter()
            .filter_map(|&i| self.recordings[i].last_played.clone())
            .max();

        if rec_indices.is_empty() {
            return Ok(Some(ReleaseGroupDetail {
                id: rg.id.clone(),
                title: rg.title.clone(),
                rg_type: None,
                artist_credit_name: rg.artist_credit_name.clone(),
                primary_artist_id: rg.artist_id.clone(),
                rating,
                last_played,
                release_date: rg.release_date.clone(),
                releases: Vec::new(),
            }));
        }

        // Group recordings by disc (TPOS, default 1)
        // disc_pos -> Vec<(track_pos, rec_idx)>
        let mut disc_map: BTreeMap<i64, Vec<(i64, usize)>> = BTreeMap::new();
        for &rec_idx in rec_indices {
            let rec = &self.recordings[rec_idx];
            let disc = rec.disc_position.unwrap_or(1);
            let track = rec.track_position.unwrap_or(0);
            disc_map.entry(disc).or_default().push((track, rec_idx));
        }

        // Build mediums and compute completeness
        let mut mediums: Vec<MediumDetail> = Vec::new();
        let mut all_missing: Vec<MissingTrackDetail> = Vec::new();
        let mut unknown_reason: Option<String> = None;

        for (&disc_pos, track_list) in &disc_map {
            let mut present: HashSet<i64> = HashSet::new();
            let mut claimed_totals: BTreeMap<i64, usize> = BTreeMap::new();
            let mut tracks: Vec<TrackDetail> = Vec::new();

            let mut sorted_list = track_list.clone();
            sorted_list.sort_by_key(|&(tp, _)| tp);

            for (track_pos, rec_idx) in sorted_list {
                let rec = &self.recordings[rec_idx];
                let primary_src = rec.sources.iter().find(|s| s.source_type == "local_file");

                // Collect track total from TRCK tag
                if let Some(total) = rec
                    .sources
                    .iter()
                    .find(|s| s.source_type == "local_file")
                    .and_then(|s| tag_first(&s.tags, "TRCK"))
                    .and_then(parse_trck_total)
                {
                    *claimed_totals.entry(total).or_default() += 1;
                }

                if track_pos > 0 {
                    present.insert(track_pos);
                }

                tracks.push(TrackDetail {
                    id: format!(
                        "mem-track-{}-{}-{}",
                        &id[..id.len().min(16)],
                        disc_pos,
                        track_pos
                    ),
                    position: track_pos,
                    title: None,
                    duration_ms: rec.duration_ms,
                    recording_id: rec.id.clone(),
                    recording_title: rec.title.clone(),
                    artist_credit_name: rec.artist_credit_name.clone(),
                    primary_artist_id: rec.primary_artist_id.clone(),
                    has_source: !rec.sources.is_empty(),
                    primary_source_id: primary_src.map(|s| s.id.clone()),
                });
            }

            match disc_completeness(disc_pos, &present, &claimed_totals) {
                DiscCompleteness::Complete => {}
                DiscCompleteness::Incomplete(missing) => all_missing.extend(missing),
                DiscCompleteness::Unknown(reason) => {
                    if unknown_reason.is_none() {
                        unknown_reason = Some(reason);
                    }
                }
            }

            mediums.push(MediumDetail {
                id: format!("mem-medium-{}-{}", &id[..id.len().min(16)], disc_pos),
                position: disc_pos,
                format: None,
                tracks,
            });
        }

        let completeness = if let Some(reason) = unknown_reason {
            ReleaseCompleteness::Unknown {
                reason,
                disagreement_groups: Vec::new(),
            }
        } else if let Some(missing) = NonEmpty::from_vec(all_missing) {
            ReleaseCompleteness::Incomplete {
                missing_tracks: missing,
            }
        } else {
            ReleaseCompleteness::Complete
        };

        let release = ReleaseDetail {
            id: format!("mem-rel-{}", &id[..id.len().min(16)]),
            title: rg.title.clone(),
            release_date: rg.release_date.clone(),
            country: None,
            label: None,
            catalog_number: None,
            mediums,
            completeness,
        };

        Ok(Some(ReleaseGroupDetail {
            id: rg.id.clone(),
            title: rg.title.clone(),
            rg_type: None,
            artist_credit_name: rg.artist_credit_name.clone(),
            primary_artist_id: rg.artist_id.clone(),
            rating,
            last_played,
            release_date: rg.release_date.clone(),
            releases: vec![release],
        }))
    }

    async fn get_recording_detail(&self, id: &str) -> Result<Option<RecordingDetail>> {
        let Some(&rec_idx) = self.recordings_by_id.get(id) else {
            return Ok(None);
        };
        let rec = &self.recordings[rec_idx];

        let artists: Vec<RecordingArtistInfo> = rec
            .artist_ids
            .iter()
            .enumerate()
            .map(|(pos, artist_id)| RecordingArtistInfo {
                artist_id: artist_id.clone(),
                name: rec.all_artist_names.get(pos).cloned().unwrap_or_default(),
                position: pos as i64,
                role: if pos == 0 {
                    "main".into()
                } else {
                    "featuring".into()
                },
                credited_as: None,
            })
            .collect();

        let releases = rec
            .release_group_id
            .as_ref()
            .map(|rg_id| {
                let rg_title = self
                    .rgs_by_id
                    .get(rg_id)
                    .map(|&idx| self.release_groups[idx].title.clone())
                    .unwrap_or_default();
                let disc_total = self.rg_disc_totals.get(rg_id).copied();
                vec![ReleaseInfo {
                    release_group_id: rg_id.clone(),
                    release_group_title: rg_title,
                    track_position: rec.track_position,
                    disc_position: rec.disc_position,
                    disc_total,
                }]
            })
            .unwrap_or_default();

        let sources: Vec<SourceDetail> = rec
            .sources
            .iter()
            .map(|src| SourceDetail {
                id: src.id.clone(),
                source_type: src.source_type.clone(),
                file_path: src.file_path.clone(),
                format: src.format.clone(),
                duration_ms: src.duration_ms,
                replay_gain_track_db: src.replay_gain_track_db,
                replay_gain_track_peak: src.replay_gain_track_peak,
                tags: raw_tags_to_source_tags(&src.tags),
            })
            .collect();

        Ok(Some(RecordingDetail {
            id: rec.id.clone(),
            title: rec.title.clone(),
            duration_ms: rec.duration_ms,
            genre: rec.genre.clone(),
            bpm: rec.bpm,
            comment: rec.comment.clone(),
            artist_credit_name: rec.artist_credit_name.clone(),
            primary_artist_id: rec.primary_artist_id.clone(),
            artist_credit_text: None,
            mbid: None,
            acoustid: None,
            rating: rec.avg_rating(),
            play_count: rec.play_count,
            last_played: rec.last_played.clone(),
            artists,
            releases,
            sources,
        }))
    }
}

// ── Pseudo-shuffle (no rng crate dependency) ──────────────────────────────────

/// Simple linear-congruential Fisher-Yates shuffle.
fn pseudo_shuffle<T>(v: &mut [T], seed: u64) {
    let n = v.len();
    if n <= 1 {
        return;
    }
    let mut state = seed.wrapping_add(6364136223846793005);
    for i in (1..n).rev() {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let j = (state >> 33) as usize % (i + 1);
        v.swap(i, j);
    }
}
