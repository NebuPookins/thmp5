use crate::models::{DbPoolDebugSnapshot, DbPoolLeaseInfo, DbPoolWaiterInfo};
use anyhow::Result;
use sqlx::{
    pool::PoolConnection,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    Sqlite, SqliteConnection, SqlitePool,
};
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct DbPool {
    pool: SqlitePool,
    tracker: Arc<PoolTracker>,
}

pub struct TrackedConnection {
    conn: PoolConnection<Sqlite>,
    tracker: Arc<PoolTracker>,
    lease_id: u64,
}

struct PoolTracker {
    next_id: AtomicU64,
    active: Mutex<HashMap<u64, LeaseEntry>>,
    waiting: Mutex<HashMap<u64, WaiterEntry>>,
}

#[derive(Clone)]
struct LeaseEntry {
    purpose: String,
    acquired_at: Instant,
    acquired_at_unix_ms: i64,
}

#[derive(Clone)]
struct WaiterEntry {
    purpose: String,
    requested_at: Instant,
    requested_at_unix_ms: i64,
}

/// Number of concurrent workers (and DB pool connections) to use.
/// Computed once as `max(1, min(cpu_count - 1, 6))`.
pub fn worker_count() -> u32 {
    std::thread::available_parallelism()
        .map(|n| (n.get() as u32).saturating_sub(1).clamp(1, 6))
        .unwrap_or(4)
}

fn pool_connection_count() -> u32 {
    // The importer can keep several connections busy while the UI also fires
    // bursts of list/detail queries together. Keep a modest reserve so those
    // short-lived UI commands do not starve behind import work.
    (worker_count() + 6).clamp(8, 16)
}

pub async fn init_pool(db_path: &Path) -> Result<DbPool> {
    let opts = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);

    // Reserve enough connections for import workers plus concurrent UI reads.
    let pool = SqlitePoolOptions::new()
        .max_connections(pool_connection_count())
        .acquire_timeout(Duration::from_secs(30))
        .connect_with(opts)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;

    Ok(DbPool {
        pool,
        tracker: Arc::new(PoolTracker::default()),
    })
}

impl DbPool {
    pub fn raw_pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn snapshot(&self) -> DbPoolDebugSnapshot {
        self.tracker
            .snapshot(self.pool.size(), self.pool.num_idle())
    }

    pub async fn acquire(&self, purpose: impl Into<String>) -> Result<TrackedConnection> {
        let purpose = purpose.into();
        let waiter_id = self.tracker.register_waiter(purpose.clone());
        let start = Instant::now();

        match self.pool.acquire().await {
            Ok(conn) => {
                let waited_ms = start.elapsed().as_millis();
                let lease_id = self.tracker.promote_waiter(waiter_id);
                tracing::debug!(purpose = %purpose, waited_ms, lease_id, "Acquired DB connection");
                Ok(TrackedConnection {
                    conn,
                    tracker: Arc::clone(&self.tracker),
                    lease_id,
                })
            }
            Err(error) => {
                self.tracker.remove_waiter(waiter_id);
                let snapshot = self.snapshot();
                tracing::error!(
                    purpose = %purpose,
                    waited_ms = start.elapsed().as_millis(),
                    size = snapshot.size,
                    idle = snapshot.idle,
                    active_connection_count = snapshot.active_connection_count,
                    waiting_request_count = snapshot.waiting_request_count,
                    active_connections = ?snapshot.active_connections,
                    waiting_requests = ?snapshot.waiting_requests,
                    "Failed to acquire DB connection"
                );
                Err(error.into())
            }
        }
    }
}

impl TrackedConnection {
    /// Sets `PRAGMA busy_timeout` on this connection so that write transactions
    /// wait for the lock rather than immediately returning SQLITE_BUSY.
    /// Call this before starting any write transaction that may contend with others.
    pub async fn set_busy_timeout(&mut self, timeout: Duration) -> Result<()> {
        let ms = timeout.as_millis() as u64;
        sqlx::query(&format!("PRAGMA busy_timeout = {ms}"))
            .execute(&mut *self.conn)
            .await?;
        Ok(())
    }
}

impl Deref for TrackedConnection {
    type Target = SqliteConnection;

    fn deref(&self) -> &Self::Target {
        &self.conn
    }
}

impl DerefMut for TrackedConnection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.conn
    }
}

impl Drop for TrackedConnection {
    fn drop(&mut self) {
        self.tracker.release(self.lease_id);
    }
}

impl Default for PoolTracker {
    fn default() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            active: Mutex::new(HashMap::new()),
            waiting: Mutex::new(HashMap::new()),
        }
    }
}

impl PoolTracker {
    fn register_waiter(&self, purpose: String) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.waiting
            .lock()
            .expect("pool waiters mutex poisoned")
            .insert(
                id,
                WaiterEntry {
                    purpose,
                    requested_at: Instant::now(),
                    requested_at_unix_ms: unix_time_ms(),
                },
            );
        id
    }

    fn promote_waiter(&self, id: u64) -> u64 {
        let waiter = self
            .waiting
            .lock()
            .expect("pool waiters mutex poisoned")
            .remove(&id)
            .expect("waiter missing during DB connection promotion");

        self.active
            .lock()
            .expect("pool active mutex poisoned")
            .insert(
                id,
                LeaseEntry {
                    purpose: waiter.purpose,
                    acquired_at: Instant::now(),
                    acquired_at_unix_ms: unix_time_ms(),
                },
            );
        id
    }

    fn remove_waiter(&self, id: u64) {
        self.waiting
            .lock()
            .expect("pool waiters mutex poisoned")
            .remove(&id);
    }

    fn release(&self, id: u64) {
        self.active
            .lock()
            .expect("pool active mutex poisoned")
            .remove(&id);
    }

    fn snapshot(&self, size: u32, idle: usize) -> DbPoolDebugSnapshot {
        let now = Instant::now();

        let mut active_connections: Vec<_> = self
            .active
            .lock()
            .expect("pool active mutex poisoned")
            .iter()
            .map(|(id, entry)| DbPoolLeaseInfo {
                id: *id,
                purpose: entry.purpose.clone(),
                acquired_at_unix_ms: entry.acquired_at_unix_ms,
                held_for_ms: now.duration_since(entry.acquired_at).as_millis(),
            })
            .collect();
        active_connections.sort_by(|a, b| b.held_for_ms.cmp(&a.held_for_ms));

        let mut waiting_requests: Vec<_> = self
            .waiting
            .lock()
            .expect("pool waiters mutex poisoned")
            .iter()
            .map(|(id, entry)| DbPoolWaiterInfo {
                id: *id,
                purpose: entry.purpose.clone(),
                requested_at_unix_ms: entry.requested_at_unix_ms,
                waiting_for_ms: now.duration_since(entry.requested_at).as_millis(),
            })
            .collect();
        waiting_requests.sort_by(|a, b| b.waiting_for_ms.cmp(&a.waiting_for_ms));

        DbPoolDebugSnapshot {
            size,
            idle,
            active_connection_count: active_connections.len(),
            waiting_request_count: waiting_requests.len(),
            active_connections,
            waiting_requests,
        }
    }
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}
