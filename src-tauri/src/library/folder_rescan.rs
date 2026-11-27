//! Planning for "Rescan folder": which library files under a folder to re-read, and which audio
//! files found there are new to the library and need importing.

use super::scanner::is_audio_file;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// The work a rescan does. `rescan` holds library files to re-read (a missing one is reported as a
/// failure, not removed), `import` holds audio files not yet in the library, and `walk_errors`
/// describes directory entries that could not be read while looking for files.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RescanPlan {
    pub rescan: Vec<String>,
    pub import: Vec<String>,
    pub walk_errors: Vec<String>,
}

impl RescanPlan {
    pub fn rescan_only(rescan: Vec<String>) -> Self {
        Self {
            rescan,
            ..Self::default()
        }
    }

    pub fn total(&self) -> usize {
        self.rescan.len() + self.import.len() + self.walk_errors.len()
    }
}

/// Resolve `folder` to the same folder spelled under `music_root` as configured, or `None` if it
/// is not inside the music root (the root itself counts as inside).
///
/// The library scan walks the configured root, so stored paths start with it as written. A folder
/// picker may hand back a different spelling (e.g. through a symlink), so containment is checked
/// on canonical paths and the result re-joins the relative part onto the configured root; walking
/// that keeps newly imported paths consistent with the scan's instead of creating a second
/// `source` row for the same file.
pub fn resolve_folder_in_root(folder: &Path, music_root: &Path) -> Result<Option<PathBuf>> {
    let canonical_folder = folder
        .canonicalize()
        .with_context(|| format!("Failed to resolve folder {}", folder.display()))?;
    let canonical_root = music_root
        .canonicalize()
        .with_context(|| format!("Failed to resolve music root {}", music_root.display()))?;
    Ok(canonical_folder
        .strip_prefix(&canonical_root)
        .ok()
        .map(|relative| music_root.join(relative)))
}

/// Plan a recursive rescan of `folder`: every library path under it is re-read, and every audio
/// file found under it that is not in `library_paths` is imported. Walks the file system.
pub fn plan_folder_rescan(folder: &Path, library_paths: Vec<String>) -> RescanPlan {
    let known: HashSet<&str> = library_paths.iter().map(String::as_str).collect();
    let (import, walk_errors) = WalkDir::new(folder).follow_links(true).into_iter().fold(
        (Vec::new(), Vec::new()),
        |(mut import, mut errors), entry| {
            match entry {
                Ok(e) if e.file_type().is_file() && is_audio_file(e.path()) => {
                    let path = e.path().to_string_lossy().into_owned();
                    if !known.contains(path.as_str()) {
                        import.push(path);
                    }
                }
                Ok(_) => {}
                Err(error) => errors.push(match error.path() {
                    Some(p) => format!("{}: {error}", p.display()),
                    None => error.to_string(),
                }),
            }
            (import, errors)
        },
    );
    let rescan = library_paths
        .iter()
        .filter(|p| Path::new(p).starts_with(folder))
        .cloned()
        .collect();
    RescanPlan {
        rescan,
        import,
        walk_errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"").unwrap();
    }

    fn s(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn plan_rescans_library_files_under_folder_and_imports_new_ones() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Album");
        let known = folder.join("01.mp3");
        let known_nested = folder.join("CD2/01.mp3");
        let new_nested = folder.join("CD2/02.flac");
        let not_audio = folder.join("cover.jpg");
        let elsewhere = dir.path().join("Other/01.mp3");
        let missing = folder.join("gone.mp3");
        for p in [&known, &known_nested, &new_nested, &not_audio, &elsewhere] {
            touch(p);
        }

        let mut plan = plan_folder_rescan(
            &folder,
            vec![s(&known), s(&known_nested), s(&elsewhere), s(&missing)],
        );
        plan.rescan.sort();

        let mut expected_rescan = vec![s(&known), s(&known_nested), s(&missing)];
        expected_rescan.sort();
        assert_eq!(
            plan,
            RescanPlan {
                rescan: expected_rescan,
                import: vec![s(&new_nested)],
                walk_errors: vec![],
            }
        );
    }

    #[test]
    fn plan_does_not_match_sibling_folder_sharing_a_name_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Album");
        let sibling = dir.path().join("Album 2/01.mp3");
        touch(&sibling);
        std::fs::create_dir_all(&folder).unwrap();

        let plan = plan_folder_rescan(&folder, vec![s(&sibling)]);
        assert_eq!(plan, RescanPlan::default());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_accepts_root_and_subfolders_spelled_under_configured_root() {
        let dir = tempfile::tempdir().unwrap();
        let real_root = dir.path().join("real");
        let sub = real_root.join("Artist/Album");
        std::fs::create_dir_all(&sub).unwrap();
        let link_root = dir.path().join("link");
        std::os::unix::fs::symlink(&real_root, &link_root).unwrap();

        assert_eq!(
            resolve_folder_in_root(&sub, &link_root).unwrap(),
            Some(link_root.join("Artist/Album"))
        );
        assert_eq!(
            resolve_folder_in_root(&real_root, &link_root).unwrap(),
            Some(link_root.clone())
        );
    }

    #[test]
    fn resolve_rejects_folder_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("music");
        let outside = dir.path().join("downloads");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        assert_eq!(resolve_folder_in_root(&outside, &root).unwrap(), None);
    }
}
