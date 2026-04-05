use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tracing_subscriber::fmt::writer::{MakeWriter, MakeWriterExt};

#[derive(Clone)]
struct SharedFileWriter {
    file: Arc<Mutex<File>>,
}

struct GuardedFileWriter {
    file: Arc<Mutex<File>>,
}

impl<'a> MakeWriter<'a> for SharedFileWriter {
    type Writer = GuardedFileWriter;

    fn make_writer(&'a self) -> Self::Writer {
        GuardedFileWriter {
            file: Arc::clone(&self.file),
        }
    }
}

impl Write for GuardedFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("log file lock poisoned"))?;
        file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("log file lock poisoned"))?;
        file.flush()
    }
}

pub fn init(log_path: &Path) -> anyhow::Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;

    let writer = SharedFileWriter {
        file: Arc::new(Mutex::new(file)),
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(writer.and(std::io::stdout))
        .with_ansi(false)
        .init();

    Ok(())
}
