export type LoveToggleAction = {
  command: "lastfm_love_track" | "lastfm_unlove_track";
  nextLoved: boolean;
};

/** Decide which Last.fm command to invoke when the heart button is clicked, given the current loved state. */
export function loveToggleAction(currentlyLoved: boolean): LoveToggleAction {
  return currentlyLoved
    ? { command: "lastfm_unlove_track", nextLoved: false }
    : { command: "lastfm_love_track", nextLoved: true };
}

/** A loved track as returned by the `lastfm_get_loved_tracks` command. */
export type LovedTrack = {
  artist: string;
  track: string;
};

/** Last.fm matches artist/title case-insensitively, so loved lookups do too. */
function lovedKey(artist: string, title: string): string {
  const norm = (s: string) => s.normalize("NFC").toLowerCase();
  return `${norm(artist)}\0${norm(title)}`;
}

/** The empty track set. */
export const NO_TRACKS: ReadonlySet<string> = new Set();

/** Build a lookup set of tracks (e.g. loved tracks) for case-insensitive membership tests. */
export function buildTrackSet(tracks: readonly LovedTrack[]): ReadonlySet<string> {
  return new Set(tracks.map((t) => lovedKey(t.artist, t.track)));
}

/** Whether (artist, title) is in the set; false when the set is not available or the track has no artist. */
export function hasTrack(set: ReadonlySet<string> | null, artist: string | null, title: string): boolean {
  return set !== null && artist !== null && set.has(lovedKey(artist, title));
}

/**
 * A set with (artist, title) present or absent; the input is never mutated.
 * Returns the same set when nothing changes, so React can skip the update.
 */
export function withTrack(
  set: ReadonlySet<string>,
  artist: string,
  title: string,
  present: boolean,
): ReadonlySet<string> {
  const key = lovedKey(artist, title);
  if (set.has(key) === present) return set;
  const next = new Set(set);
  if (present) next.add(key);
  else next.delete(key);
  return next;
}

/** Whether two (artist, title) pairs name the same track; a missing artist never matches. */
export function sameTrack(
  artistA: string | null,
  titleA: string,
  artistB: string | null,
  titleB: string,
): boolean {
  return artistA !== null && artistB !== null && lovedKey(artistA, titleA) === lovedKey(artistB, titleB);
}

/** A love/unlove that was applied while the loved list was not yet loaded. */
export type TrackEdit = { artist: string; title: string; loved: boolean };

/** Apply edits in order (a later edit for the same track wins); the input set is not mutated. */
export function applyTrackEdits(set: ReadonlySet<string>, edits: readonly TrackEdit[]): ReadonlySet<string> {
  return edits.reduce((acc, e) => withTrack(acc, e.artist, e.title, e.loved), set);
}
