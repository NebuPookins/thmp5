//! The error type returned by every Tauri command.
//!
//! Converting any error into a [`CommandError`] (usually via `?`) logs it
//! exactly once, at the moment it is created. Because that happens inside the
//! command's `#[tracing::instrument]` span, the log line automatically carries
//! the command name and its identifying inputs. The frontend receives a plain
//! string.

use std::fmt;

/// Why a command failed. The two cases are distinguishable by type, and both
/// are logged when constructed.
#[derive(Debug)]
pub enum CommandError {
    /// An underlying operation failed (database, I/O, decoding, ...).
    /// Logged at `error` level with the full cause chain.
    Failed(anyhow::Error),
    /// The request was refused (invalid input, entity not found, ...).
    /// Logged at `warn` level.
    Rejected(String),
}

impl CommandError {
    /// Refuse a request with a message meant for the user.
    pub fn rejected(message: impl Into<String>) -> Self {
        let message = message.into();
        tracing::warn!(%message, "Command rejected");
        Self::Rejected(message)
    }
}

// Deliberately does not implement `std::error::Error`: that would make this
// blanket impl overlap with the reflexive `From<T> for T`.
impl<E: Into<anyhow::Error>> From<E> for CommandError {
    fn from(error: E) -> Self {
        let error = error.into();
        tracing::error!(error = %format!("{error:#}"), "Command failed");
        Self::Failed(error)
    }
}

/// The message the frontend sees: the full `{:#}` cause chain for failures.
impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed(error) => write!(f, "{error:#}"),
            Self::Rejected(message) => f.write_str(message),
        }
    }
}

impl serde::Serialize for CommandError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use std::io;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Capture {
        type Writer = Capture;
        fn make_writer(&'a self) -> Capture {
            self.clone()
        }
    }

    fn capture_logs<T>(run: impl FnOnce() -> T) -> (T, String) {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .finish();
        let value = tracing::subscriber::with_default(subscriber, run);
        let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        (value, logs)
    }

    #[tracing::instrument(skip_all, fields(source_id = %source_id))]
    fn failing_command(source_id: &str) -> Result<(), CommandError> {
        Err(io::Error::other("disk on fire")).context("Reading tags")?;
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(source_id = %source_id))]
    fn rejecting_command(source_id: &str) -> Result<(), CommandError> {
        Err(CommandError::rejected(format!(
            "Source {source_id} not found"
        )))
    }

    #[test]
    fn failure_is_logged_once_with_command_name_and_inputs() {
        let (result, logs) = capture_logs(|| failing_command("src-42"));
        assert!(result.is_err());
        assert_eq!(logs.lines().count(), 1, "logs: {logs}");
        assert!(logs.contains("ERROR"), "logs: {logs}");
        assert!(logs.contains("failing_command"), "logs: {logs}");
        assert!(logs.contains("src-42"), "logs: {logs}");
        assert!(logs.contains("disk on fire"), "logs: {logs}");
    }

    #[test]
    fn rejection_is_logged_once_as_a_warning_with_span_context() {
        let (result, logs) = capture_logs(|| rejecting_command("src-7"));
        assert!(result.is_err());
        assert_eq!(logs.lines().count(), 1, "logs: {logs}");
        assert!(logs.contains("WARN"), "logs: {logs}");
        assert!(logs.contains("rejecting_command"), "logs: {logs}");
        assert!(logs.contains("src-7"), "logs: {logs}");
    }

    #[test]
    fn serializes_to_a_string_with_the_full_cause_chain() {
        let (error, _) = capture_logs(|| failing_command("src-1").unwrap_err());
        assert_eq!(
            serde_json::to_string(&error).unwrap(),
            r#""Reading tags: disk on fire""#
        );
    }

    #[test]
    fn rejection_serializes_to_its_message() {
        let (error, _) = capture_logs(|| CommandError::rejected("Nope."));
        assert_eq!(serde_json::to_string(&error).unwrap(), r#""Nope.""#);
    }
}
