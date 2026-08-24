//! Connection pool for NovaDB.
//!
//! Provides `NovaDbPool` — a thread-safe, multi-reader, single-writer connection
//! pool backed by SQLite WAL mode. Clients acquire connections via `acquire()` and
//! return them automatically when the guard is dropped.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────┐
//! │            NovaDbPool                    │
//! ├─────────────────────────────────────────┤
//! │  Writer: Mutex<NovaDb>  (1 exclusive)   │
//! │  Readers: Vec<NovaDb>   (N parallel)    │
//! │  Reader semaphore                       │
//! └─────────────────────────────────────────┘
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;

use crate::{NovaDb, QueryResult, Result};

/// Default number of read connections in the pool.
pub const DEFAULT_POOL_SIZE: usize = 4;

/// Maximum pool size.
pub const MAX_POOL_SIZE: usize = 64;

/// Configuration for a connection pool.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Path to the database file.
    pub path: PathBuf,
    /// Number of read connections (default: 4).
    pub read_pool_size: usize,
}

impl PoolConfig {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            read_pool_size: DEFAULT_POOL_SIZE,
        }
    }

    pub fn with_read_pool_size(mut self, size: usize) -> Self {
        self.read_pool_size = size.clamp(1, MAX_POOL_SIZE);
        self
    }
}

/// A multi-client connection pool over a single database file.
///
/// Uses SQLite WAL mode to allow one writer and multiple parallel readers.
/// All connections share the same database file and see a consistent view
/// of data once transactions commit.
pub struct NovaDbPool {
    inner: Arc<PoolInner>,
}

struct PoolInner {
    /// The single writer connection (serialized via Mutex).
    writer: Mutex<NovaDb>,
    /// Pool of read-only connections.
    readers: Vec<Mutex<NovaDb>>,
    /// Round-robin index for reader selection.
    reader_index: AtomicUsize,
    /// Original database path.
    path: PathBuf,
    /// Pool configuration.
    read_pool_size: usize,
}

impl NovaDbPool {
    /// Open a connection pool for the database at the given path.
    pub fn open(config: PoolConfig) -> Result<Self> {
        let writer = NovaDb::open(&config.path)?;
        let mut readers = Vec::with_capacity(config.read_pool_size);
        for _ in 0..config.read_pool_size {
            readers.push(Mutex::new(NovaDb::open(&config.path)?));
        }

        Ok(Self {
            inner: Arc::new(PoolInner {
                writer: Mutex::new(writer),
                readers,
                reader_index: AtomicUsize::new(0),
                path: config.path,
                read_pool_size: config.read_pool_size,
            }),
        })
    }

    /// Open a pool with default settings.
    pub fn open_default(path: impl AsRef<Path>) -> Result<Self> {
        Self::open(PoolConfig::new(path.as_ref()))
    }

    /// Execute a read-only query using a reader from the pool.
    ///
    /// Readers are selected round-robin to distribute load.
    pub fn query(&self, sql: &str) -> Result<QueryResult> {
        let idx = self.inner.reader_index.fetch_add(1, Ordering::Relaxed)
            % self.inner.read_pool_size;
        let reader = self.inner.readers[idx].lock();
        reader.query(sql)
    }

    /// Execute a write batch using the exclusive writer connection.
    ///
    /// Only one write can execute at a time. Other writers will wait
    /// for the mutex.
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        let writer = self.inner.writer.lock();
        writer.execute_batch(sql)
    }

    /// Access the writer directly (for sync operations, etc.).
    pub fn writer(&self) -> parking_lot::MutexGuard<'_, NovaDb> {
        self.inner.writer.lock()
    }

    /// Get the device ID from the writer connection.
    pub fn device_id(&self) -> String {
        self.inner.writer.lock().device_id().to_owned()
    }

    /// Get the database file path.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Get the number of reader connections.
    pub fn read_pool_size(&self) -> usize {
        self.inner.read_pool_size
    }

    /// Run migrations using the writer connection.
    pub fn run_migrations(&self, migrations: &[crate::Migration]) -> Result<crate::MigrationReport> {
        let writer = self.inner.writer.lock();
        writer.run_migrations(migrations)
    }

    /// Enable sync on a table using the writer connection.
    pub fn enable_sync(&self, table: &str, primary_key: &str) -> Result<()> {
        let writer = self.inner.writer.lock();
        writer.enable_sync(table, primary_key)
    }

    /// Get changes after a cursor using the writer connection.
    pub fn changes_after(
        &self,
        after: i64,
        limit: usize,
    ) -> Result<Vec<crate::Change>> {
        let writer = self.inner.writer.lock();
        writer.changes_after(after, limit)
    }
}

impl Clone for NovaDbPool {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn pool_opens_and_queries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pool_test.db");
        let pool = NovaDbPool::open(PoolConfig::new(&path).with_read_pool_size(2)).unwrap();

        pool.execute_batch(
            "CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT);
             INSERT INTO items VALUES (1, 'hello');",
        )
        .unwrap();

        let result = pool.query("SELECT name FROM items").unwrap();
        assert_eq!(result.rows.len(), 1);
    }

    #[test]
    fn pool_default_size_is_4() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("default_pool.db");
        let pool = NovaDbPool::open_default(&path).unwrap();
        assert_eq!(pool.read_pool_size(), DEFAULT_POOL_SIZE);
    }

    #[test]
    fn pool_serves_concurrent_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("concurrent.db");
        let pool = NovaDbPool::open(PoolConfig::new(&path).with_read_pool_size(4)).unwrap();

        pool.execute_batch(
            "CREATE TABLE data(id INTEGER PRIMARY KEY, value TEXT);
             INSERT INTO data VALUES (1, 'a');
             INSERT INTO data VALUES (2, 'b');",
        )
        .unwrap();

        let pool = Arc::new(pool);
        let mut handles = Vec::new();

        for _ in 0..8 {
            let pool = Arc::clone(&pool);
            handles.push(thread::spawn(move || {
                let result = pool.query("SELECT COUNT(*) as cnt FROM data").unwrap();
                assert_eq!(result.rows.len(), 1);
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn pool_write_is_serialized() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serial_write.db");
        let pool = Arc::new(
            NovaDbPool::open(PoolConfig::new(&path).with_read_pool_size(2)).unwrap(),
        );

        pool.execute_batch(
            "CREATE TABLE counter(id INTEGER PRIMARY KEY, n INTEGER DEFAULT 0);
             INSERT INTO counter VALUES (1, 0);",
        )
        .unwrap();

        let mut handles = Vec::new();
        for _ in 0..10 {
            let pool = Arc::clone(&pool);
            handles.push(thread::spawn(move || {
                pool.execute_batch("UPDATE counter SET n = n + 1 WHERE id = 1")
                    .unwrap();
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let result = pool.query("SELECT n FROM counter WHERE id = 1").unwrap();
        assert_eq!(result.rows[0]["n"], 10);
    }

    #[test]
    fn pool_migrations_and_sync() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("migrate_pool.db");
        let pool = NovaDbPool::open_default(&path).unwrap();

        let report = pool
            .run_migrations(&[crate::Migration::new(
                1,
                "create notes",
                "CREATE TABLE notes(id TEXT COLLATE BINARY PRIMARY KEY, title TEXT);",
            )])
            .unwrap();
        assert_eq!(report.applied_versions, vec![1]);

        pool.enable_sync("notes", "id").unwrap();
        pool.execute_batch("INSERT INTO notes VALUES ('n1', 'hello')")
            .unwrap();

        let changes = pool.changes_after(0, 100).unwrap();
        assert!(!changes.is_empty());
    }

    #[test]
    fn pool_clone_shares_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clone_pool.db");
        let pool1 = NovaDbPool::open_default(&path).unwrap();
        let pool2 = pool1.clone();

        pool1
            .execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY)")
            .unwrap();

        // pool2 should see the table via its reader
        let result = pool2
            .query("SELECT COUNT(*) as cnt FROM t")
            .unwrap();
        assert_eq!(result.rows.len(), 1);
    }
}
