//! Memory usage diagnostics written to the application log.
//!
//! Snapshots are logged at startup, after every catalog (re)load, and every
//! [`SNAPSHOT_INTERVAL`], so a long session's log shows how memory evolves and
//! which part grows: our own process (split into glibc heap in use vs. free,
//! to tell leaks from fragmentation) or the WebKit child processes that host
//! the UI.

use crate::file_issues::FileIssueLog;
use crate::storage::{Catalog, CatalogStats};
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Number of times the in-memory catalog has been built since startup.
static CATALOG_LOADS: AtomicU64 = AtomicU64::new(0);

/// Resident memory of one process, from `/proc/<pid>/status`, in KiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcessMemory {
    rss_kib: u64,
    peak_rss_kib: u64,
    anon_kib: u64,
    file_kib: u64,
    shmem_kib: u64,
    swap_kib: u64,
}

/// Parses the memory fields of a `/proc/<pid>/status` file. Returns `None` for
/// processes without them (e.g. kernel threads).
fn parse_status(status: &str) -> Option<ProcessMemory> {
    let field = |name: &str| -> Option<u64> {
        status.lines().find_map(|line| {
            line.strip_prefix(name)?
                .strip_prefix(':')?
                .trim()
                .strip_suffix("kB")?
                .trim()
                .parse()
                .ok()
        })
    };
    Some(ProcessMemory {
        rss_kib: field("VmRSS")?,
        peak_rss_kib: field("VmHWM")?,
        anon_kib: field("RssAnon")?,
        file_kib: field("RssFile")?,
        shmem_kib: field("RssShmem")?,
        swap_kib: field("VmSwap")?,
    })
}

fn read_process_memory(pid: &str) -> Option<ProcessMemory> {
    parse_status(&std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?)
}

/// One entry of the process table: `(pid, parent pid, command name)`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcEntry {
    pid: u32,
    ppid: u32,
    name: String,
}

/// Parses `/proc/<pid>/stat` (`pid (comm) state ppid ...`). The command name
/// may itself contain spaces and parentheses, so it spans to the *last* `)`.
fn parse_stat(stat: &str) -> Option<ProcEntry> {
    let (head, tail) = stat.rsplit_once(')')?;
    let (pid, name) = head.split_once(" (")?;
    let ppid = tail.split_whitespace().nth(1)?;
    Some(ProcEntry {
        pid: pid.trim().parse().ok()?,
        ppid: ppid.parse().ok()?,
        name: name.to_string(),
    })
}

/// All transitive descendants of `root`, in breadth-first order.
fn descendants(root: u32, table: &[ProcEntry]) -> Vec<&ProcEntry> {
    let mut found: Vec<&ProcEntry> = Vec::new();
    let mut frontier = vec![root];
    while !frontier.is_empty() {
        let children: Vec<&ProcEntry> = table
            .iter()
            .filter(|e| frontier.contains(&e.ppid))
            .collect();
        frontier = children.iter().map(|e| e.pid).collect();
        found.extend(children);
    }
    found
}

fn process_table() -> Vec<ProcEntry> {
    std::fs::read_dir("/proc")
        .map(|dir| {
            dir.filter_map(|entry| {
                let name = entry.ok()?.file_name().into_string().ok()?;
                name.parse::<u32>().ok()?;
                parse_stat(&std::fs::read_to_string(format!("/proc/{name}/stat")).ok()?)
            })
            .collect()
        })
        .unwrap_or_default()
}

/// The full executable name of `pid`; `comm` in `/proc/<pid>/stat` is
/// truncated to 15 characters (`WebKitWebProces`).
fn executable_name(pid: u32) -> Option<String> {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    Some(exe.file_name()?.to_string_lossy().into_owned())
}

/// A descendant process, e.g. `WebKitWebProcess` hosting the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChildProcess {
    pid: u32,
    name: String,
    memory: ProcessMemory,
}

/// glibc allocator statistics for this process, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeapStats {
    /// Memory obtained from the OS for malloc arenas (excluding mmapped chunks).
    arena_bytes: usize,
    /// Large allocations served directly by mmap.
    mmap_bytes: usize,
    /// Bytes currently allocated from the arenas.
    in_use_bytes: usize,
    /// Free bytes held in the arenas; a large value relative to `in_use_bytes`
    /// means fragmentation rather than live data.
    free_bytes: usize,
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn heap_stats() -> Option<HeapStats> {
    // mallinfo2 only exists in glibc >= 2.33. Resolve it at runtime rather
    // than linking it, so release builds still start on older glibc.
    // SAFETY: dlsym with a NUL-terminated name has no other preconditions.
    let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"mallinfo2".as_ptr()) };
    if symbol.is_null() {
        return None;
    }
    // SAFETY: glibc's `mallinfo2` symbol has exactly this signature.
    let mallinfo2: extern "C" fn() -> libc::mallinfo2 = unsafe { std::mem::transmute(symbol) };
    // mallinfo2 has no preconditions; it only reads allocator state.
    let info = mallinfo2();
    Some(HeapStats {
        arena_bytes: info.arena,
        mmap_bytes: info.hblkhd,
        in_use_bytes: info.uordblks,
        free_bytes: info.fordblks,
    })
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn heap_stats() -> Option<HeapStats> {
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MemorySnapshot {
    process: Option<ProcessMemory>,
    heap: Option<HeapStats>,
    children: Vec<ChildProcess>,
}

impl MemorySnapshot {
    fn capture() -> Self {
        let table = process_table();
        let children = descendants(std::process::id(), &table)
            .into_iter()
            .filter_map(|e| {
                Some(ChildProcess {
                    pid: e.pid,
                    name: executable_name(e.pid).unwrap_or_else(|| e.name.clone()),
                    memory: read_process_memory(&e.pid.to_string())?,
                })
            })
            .collect();
        MemorySnapshot {
            process: read_process_memory("self"),
            heap: heap_stats(),
            children,
        }
    }

    /// Resident memory of this process plus all of its descendants, in KiB.
    fn total_rss_kib(&self) -> u64 {
        self.process.map_or(0, |p| p.rss_kib)
            + self.children.iter().map(|c| c.memory.rss_kib).sum::<u64>()
    }
}

/// An amount of memory in KiB, displayed in MiB.
struct Kib(u64);

impl Kib {
    fn from_bytes(bytes: usize) -> Self {
        Kib(bytes as u64 / 1024)
    }
}

impl fmt::Display for Kib {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.1}MiB", self.0 as f64 / 1024.0)
    }
}

impl fmt::Display for MemorySnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "total_rss={}", Kib(self.total_rss_kib()))?;
        match self.process {
            Some(p) => write!(
                f,
                " self_rss={} self_peak_rss={} self_anon={} self_file={} self_shmem={} self_swap={}",
                Kib(p.rss_kib),
                Kib(p.peak_rss_kib),
                Kib(p.anon_kib),
                Kib(p.file_kib),
                Kib(p.shmem_kib),
                Kib(p.swap_kib)
            )?,
            None => write!(f, " self=unavailable")?,
        }
        if let Some(h) = self.heap {
            write!(
                f,
                " heap_arena={} heap_in_use={} heap_free={} heap_mmap={}",
                Kib::from_bytes(h.arena_bytes),
                Kib::from_bytes(h.in_use_bytes),
                Kib::from_bytes(h.free_bytes),
                Kib::from_bytes(h.mmap_bytes)
            )?;
        }
        self.children.iter().try_for_each(|c| {
            write!(
                f,
                " child[{} {}]: rss={} anon={} swap={}",
                c.pid,
                c.name,
                Kib(c.memory.rss_kib),
                Kib(c.memory.anon_kib),
                Kib(c.memory.swap_kib)
            )
        })
    }
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

/// Logs the build and environment details that affect memory usage.
pub fn log_environment(db_path: &Path) {
    let wal_path = db_path.with_extension("db-wal");
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        pid = std::process::id(),
        db_bytes = file_size(db_path),
        db_wal_bytes = file_size(&wal_path),
        "Memory diagnostics: environment"
    );
}

/// Captures and logs a memory snapshot, tagged with what prompted it. This
/// scans `/proc` and walks the malloc arenas, so async callers should run it
/// via `spawn_blocking`.
pub fn log_snapshot(trigger: &str) {
    tracing::info!(
        trigger,
        catalog_loads = CATALOG_LOADS.load(Ordering::Relaxed),
        "Memory snapshot: {}",
        MemorySnapshot::capture()
    );
}

/// Counts and logs a catalog build that took `elapsed`.
pub fn record_catalog_load(stats: &CatalogStats, elapsed: Duration) {
    CATALOG_LOADS.fetch_add(1, Ordering::Relaxed);
    tracing::info!(
        elapsed_ms = elapsed.as_millis() as u64,
        "Catalog loaded: {stats}"
    );
}

/// Logs a snapshot (plus catalog and file-issue counts) every
/// [`SNAPSHOT_INTERVAL`] for the lifetime of the app.
pub fn spawn_periodic_logging(catalog: Arc<RwLock<Catalog>>, file_issues: FileIssueLog) {
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval_at(
            tokio::time::Instant::now() + SNAPSHOT_INTERVAL,
            SNAPSHOT_INTERVAL,
        );
        // After a suspend, log one snapshot rather than a burst of missed ones.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let stats = catalog.read().await.stats();
            tracing::info!(runtime_file_issues = file_issues.len(), "Catalog: {stats}");
            let _ = tokio::task::spawn_blocking(|| log_snapshot("periodic")).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = "Name:\tthmp5\nVmPeak:\t 5000000 kB\nVmHWM:\t  2000000 kB\n\
        VmRSS:\t  1300000 kB\nRssAnon:\t  1000000 kB\nRssFile:\t   250000 kB\n\
        RssShmem:\t    50000 kB\nVmSwap:\t       12 kB\nThreads:\t30\n";

    #[test]
    fn parse_status_reads_memory_fields() {
        assert_eq!(
            parse_status(STATUS),
            Some(ProcessMemory {
                rss_kib: 1_300_000,
                peak_rss_kib: 2_000_000,
                anon_kib: 1_000_000,
                file_kib: 250_000,
                shmem_kib: 50_000,
                swap_kib: 12,
            })
        );
    }

    #[test]
    fn parse_status_rejects_process_without_memory_fields() {
        assert_eq!(
            parse_status("Name:\tkthreadd\nState:\tS (sleeping)\n"),
            None
        );
    }

    #[test]
    fn parse_stat_handles_names_with_spaces_and_parens() {
        assert_eq!(
            parse_stat("4242 (Web Content (x)) S 4000 4242 4242 0 -1"),
            Some(ProcEntry {
                pid: 4242,
                ppid: 4000,
                name: "Web Content (x)".to_string(),
            })
        );
    }

    #[test]
    fn descendants_includes_grandchildren_but_not_unrelated_processes() {
        let entry = |pid, ppid| ProcEntry {
            pid,
            ppid,
            name: format!("p{pid}"),
        };
        let table = vec![
            entry(1, 0),
            entry(10, 1),
            entry(11, 10),
            entry(12, 11),
            entry(20, 1),
        ];
        let pids: Vec<u32> = descendants(10, &table).iter().map(|e| e.pid).collect();
        assert_eq!(pids, vec![11, 12]);
    }

    #[test]
    fn total_rss_sums_self_and_children() {
        let memory = |rss_kib| ProcessMemory {
            rss_kib,
            peak_rss_kib: rss_kib,
            anon_kib: 0,
            file_kib: 0,
            shmem_kib: 0,
            swap_kib: 0,
        };
        let snapshot = MemorySnapshot {
            process: Some(memory(1000)),
            heap: None,
            children: vec![ChildProcess {
                pid: 2,
                name: "WebKitWebProcess".to_string(),
                memory: memory(500),
            }],
        };
        assert_eq!(snapshot.total_rss_kib(), 1500);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn capture_reports_own_process_memory() {
        let snapshot = MemorySnapshot::capture();
        assert!(snapshot.process.is_some_and(|p| p.rss_kib > 0));
    }
}
