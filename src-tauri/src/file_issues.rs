use serde::Serialize;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileIssueKind {
    ImportError,
    PlaybackError,
    OrphanSource,
    DuplicateFrame,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileIssue {
    pub file_path: String,
    pub kind: FileIssueKind,
    pub message: String,
    /// Present only for OrphanSource issues — the source row id so
    /// the frontend can request a fix without re‑querying.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_id: Option<String>,
    /// For DuplicateFrame issues: which ID3v2 frame was duplicated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<String>,
    /// For DuplicateFrame issues: human-readable field name for the frame.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field_name: Option<String>,
    /// For DuplicateFrame issues: the value Lofty returned (last tag).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lofty_value: Option<String>,
    /// For DuplicateFrame issues: the corrected value (first tag).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corrected_value: Option<String>,
}

/// Shared, in-memory log of files that have encountered problems.
/// Cheap to clone — all clones share the same underlying list.
#[derive(Clone, Default)]
pub struct FileIssueLog {
    issues: Arc<Mutex<Vec<FileIssue>>>,
}

impl FileIssueLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_import_error(&self, file_path: impl Into<String>, message: impl Into<String>) {
        self.push(FileIssue {
            file_path: file_path.into(),
            kind: FileIssueKind::ImportError,
            message: message.into(),
            source_id: None,
            recording_id: None,
            frame_id: None,
            field_name: None,
            lofty_value: None,
            corrected_value: None,
        });
    }

    pub fn push_playback_error(&self, file_path: impl Into<String>, message: impl Into<String>) {
        self.push(FileIssue {
            file_path: file_path.into(),
            kind: FileIssueKind::PlaybackError,
            message: message.into(),
            source_id: None,
            recording_id: None,
            frame_id: None,
            field_name: None,
            lofty_value: None,
            corrected_value: None,
        });
    }

    pub fn push_orphan_source(
        &self,
        file_path: impl Into<String>,
        message: impl Into<String>,
        source_id: impl Into<String>,
        recording_id: impl Into<String>,
    ) {
        self.push(FileIssue {
            file_path: file_path.into(),
            kind: FileIssueKind::OrphanSource,
            message: message.into(),
            source_id: Some(source_id.into()),
            recording_id: Some(recording_id.into()),
            frame_id: None,
            field_name: None,
            lofty_value: None,
            corrected_value: None,
        });
    }

    pub fn push_duplicate_frame(
        &self,
        file_path: impl Into<String>,
        frame_id: impl Into<String>,
        field_name: impl Into<String>,
        lofty_value: impl Into<String>,
        corrected_value: impl Into<String>,
    ) {
        let frame_id = frame_id.into();
        let field_name = field_name.into();
        let lofty_value = lofty_value.into();
        let corrected_value = corrected_value.into();
        self.push(FileIssue {
            file_path: file_path.into(),
            kind: FileIssueKind::DuplicateFrame,
            message: format!(
                "ID3v2 {field_name} (frame {frame_id}) has conflicting values: \
                 Lofty picked {lofty_value:?}, raw first-value is {corrected_value:?}"
            ),
            source_id: None,
            recording_id: None,
            frame_id: Some(frame_id),
            field_name: Some(field_name),
            lofty_value: Some(lofty_value),
            corrected_value: Some(corrected_value),
        });
    }

    pub fn all(&self) -> Vec<FileIssue> {
        self.issues
            .lock()
            .map(|issues| issues.clone())
            .unwrap_or_default()
    }

    /// Remove issues that don't satisfy the predicate.
    pub fn retain<F>(&self, f: F)
    where
        F: FnMut(&FileIssue) -> bool,
    {
        if let Ok(mut issues) = self.issues.lock() {
            issues.retain(f);
        }
    }

    fn push(&self, issue: FileIssue) {
        if let Ok(mut issues) = self.issues.lock() {
            issues.push(issue);
        }
    }
}
