When running the "Rescan source" operation on a file, we may occasionally
encounter new issues with a file. For example, we might detect that a file has
duplicate ID3v2 tags, in which case we will add those issues to a list of found
file issues.

When the list of file issues is non-empty, a warning icon will appear in the
status bar, and if the user clicks on it, a modal window will open up listing
all the file issues. Some issues may be presented with buttons to apply a fix
to the issue. For example, if a file has duplicate tags, we might present the
user with the alternative tags and let the user pick which one applies.

---

Edge case: The user performs a source scan on a file "a.mp3". We detect an
issue (e.g. duplicate tag), and so we add an entry in the list of file issues.
The user performs a source scan on the same file "a.mp3". We still detect the
issue, but we do not add a new entry in the list of file issues, because that
entry is already present there.

---

Edge case: A file issue is detect for a file a.mp3, and so an item is added to
the list of file issues. The user uses an external tool to address the issue
(e.g. deletes the duplicate tag) and then performs "Rescan source" on the
source file a.mp3. Thmp5 should detect that the issue is fixed and remove the
entry from the list of file issues.
