In the Album Detail view, there is a button "Rescan all sources".

Sometimes there is something wrong with the data associated with a release or
album, and the "Rescan all sources" is a quick way to address a large variety
of these problems.

# Invalid/Old Album

One use case that "Rescan all sources" should be able to handle is that the
source field previously asserted the existence of some album by having a specific
value in the TALB frame (e.g. "Dance Dance Revolution Extreme") and so a release
with that name was created. But then subsequently, the ID3 tag was updated so that
the the TALB frame refers to a different album (e.g. "Dance Dance Revolution EXTREME ORIGINAL SOUNDTRACK")
and was reimported.

Now there is both a release "Dance Dance Revolution Extreme" and "Dance Dance Revolution EXTREME ORIGINAL SOUNDTRACK"
but the former should no longer exist, because in fact, no source asserts its existence.

The way this use case should get handled is as follows:

- When "Rescan all Sources" is invoked, we look at all the Recordings associated with this release.
- For each recording, we re-scan and process the ID3 tags of all sources associated with the recording.
- If there is none of the ID3 tags claim that a recording is associated with a given release, then we remove the association between the recording and the release.
- If a release has zero recordings after this process, we delete the release.

Note that this use case is distinct from the case where a release has recordings with no source. In this scenario, the recordings DO have sources. However, those sources do not assert the existence of the release that the recording is currently a part of.

Another edge case to be careful of:

A recording can genuinely appear on multiple releases. For example a band "Cool Band" might have a track "Cool Track" that appears both on the release "Cool album" and also "Best of Cool Band". The file system may also contain sources for both of these appearances, for example one folder for all the tracks of "Cool album" and a different folder for all the tracks of "Best of Cool Band". Thus it is not correct to conclude that just because you've found a source asserting that "Cool Track" is associated with "Cool Album", that means it's safe to remove the association with "Best of Cool Band". You should only remove the association between a recording and a release if you have look at ALL sources associated with a recording, and none of those sources have asserted an association with the target release.
