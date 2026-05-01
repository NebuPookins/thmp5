import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import "./EntityDetailView.css";

// ── Types matching Rust models ─────────────────────────────────────────────

export type EntityType = "artist" | "release_group" | "recording";

type SourceTagInfo = {
  frame_id: string;
  field_name: string;
  value: string;
};

type SourceDetail = {
  id: string;
  source_type: string;
  file_path: string | null;
  format: string | null;
  duration_ms: number | null;
  replay_gain_track_db: number | null;
  replay_gain_track_peak: number | null;
  tags: SourceTagInfo[];
};

type ReleaseInfo = {
  release_group_id: string;
  release_group_title: string;
  track_position: number | null;
  disc_position: number | null;
  disc_total: number | null;
};

// ── Artist detail types ─────────────────────────────────────────────────────

type ArtistReleaseGroup = {
  id: string;
  title: string;
  rg_type: string | null;
  release_date: string | null;
  recording_count: number;
  rating: number | null;
  primary_artist_id: string | null;
  artist_credit_name: string | null;
};

type ArtistDetail = {
  id: string;
  name: string;
  sort_name: string;
  mbid: string | null;
  rating: number | null;
  last_played: string | null;
  recording_count: number;
  release_group_count: number;
  release_groups: ArtistReleaseGroup[];
};

// ── Release group detail types ──────────────────────────────────────────────

type TrackDetail = {
  id: string;
  position: number;
  title: string | null;
  duration_ms: number | null;
  recording_id: string;
  recording_title: string;
  artist_credit_name: string | null;
  primary_artist_id: string | null;
  has_source: boolean;
  primary_source_id: string | null;
};

type MediumDetail = {
  id: string;
  position: number;
  format: string | null;
  tracks: TrackDetail[];
};

type ReleaseCompleteness =
  | { type: "complete" }
  | { type: "incomplete"; missing_tracks: MissingTrackDetail[] }
  | { type: "unknown"; reason: string };

type ReleaseDetail = {
  id: string;
  title: string;
  release_date: string | null;
  country: string | null;
  label: string | null;
  catalog_number: string | null;
  mediums: MediumDetail[];
  completeness: ReleaseCompleteness;
};

type MissingTrackDetail = {
  disc_position: number;
  track_position: number;
  title: string;
  recording_id: string | null;
};

type ReleaseGroupDetail = {
  id: string;
  title: string;
  rg_type: string | null;
  artist_credit_name: string | null;
  primary_artist_id: string | null;
  rating: number | null;
  last_played: string | null;
  release_date: string | null;
  releases: ReleaseDetail[];
};

// ── Recording detail types ──────────────────────────────────────────────────

type RecordingDetail = {
  id: string;
  title: string;
  duration_ms: number | null;
  genre: string | null;
  bpm: number | null;
  comment: string | null;
  artist_credit_name: string | null;
  primary_artist_id: string | null;
  artist_credit_text: string | null;
  mbid: string | null;
  acoustid: string | null;
  rating: number | null;
  play_count: number;
  last_played: string | null;
  releases: ReleaseInfo[];
  sources: SourceDetail[];
};

// ── Navigation type ─────────────────────────────────────────────────────────

export type DetailNav = {
  type: EntityType;
  id: string;
};

// ── Helpers ─────────────────────────────────────────────────────────────────

function formatDuration(ms: number | null): string {
  if (!ms || ms <= 0) return "Unknown";
  const totalSeconds = Math.floor(ms / 1000);
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${minutes}:${seconds.toString().padStart(2, "0")}`;
}

function formatYear(value: string | null): string {
  return value?.slice(0, 10) ?? "Unknown date";
}

function formatAlbumRating(value: number | null): string {
  if (value === null) return "Unrated";
  return `Avg ${value.toFixed(1)}`;
}

function stars(value: number | null): string {
  if (value === null) return "—";
  return "★".repeat(value) + "☆".repeat(5 - value);
}

function completenessIcon(c: ReleaseCompleteness) {
  switch (c.type) {
    case "complete":
      return <span className="completeness-icon completeness-complete" title="All tracks have sources">✓</span>;
    case "incomplete":
      const missing = c.missing_tracks.map(mt => {
        const base = `Disc ${mt.disc_position}, Track ${mt.track_position}`;
        return mt.title ? `${base} — ${mt.title}` : base;
      }).join(", ");
      return <span className="completeness-icon completeness-incomplete" title={`Missing: ${missing}`}>✗</span>;
    case "unknown":
      return <span className="completeness-icon completeness-unknown" title={c.reason}>?</span>;
  }
}

// ── Component ───────────────────────────────────────────────────────────────

type Props = {
  nav: DetailNav;
  canGoBack: boolean;
  canGoForward: boolean;
  onNavigate: (nav: DetailNav) => void;
  onBack: () => void;
  onForward: () => void;
  onClose: () => void;
  onSourceContextMenu?: (e: React.MouseEvent<HTMLDivElement>, filePath: string) => void;
  onEnqueueTrack?: (track: TrackDetail) => void;
};

export default function EntityDetailView({ nav, canGoBack, canGoForward, onNavigate, onBack, onForward, onClose, onSourceContextMenu, onEnqueueTrack }: Props) {
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [artist, setArtist] = useState<ArtistDetail | null>(null);
  const [releaseGroup, setReleaseGroup] = useState<ReleaseGroupDetail | null>(null);
  const [recording, setRecording] = useState<RecordingDetail | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      switch (nav.type) {
        case "artist":
          setArtist(await invoke<ArtistDetail>("get_artist_detail", { id: nav.id }));
          setReleaseGroup(null);
          setRecording(null);
          break;
        case "release_group":
          setReleaseGroup(await invoke<ReleaseGroupDetail>("get_release_group_detail", { id: nav.id }));
          setArtist(null);
          setRecording(null);
          break;
        case "recording":
          setRecording(await invoke<RecordingDetail>("get_recording_detail", { id: nav.id }));
          setArtist(null);
          setReleaseGroup(null);
          break;
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [nav]);

  useEffect(() => { void load(); }, [load]);

  useEffect(() => {
    function handleKeyDown(e: KeyboardEvent) {
      if (e.altKey && e.key === "ArrowLeft") { e.preventDefault(); onBack(); }
      if (e.altKey && e.key === "ArrowRight") { e.preventDefault(); onForward(); }
    }
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [onBack, onForward]);

  function artistLink(id: string, name: string) {
    return (
      <button className="entity-link" onClick={() => onNavigate({ type: "artist", id })} type="button">
        {name}
      </button>
    );
  }

  function releaseGroupLink(id: string, title: string) {
    return (
      <button className="entity-link" onClick={() => onNavigate({ type: "release_group", id })} type="button">
        {title}
      </button>
    );
  }

  function recordingLink(id: string, title: string) {
    return (
      <button className="entity-link" onClick={() => onNavigate({ type: "recording", id })} type="button">
        {title}
      </button>
    );
  }

  // ── Loading / Error ─────────────────────────────────────────────────────

  if (loading) {
    return (
      <section className="entity-detail-panel">
        <div className="entity-detail-header">
          <span className="panel-label">{nav.type.replace("_", " ")} detail</span>
          <span className="entity-detail-nav">
            <button className="nav-btn" disabled={!canGoBack} onClick={onBack} type="button" title="Back (Alt+Left)">◀</button>
            <button className="nav-btn" disabled={!canGoForward} onClick={onForward} type="button" title="Forward (Alt+Right)">▶</button>
          </span>
          <button className="modal-close-btn" onClick={onClose} type="button">✕</button>
        </div>
        <div className="entity-detail-body">
          <p className="empty-browser-state">Loading…</p>
        </div>
      </section>
    );
  }

  if (error) {
    return (
      <section className="entity-detail-panel">
        <div className="entity-detail-header">
          <span className="panel-label">{nav.type.replace("_", " ")} detail</span>
          <span className="entity-detail-nav">
            <button className="nav-btn" disabled={!canGoBack} onClick={onBack} type="button" title="Back (Alt+Left)">◀</button>
            <button className="nav-btn" disabled={!canGoForward} onClick={onForward} type="button" title="Forward (Alt+Right)">▶</button>
          </span>
          <button className="modal-close-btn" onClick={onClose} type="button">✕</button>
        </div>
        <div className="entity-detail-body">
          <div className="error-banner">{error}</div>
        </div>
      </section>
    );
  }

  // ── Artist view ─────────────────────────────────────────────────────────

  if (nav.type === "artist" && artist) {
    return (
      <section className="entity-detail-panel">
        <div className="entity-detail-header">
          <span className="panel-label">Artist detail</span>
          <span className="entity-detail-nav">
            <button className="nav-btn" disabled={!canGoBack} onClick={onBack} type="button" title="Back (Alt+Left)">◀</button>
            <button className="nav-btn" disabled={!canGoForward} onClick={onForward} type="button" title="Forward (Alt+Right)">▶</button>
          </span>
          <button className="modal-close-btn" onClick={onClose} type="button">✕</button>
        </div>
        <div className="entity-detail-body">
          <div className="entity-detail-hero">
            <h2>{artist.name}</h2>
            <p className="subtle-text">sort name: {artist.sort_name}</p>
            {artist.mbid && <p className="subtle-text">MBID: {artist.mbid}</p>}
            <div className="entity-detail-stats">
              <span>{artist.recording_count} tracks</span>
              <span className="status-bar-sep">·</span>
              <span>{artist.release_group_count} albums</span>
              <span className="status-bar-sep">·</span>
              <span className="rating-summary">{formatAlbumRating(artist.rating)}</span>
            </div>
          </div>

          <div className="entity-detail-section">
            <h3 className="entity-detail-section-title">Albums ({artist.release_groups.length})</h3>
            {artist.release_groups.length === 0 ? (
              <p className="empty-browser-state">No albums.</p>
            ) : (
              <div className="entity-detail-list">
                {artist.release_groups.map((rg) => (
                  <div key={rg.id} className="entity-detail-list-item">
                    <div className="entity-detail-list-item-main">
                      {releaseGroupLink(rg.id, rg.title)}
                      {rg.rg_type && <span className="entity-detail-badge">{rg.rg_type}</span>}
                    </div>
                    <div className="entity-detail-list-item-meta">
                      <span>{formatYear(rg.release_date)}</span>
                      <span> · {rg.recording_count} tracks</span>
                      <span> · <span className="rating-summary">{formatAlbumRating(rg.rating)}</span></span>
                      {rg.artist_credit_name && rg.primary_artist_id && rg.primary_artist_id !== artist.id && (
                        <span> · {artistLink(rg.primary_artist_id, rg.artist_credit_name)}</span>
                      )}
                    </div>
                  </div>
                ))}
              </div>
            )}
          </div>
        </div>
      </section>
    );
  }

  // ── Release group view ──────────────────────────────────────────────────

  if (nav.type === "release_group" && releaseGroup) {
    return (
      <section className="entity-detail-panel">
        <div className="entity-detail-header">
          <span className="panel-label">Album detail</span>
          <span className="entity-detail-nav">
            <button className="nav-btn" disabled={!canGoBack} onClick={onBack} type="button" title="Back (Alt+Left)">◀</button>
            <button className="nav-btn" disabled={!canGoForward} onClick={onForward} type="button" title="Forward (Alt+Right)">▶</button>
          </span>
          <button className="modal-close-btn" onClick={onClose} type="button">✕</button>
        </div>
        <div className="entity-detail-body">
          <div className="entity-detail-hero">
            <h2>{releaseGroup.title}</h2>
            {releaseGroup.artist_credit_name && releaseGroup.primary_artist_id ? (
              <p>{artistLink(releaseGroup.primary_artist_id, releaseGroup.artist_credit_name)}</p>
            ) : (
              <p className="subtle-text">Unknown Artist</p>
            )}
            {releaseGroup.rg_type && <p className="subtle-text">Type: {releaseGroup.rg_type}</p>}
            <div className="entity-detail-stats">
              <span>{formatYear(releaseGroup.release_date)}</span>
              <span className="status-bar-sep">·</span>
              <span>{releaseGroup.releases.length} release{releaseGroup.releases.length !== 1 ? "s" : ""}</span>
              <span className="status-bar-sep">·</span>
              <span className="rating-summary">{formatAlbumRating(releaseGroup.rating)}</span>
            </div>
          </div>

          {releaseGroup.releases.map((release) => (
            <div key={release.id} className="entity-detail-section">
              <div className="entity-detail-section-title-row">
                <h3 className="entity-detail-section-title">
                  {completenessIcon(release.completeness)}
                  {release.title}
                  {release.release_date && <> ({formatYear(release.release_date)})</>}
                </h3>
              </div>
              {release.completeness.type === "unknown" && (
                <p className="subtle-text completeness-reason">{release.completeness.reason}</p>
              )}
              {release.country && <p className="subtle-text">Country: {release.country}</p>}
              {release.label && <p className="subtle-text">Label: {release.label}</p>}
              {release.catalog_number && <p className="subtle-text">Catalog: {release.catalog_number}</p>}

              {release.completeness.type === "incomplete" && release.completeness.missing_tracks.length > 0 && (
                <div className="missing-tracks">
                  <span className="subtle-text missing-tracks-label">Missing sources:</span>
                  <div className="missing-tracks-list">
                    {release.completeness.missing_tracks.map((mt) => (
                      <span key={`${mt.disc_position}-${mt.track_position}`} className="missing-track-item">
                        {mt.recording_id
                          ? recordingLink(mt.recording_id, `Disc ${mt.disc_position}, Track ${mt.track_position}${mt.title ? ` — ${mt.title}` : ""}`)
                          : <span>{`Disc ${mt.disc_position}, Track ${mt.track_position}`}</span>
                        }
                      </span>
                    ))}
                  </div>
                </div>
              )}

              {release.mediums.map((medium) => (
                <div key={medium.id} className="entity-detail-medium">
                  <p className="entity-detail-medium-label">
                    {medium.format ?? "Disc"} {medium.position}
                    {medium.format && medium.position > 1 ? ` ${medium.position}` : ""}
                  </p>
                  <table className="entity-detail-track-table">
                    <thead>
                      <tr>
                        <th className="source-indicator-col">Src</th>
                        <th>#</th>
                        <th>Title</th>
                        <th>Artist</th>
                        <th>Duration</th>
                      </tr>
                    </thead>
                    <tbody>
                      {medium.tracks.map((track) => (
                        <tr key={track.id}>
                          <td className="source-indicator-col">
                            {track.has_source ? (
                              <span
                                className="source-indicator source-present"
                                title="Has source — click to add to queue"
                                onClick={() => onEnqueueTrack?.(track)}
                              >✓</span>
                            ) : (
                              <span className="source-indicator source-missing" title="No source available">✗</span>
                            )}
                          </td>
                          <td>{track.position}</td>
                          <td>{recordingLink(track.recording_id, track.title ?? track.recording_title)}</td>
                          <td>
                            {track.primary_artist_id && track.artist_credit_name
                              ? artistLink(track.primary_artist_id, track.artist_credit_name)
                              : "—"}
                          </td>
                          <td>{formatDuration(track.duration_ms)}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              ))}
            </div>
          ))}
        </div>
      </section>
    );
  }

  // ── Recording view ──────────────────────────────────────────────────────

  if (nav.type === "recording" && recording) {
    return (
      <section className="entity-detail-panel">
        <div className="entity-detail-header">
          <span className="panel-label">Recording detail</span>
          <span className="entity-detail-nav">
            <button className="nav-btn" disabled={!canGoBack} onClick={onBack} type="button" title="Back (Alt+Left)">◀</button>
            <button className="nav-btn" disabled={!canGoForward} onClick={onForward} type="button" title="Forward (Alt+Right)">▶</button>
          </span>
          <button className="modal-close-btn" onClick={onClose} type="button">✕</button>
        </div>
        <div className="entity-detail-body">
          <div className="entity-detail-hero">
            <h2>{recording.title}</h2>
            {recording.artist_credit_name && recording.primary_artist_id ? (
              <p>{artistLink(recording.primary_artist_id, recording.artist_credit_name)}</p>
            ) : (
              <p className="subtle-text">Unknown Artist</p>
            )}
            <div className="entity-detail-stats">
              <span>{formatDuration(recording.duration_ms)}</span>
              <span className="status-bar-sep">·</span>
              <span className="rating-summary">{stars(recording.rating)}</span>
              <span className="status-bar-sep">·</span>
              <span>{recording.play_count} play{recording.play_count !== 1 ? "s" : ""}</span>
              <span className="status-bar-sep">·</span>
              <span>Last: {recording.last_played ? recording.last_played.slice(0, 10) : "Never"}</span>
            </div>
          </div>

          {recording.genre || recording.bpm || recording.comment || recording.mbid || recording.acoustid || recording.artist_credit_text ? (
            <div className="entity-detail-section">
              <h3 className="entity-detail-section-title">Metadata</h3>
              <table className="entity-detail-meta-table">
                <tbody>
                  {recording.genre && <tr><td>Genre</td><td>{recording.genre}</td></tr>}
                  {recording.bpm && <tr><td>BPM</td><td>{recording.bpm}</td></tr>}
                  {recording.comment && <tr><td>Comment</td><td>{recording.comment}</td></tr>}
                  {recording.artist_credit_text && <tr><td>Artist credit</td><td>{recording.artist_credit_text}</td></tr>}
                  {recording.mbid && <tr><td>MBID</td><td className="entity-detail-monospace">{recording.mbid}</td></tr>}
                  {recording.acoustid && <tr><td>AcoustID</td><td className="entity-detail-monospace">{recording.acoustid}</td></tr>}
                </tbody>
              </table>
            </div>
          ) : null}

          {recording.releases.length > 0 && (
            <div className="entity-detail-section">
              <h3 className="entity-detail-section-title">Appears on</h3>
              <div className="entity-detail-list">
                {recording.releases.map((rel, i) => {
                  const pos = rel.disc_total && rel.disc_total > 1 && rel.disc_position
                    ? `Disc ${rel.disc_position}/${rel.disc_total}, Track ${rel.track_position ?? "—"}`
                    : rel.track_position != null
                      ? `Track ${rel.track_position}`
                      : null;
                  return (
                    <div key={i} className="entity-detail-list-item">
                      {releaseGroupLink(rel.release_group_id, rel.release_group_title)}
                      {pos && <span className="subtle-text"> ({pos})</span>}
                    </div>
                  );
                })}
              </div>
            </div>
          )}

          <div className="entity-detail-section">
            <h3 className="entity-detail-section-title">
              Sources ({recording.sources.length})
            </h3>
            {recording.sources.length === 0 ? (
              <p className="empty-browser-state">No sources.</p>
            ) : (
              recording.sources.map((source) => (
                <div
                  key={source.id}
                  className="entity-detail-source"
                  onContextMenu={source.file_path ? (e) => onSourceContextMenu?.(e, source.file_path!) : undefined}
                >
                  <div className="entity-detail-source-header">
                    <span className="entity-detail-badge">{source.source_type}</span>
                    {source.format && <span className="entity-detail-badge">{source.format}</span>}
                    {source.duration_ms && <span className="subtle-text">{formatDuration(source.duration_ms)}</span>}
                  </div>
                  {source.file_path && (
                    <p className="entity-detail-source-path">{source.file_path}</p>
                  )}
                  {source.replay_gain_track_db != null && (
                    <p className="subtle-text">
                      ReplayGain: {source.replay_gain_track_db.toFixed(2)} dB
                      (peak {source.replay_gain_track_peak?.toFixed(4) ?? "?"})
                    </p>
                  )}

                  {source.tags.length > 0 && (
                    <div className="entity-detail-tags-section">
                      <p className="entity-detail-tags-label">ID3 Tags ({source.tags.length})</p>
                      <table className="entity-detail-tags-table">
                        <thead>
                          <tr>
                            <th>Field</th>
                            <th>Frame</th>
                            <th>Value</th>
                          </tr>
                        </thead>
                        <tbody>
                          {source.tags.map((tag, i) => (
                            <tr key={i}>
                              <td>{tag.field_name}</td>
                              <td className="entity-detail-monospace">{tag.frame_id}</td>
                              <td className="entity-detail-tag-value">{tag.value}</td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                    </div>
                  )}
                </div>
              ))
            )}
          </div>
        </div>
      </section>
    );
  }

  // Fallback
  return (
    <section className="entity-detail-panel">
      <div className="entity-detail-header">
        <span className="panel-label">Detail</span>
        <span className="entity-detail-nav">
            <button className="nav-btn" disabled={!canGoBack} onClick={onBack} type="button" title="Back (Alt+Left)">◀</button>
            <button className="nav-btn" disabled={!canGoForward} onClick={onForward} type="button" title="Forward (Alt+Right)">▶</button>
          </span>
          <button className="modal-close-btn" onClick={onClose} type="button">✕</button>
      </div>
      <div className="entity-detail-body">
        <p className="empty-browser-state">Unknown entity type.</p>
      </div>
    </section>
  );
}
