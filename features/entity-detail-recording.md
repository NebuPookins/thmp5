# Recording Detail View

The "Recording detail" view is a panel in the browser layout that shows
comprehensive information about a single recording. It is one of three entity
detail views (artist, release group/album, recording) sharing the same panel
component (`EntityDetailView`), switched via a discriminated union on
`nav.type === "recording"`.

## Navigation

The detail panel has its own stack-based navigation history. Each entity detail
view receives:
- Back/Forward buttons (disabled at history bounds)
- Close button (clears the detail panel)
- Clickable links within the view (artist names, release group titles).

Activating the detail panel for a recording happens through:
- Right-click context menu on a recording anywhere in the UI → "View details"
- Clicking a link to a recording from some other entity detail view.

## Hero Section

The top of the view displays the recording's identity:

- **Title** — the recording's `title` field (mandatory, displayed as `<h2>`)
- **Artists** — A list of 0 or more artists names, taking from
  `recording.artists`, and linking to the appropriate artist entity detail
  view.
- **Artist Credit Name** If present and different from the direct stringification
  of the artist names, using `artist_credit_name` and shown in the UI as
  "Credited As: ${value}". This is not a clickable link.
- **Stats row** — inline display of:
  - Duration (formatted via `formatDuration`)
  - User rating (star representation, or "Unrated")
  - Play count (e.g. "3 plays" / "1 play")
  - Last played date (formatted as YYYY-MM-DD, or "Never")

## Metadata Table

A conditional section that only renders when at least one metadata field is
non-null. Displays a key-value table with rows for:

- Genre
- BPM
- Comment
- Artist credit (the raw `artist_credit_text` field, distinct from the
  computed `artist_credit_name`)
- MBID (MusicBrainz Identifier, monospace), which should link to the appropriate
  MusicBrainz page, e.g. https://musicbrainz.org/recording/8726b172-05c2-4350-a730-bbdef4b219b7
- AcoustID (monospace)

## "Appears On" Section

Lists every release group this recording is part of, ordered by release group
title. Each entry shows:

- Release group title (clickable link — navigates to the album detail view)
- Position annotation (subtle text) showing where on that release the
  recording appears:
  - For multi-disc releases: "Disc N/M, Track P"
  - For single-disc releases: "Track P"

Data comes from the `releases` field (`Vec<ReleaseInfo>`), which is populated
by a SQL JOIN: `track → medium → release → release_group` for the recording.

## Sources Section

Shows all source files/streams associated with the recording, with a header
counting the total (e.g. "Sources (3)"). Each source is in its own collapsible
card-style block with:

- **Source type badge** — e.g. "local_file", "youtube"
- **Format badge** — e.g. "MP3", "FLAC", "OGG" (when available)
- **Duration** — formatted duration if non-null
- **File path** — the full filesystem path (clickable via right-click context
  menu; calls `onSourceContextMenu`)
- **ReplayGain** — track gain in dB and peak amplitude (R128 reference), shown
  when `replay_gain_track_db` is non-null (e.g. "ReplayGain: -8.23 dB (peak
  0.9999)")
- **ID3 Tags table** — for `local_file` sources only, a complete dump of all
  raw ID3 frames read from the file. Shows three columns:
  - Field (human-readable name, e.g. "Title", "Album", "Artist")
  - Frame (the raw ID3 frame ID, monospace, e.g. "TIT2", "TALB", "TPE1")
  - Value (the tag value, with word-break for long values)

  Tag reading is done on-demand per source via `list_all_tags` in the Rust
  scanner, which tries `lofty` first and falls back to `taglib` C binding.
  Non-local-file sources get an empty tag list.

Empty source list shows "No sources." placeholder text.

## "Split Recording..." Button

Only rendered when the recording has more than one source
(`recording.sources.length > 1`) and the `onSplitRecording` callback is
provided. Opens a modal for splitting sources off into a new recording (see
split recording feature).

## "Rescan all sources..." Button

When click, triggers a rescan of all sources associated with the recording.

This also applies any appropriate automatic fixes.

### Duplicate track entries

One scenario that requires an automatic fix is if a recording has a duplicate
track entry. A recording can appear on multiple albums and thus have multiple
track entries, so have multiple track entries is not itself erroneous. However,
if the recording has multiple track entries all relating it to the same album
and track position, then one or more of those track entries are superfluous.

For example, the recording "Presto" by "Osamu Kubota" appears on (at least) two
distinct albums: "History of beatmania IIDX" (as track 15) and
"beatmania IIDX 3rd Style Original Soundtracks" (as track 24).

If there are two track entries associated with the recording, one pointing to
"History of beatmania IIDX" track 15 and one pointing to
"beatmania IIDX 3rd Style Original Soundtracks" track 24, then this is all
correct and no correction needs to be performed.

However, if there are three track entries associated with the recording:

- "History of beatmania IIDX" track 15
- "History of beatmania IIDX" track 15
- "beatmania IIDX 3rd Style Original Soundtracks" track 24

Then one of the "History of beatmania IIDX" track 15 entries is duplicated and
should be deleted.