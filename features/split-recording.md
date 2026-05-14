Sometimes a recording will have two sources, which is a supported use case for
this music player. The app uses AcoustID to identify when two sources might
refer to the same recording and auto merge them.

However, sometimes it might do this incorrectly, and so there needs to be a way
for the user to split a recording with two or more sources into two recordings.

When the user right clicks on a recording with two or more sources associated
with it, the context menu will contain a menu item "Split recording". When that
item is selected, a modal window pops up.

The modal shows a list of all sources with checkboxes next to them, asking
which sources should be moved to a new recording. Any unchecked sources will
remain associated with the original recording, while a new recording will be
generated for the sources whose checkbox is selected.

---

When showing the sources, if we are unable to show the full path (because it is
too long to fit in the window), we should prioritize showing the path elements
that are unique to that recording.

For example, if the two sources are:

- /home/nebu/Music/!Full Albums/Game - 2003 - Beatmania IIDX 9th OST/Disc 1/Beatmania 5th Disc 1 (32) 290 - Paranoia Survivor Max.mp3
- /home/nebu/Music/!Full Albums/Game - 2003 - Dance Dance Revolution Extreme/DDR Extreme Disc 1 (26) 290 - Paranoia Survivor Max.mp3

Rather than showing the prefixes like:

- /home/nebu/Music/!Full Albums/Gam...
- /home/nebu/Music/!Full Albums/Gam...

which does not give the user enough information to differentiate between the
two sources, we should show:

- ...Beatmania IIDX 9th OST/Disc 1/...
- ...Dance Dance Revolution Extreme...

I.e. we should eliminate the prefix and suffix that are identical across
multiple sources, and then show whatever remains.

Further more, when the user hovers the cursor over the abbreviated form of the
paths, a tooltip should show the full path.

---

There needs to be at least one source which is checked and one source which is
not checked for the split to proceed.

---

When creating a new recording for the selected sources, we should NOT copy
over the metadata from the old source. Instead, we should re-derive the metadata
for the recording by processing the ID3 tags of the selected sources, as if they
were being newly imported into the library.

We should also show a preview of the metadata that will be on the newly created
recording so that the user can check that they have selected the correct set of
sources.

We do not provide text boxes for the user to override or manually specify the
metadata value. One of the philosophies of this project is that the user is
responsible for ensuring that the mp3 files are correctly tagged (perhaps using
an external tool like MusicBrainz Picard), and we will just process whatever the
ID3 tags say.

The preview of the metadata should include:

- Title
- List of artists
- List of [ Release (Album), Disc number / Disc Total, Track number / Track Total ]
- List of tags

Often these lists will have length 1, but we support the case where there may
be more than one.