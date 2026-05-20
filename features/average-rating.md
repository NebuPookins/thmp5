# Average Rating Policy

Ratings are stored at the **source** level (one per file) in the `source_rating`
table. All derived ratings at higher levels are computed on demand; nothing is
stored.

## Hierarchy

```
Source          — user-assigned integer 1–5
Recording       — average of its sources' ratings
Track           — the recording's rating (one recording per track)
Release         — average of all track ratings, all tracks equally weighted
                  (disc count does not affect weighting)
Release Group   — average of its releases' ratings
Artist          — flat average of all the artist's recording ratings
                  (recordings are equally weighted, not grouped by album)
```

## Design decisions

**Ratings live on sources, not recordings.**  
A recording can have multiple source files (e.g. a FLAC and an MP3 of the same
track). Each file can be rated independently. The recording's displayed rating
is the average.

**All tracks in a release are equally weighted.**  
A disc with 10 tracks and a disc with 1 track contribute equally track-by-track;
the 10-track disc does not dominate. Disc grouping is used only for display
(track listing), not for rating computation.

**A track has exactly one recording.**  
A recording can appear on multiple tracks across different releases (e.g. a
bonus track on the UK edition). In that scenario the same recording's avg_rating
feeds into each release's average independently.

**Artist rating is a flat average of recordings.**  
An artist with 100 recordings on one album and 1 on another would have those 101
recordings weighted equally. Albums are not treated as aggregation units for
the artist level.