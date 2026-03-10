use crate::models::TrackMetadata;
use anyhow::{Context, Result};
use lofty::prelude::*;
use lofty::probe::Probe;
use lofty::tag::ItemKey;
use std::path::Path;

/// Supported audio extensions.
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "ogg", "flac", "wav", "m4a", "aac", "opus", "wma", "ape",
];

pub fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Read audio metadata from a file using lofty.
pub fn read_metadata(path: &Path) -> Result<TrackMetadata> {
    let tagged_file = Probe::open(path)
        .context("Failed to open file for metadata reading")?
        .guess_file_type()
        .context("Failed to detect file type")?
        .read()
        .context("Failed to read tags")?;

    let properties = tagged_file.properties();
    let duration_ms = properties.duration().as_millis() as u64;

    let format = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("unknown")
        .to_lowercase();

    let mut meta = TrackMetadata {
        duration_ms,
        format,
        ..Default::default()
    };

    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag());

    if let Some(tag) = tag {
        meta.title = tag.title().map(|s| s.into_owned());
        meta.artist = tag.artist().map(|s| s.into_owned());
        meta.album = tag.album().map(|s| s.into_owned());
        meta.year = tag.year();
        meta.track_number = tag.track();
        meta.track_total = tag.track_total();
        meta.disc_number = tag.disk();

        // Album artist (not in the Accessor trait; use the tag item key)
        meta.album_artist = tag
            .get_string(&ItemKey::AlbumArtist)
            .map(ToString::to_string);

        meta.genre = tag.genre().map(|s| s.into_owned());
        meta.comment = tag.comment().map(|s| s.into_owned());
        meta.bpm = tag
            .get_string(&ItemKey::Bpm)
            .and_then(|s| s.trim().parse().ok());
        meta.replay_gain_track_db = tag
            .get_string(&ItemKey::ReplayGainTrackGain)
            .and_then(|s| s.trim().trim_end_matches("dB").trim().parse().ok());
        meta.replay_gain_track_peak = tag
            .get_string(&ItemKey::ReplayGainTrackPeak)
            .and_then(|s| s.trim().parse().ok());
        meta.replay_gain_album_db = tag
            .get_string(&ItemKey::ReplayGainAlbumGain)
            .and_then(|s| s.trim().trim_end_matches("dB").trim().parse().ok());
        meta.replay_gain_album_peak = tag
            .get_string(&ItemKey::ReplayGainAlbumPeak)
            .and_then(|s| s.trim().parse().ok());
    }

    // Fall back: use filename as title if no title tag
    if meta.title.is_none() {
        meta.title = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(ToString::to_string);
    }

    Ok(meta)
}
