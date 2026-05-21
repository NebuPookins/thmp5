# Album Detail View

The "Album detail" view is a panel in the browser layout that shows
comprehensive information about a single album (called a "release group" in the
data model). It is one of three entity detail views (artist, album, recording)
and replaces the track-list column in the three-column browser layout when
active.

## Navigation

The detail panel has its own stack-based navigation history with:

- **Back/Forward buttons** — disabled at the ends of the history stack.
  Keyboard shortcuts: `Alt+Left` / `Alt+Right`.
- **Close button** — dismisses the panel and returns to the track-list column.
- **Clickable entity links** — any artist name or recording title within the
  view navigates to that entity's detail panel, pushing onto the history stack.

Activating the album detail panel happens through:

- Right-click context menu on an album anywhere in the UI → "View details"
- Clicking an album title link from some other entity detail view (e.g. from
  the "Appears on" section of a recording detail view).

## Hero Section

The top of the view displays the album's identity:

- **Title** — the album's title, displayed as `<h2>`.
- **Artist** — the primary artist name, linked to the artist detail view. If
  unknown, shows "Unknown Artist" in subtle text.
- **Type badge** — e.g. "album", "single", "EP", "compilation" (shown when
  available).
- **Stats row** — inline display of:
  - Date of release (formatted from the release date, or "Unknown date")
  - Release count (e.g. "2 releases")
  - Average rating (star-based, or "Unrated")

## Action Buttons

Two action buttons sit below the hero stats:

- **"Rescan all sources"** — re-reads metadata from all source files
  associated with any recording in this album. Used to fix stale metadata,
  remove orphaned release associations, and pick up tag changes. See
  `album-detail-rescan-all-sources.md` for detailed behavior.
- **"Merge album…"** — opens an inline merge panel.

## Merge Panel

When "Merge album…" is clicked, an inline panel appears below the hero section
with a search field. The user types to search for another album; results update
as they type (debounced).

Each search result shows: album title, artist name, and track count. Selecting
a target prompts a confirmation dialog:

> Merge "Target Album" into "Current Album"?
> All tracks from "Target Album" will be moved into "Current Album".
> "Target Album" will be deleted.

On confirmation, the merge runs and the detail panel navigates to the surviving
album. A cancel button dismisses the panel without merging.

## Release Sections

Below the hero, the view shows one section per **release** (a specific edition
of the album). Each release section has:

### Release Header

- **Completeness icon** — one of three visual indicators:
  - Green checkmark (✓) — all expected tracks have source files
  - Yellow/red cross (✗) — some tracks lack source files (hovering reveals
    which disc/position/title is missing)
  - Grey question mark (?) — completeness could not be determined
- **Release title** with year in parentheses

### Completeness Details (conditional)

When completeness is **unknown**, a block of text explains why (e.g. "Sources
disagree on total track count") followed by groups of conflicting source file
paths, each with a description of what each group asserts.

When completeness is **incomplete**, a "Missing sources:" section lists the
missing tracks by disc position, track position, and title. Each missing track
that has a recording in the database links to that recording's detail view.

#### Edge cases:

- Note that if a release consist of multiple discs and each disc contains a
  different number of tracks, this is not a disagreement. For example, if an
  album has a disc 1 with 13 tracks and a disc 2 with 15 tracks, and the 13
  tracks on disc 1 all claim a "total tracks" of 13, and the 15 tracks on disc
  2 all claim a "total tracks" of 15, there is no disagreement.
- Note that tracks can appear on multiple releases, and this does not
  (necessarily) constitute a disagreement. For example, the track "Pink Rose"
  by "Kiyommy+Seiya" appears on (at least) two different releases:
  "Keyboardmania 3rdMIX" and "Beatmania IIDX 12th Mix". On
  "Keyboardmania 3rdMIX", it (along with all other tracks on that release)
  asserts that there are 23 tracks total. On "Beatmania IIDX 12th Mix", it
  (along with all other tracks on that release) asserts that there are 35 tracks
  total. When viewing the album detail for "Keyboardmania 3rdMIX", this should
  not lead to a disagreement where Pink Rose says there are 35 tracks on
  "Keyboardmania 3rdMIX". I.e. the assertion for the number of total tracks
  belongs on a source and a track but NOT on a recording.

### Release Metadata (conditional)

Shown only when non-null:

- Country
- Label
- Catalog number

### Medium / Disc Tables

Each release contains one or more **mediums** (physical or logical discs).
Each medium is labeled by its format and position (e.g. "Disc 1/n",
"MP3 1/n", "Vinyl 1/n").

Each medium has a table with the following columns:

- Src: A checkmark (✓) if a source file exists for this track, or a cross (✗)
  if not. Clicking the checkmark adds the track to the play queue.
- \#: Track position number
- Title: Track/recording title (clickable — navigates to the recording detail view)
- Artist: List of artists (clickable — navigates to artist detail). Also
  includes "credited as" if it differs from the stringification of the artist name.
- Duration: Formatted duration (e.g. "3:45"), or "Unknown"

## Track Queue Integration

Tracks with a source file (green checkmark in the Src column) can be
clicked to add them to the play queue. Tracks without a source (cross in
the Src column) cannot be queued from this view.

## Automatic Reload

When an external event updates recordings within this album (e.g. after a
rescan completes), the detail panel automatically refreshes its data so the
user sees the current state without manually re-navigating.
