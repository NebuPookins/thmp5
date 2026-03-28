use serde::Serialize;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileIssueKind {
    ImportError,
    PlaybackError,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileIssue {
    pub file_path: String,
    pub kind: FileIssueKind,
    pub message: String,
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
        });
    }

    pub fn push_playback_error(&self, file_path: impl Into<String>, message: impl Into<String>) {
        self.push(FileIssue {
            file_path: file_path.into(),
            kind: FileIssueKind::PlaybackError,
            message: message.into(),
        });
    }

    pub fn all(&self) -> Vec<FileIssue> {
        self.issues
            .lock()
            .map(|issues| issues.clone())
            .unwrap_or_default()
    }

    fn push(&self, issue: FileIssue) {
        if let Ok(mut issues) = self.issues.lock() {
            issues.push(issue);
        }
    }
}
