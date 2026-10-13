//! Connection pool for NovaDB.
//!
//! Provides `NovaDbPool` - a thread-safe, multi-reader, single-writer connection
//! pool backed by SQLite WAL mode. Reads run in parallel on their own
//! `query_only` connections; writes go through the writer handle and the
//! per-file write gate (and are group-committed under contention, see
//! [`crate::GroupCommitConfig`]).
//!
//! # Architecture
//!
//! ```text
//! +-----------------------------------------+
//! |            NovaDbPool                   |
//! +-----------------------------------------+
//! |  Writer: NovaDb (1, gate-serialised)    |
//! |  Readers: up to N query_only NovaDb,    |
//! |           opened lazily on demand       |
//! +-----------------------------------------+
//! ```
//!
//! Read-your-writes: a reader statement starts a fresh WAL snapshot, so it
//! sees every transaction committed before it started; it never sees a
//! half-applied batch.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;

use crate::{DialectMode, NovaDb, OpenOptions, QueryResult, Result, ResultLimit, TypedQueryResult};

/// Default number of read connections in the pool.
pub const DEFAULT_POOL_SIZE: usize = 4;

/// Maximum pool size.
pub const MAX_POOL_SIZE: usize = 1024;

/// Configuration for a connection pool.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Path to the database file.
    pub path: PathBuf,
    /// Maximum number of read connections (default: 4). They are opened
    /// lazily, only when every open reader is busy.
    pub read_pool_size: usize,
    /// Settings applied to the writer and every reader connection.
    pub options: OpenOptions,
}

impl PoolConfig {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            read_pool_size: DEFAULT_POOL_SIZE,
            options: OpenOptions::default(),
        }
    }

    /// Overrides the connection settings (e.g. `busy_timeout`).
    #[must_use]
    pub fn with_open_options(mut self, options: OpenOptions) -> Self {
        self.options = options;
        self
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
    /// The writer connection (writes serialise on the per-file gate).
    writer: NovaDb,
    /// Slots for read-only (`query_only`) connections, filled lazily. Each has
    /// its own `SQLite` connection, so in WAL mode readers run in parallel
    /// with each other and with the writer.
    readers: Box<[OnceLock<NovaDb>]>,
    /// Serialises opening a new reader so two threads never race for a slot.
    opening: Mutex<()>,
    /// Number of initialised reader slots (they fill from index 0 upwards).
    opened: AtomicUsize,
    /// Round-robin index for reader selection.
    reader_index: AtomicUsize,
    /// Options for reader connections.
    reader_options: OpenOptions,
    /// Original database path.
    path: PathBuf,
}

impl NovaDbPool {
    /// Opens the writer for the database at the given path; readers open on
    /// first use.
    pub fn open(config: PoolConfig) -> Result<Self> {
        let writer = NovaDb::open_with_options(&config.path, &config.options)?;
        let readers = (0..config.read_pool_size.clamp(1, MAX_POOL_SIZE))
            .map(|_| OnceLock::new())
            .collect();
        Ok(Self {
            inner: Arc::new(PoolInner {
                writer,
                readers,
                opening: Mutex::new(()),
                opened: AtomicUsize::new(0),
                reader_index: AtomicUsize::new(0),
                reader_options: config.options.clone().query_only(true),
                path: config.path,
            }),
        })
    }

    /// Open a pool with default settings.
    pub fn open_default(path: impl AsRef<Path>) -> Result<Self> {
        Self::open(PoolConfig::new(path.as_ref()))
    }

    /// Runs `operation` on an idle reader.
    ///
    /// The search starts at a rotating position and takes the first idle
    /// reader. When all open readers are busy and the pool is not yet at its
    /// limit a new reader is opened; at the limit the caller waits for the
    /// reader it started at.
    fn with_reader<T>(
        &self,
        operation: impl Fn(&NovaDb) -> Option<T>,
        blocking: impl Fn(&NovaDb) -> T,
    ) -> Result<T> {
        let inner = &self.inner;
        loop {
            let opened = inner.opened.load(Ordering::Acquire);
            if opened > 0 {
                let start = inner.reader_index.fetch_add(1, Ordering::Relaxed) % opened;
                for offset in 0..opened {
                    if let Some(reader) = inner.readers[(start + offset) % opened].get() {
                        if let Some(result) = operation(reader) {
                            return Ok(result);
                        }
                    }
                }
            }
            if opened < inner.readers.len() {
                let _opening = inner.opening.lock();
                let now = inner.opened.load(Ordering::Acquire);
                if now == opened {
                    // Still the same count: this thread opens the next reader.
                    let reader = NovaDb::open_with_options(&inner.path, &inner.reader_options)?;
                    let _ = inner.readers[now].set(reader);
                    inner.opened.store(now + 1, Ordering::Release);
                }
                continue;
            }
            // Pool is full and every reader is busy: wait on one of them.
            let start = inner.reader_index.fetch_add(1, Ordering::Relaxed) % opened.max(1);
            let reader = inner.readers[start]
                .get()
                .ok_or_else(|| crate::Error::Unsupported("reader pool is empty".into()))?;
            return Ok(blocking(reader));
        }
    }

    /// Execute a read-only query using a reader from the pool.
    pub fn query(&self, sql: &str) -> Result<QueryResult> {
        self.query_with(sql, DialectMode::TSql)
    }

    /// [`NovaDbPool::query`] with an explicit [`DialectMode`].
    pub fn query_with(&self, sql: &str, mode: DialectMode) -> Result<QueryResult> {
        self.query_typed(sql, mode)
            .map(TypedQueryResult::into_query_result)
    }

    /// Like [`NovaDbPool::query_with`], also reporting declared column types.
    pub fn query_typed(&self, sql: &str, mode: DialectMode) -> Result<TypedQueryResult> {
        self.query_limited(sql, mode, ResultLimit::UNLIMITED)
    }

    /// Like [`NovaDbPool::query_typed`] but fails with
    /// [`crate::Error::ResultTooLarge`] beyond `limit`.
    pub fn query_limited(
        &self,
        sql: &str,
        mode: DialectMode,
        limit: ResultLimit,
    ) -> Result<TypedQueryResult> {
        self.with_reader(
            |reader| reader.try_query_typed(sql, mode, limit),
            |reader| reader.query_limited(sql, mode, limit),
        )?
    }

    /// Execute a write batch on the writer (serialised per file; concurrent
    /// small writes are group-committed).
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        self.inner.writer.execute_batch(sql)
    }

    /// The writer handle (for sync operations, backups, maintenance, ...).
    pub fn writer(&self) -> &NovaDb {
        &self.inner.writer
    }

    /// `PRAGMA integrity_check` on the writer connection.
    pub fn integrity_check(&self) -> Result<crate::IntegrityReport> {
        self.inner.writer.integrity_check()
    }

    /// Truncating WAL checkpoint on the writer connection.
    pub fn wal_checkpoint(&self) -> Result<crate::WalCheckpointReport> {
        self.inner.writer.wal_checkpoint()
    }

    /// Online backup to a new file.
    pub fn backup_to(&self, path: impl AsRef<Path>) -> Result<()> {
        self.inner.writer.backup_to(path)
    }

    /// Get the device ID from the writer connection.
    pub fn device_id(&self) -> String {
        self.inner.writer.device_id().to_owned()
    }

    /// Get the database file path.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Maximum number of reader connections.
    pub fn read_pool_size(&self) -> usize {
        self.inner.readers.len()
    }

    /// Reader connections opened so far.
    pub fn open_readers(&self) -> usize {
        self.inner.opened.load(Ordering::Acquire)
    }

    /// Run migrations using the writer connection.
    pub fn run_migrations(
        &self,
        migrations: &[crate::Migration],
    ) -> Result<crate::MigrationReport> {
        self.inner.writer.run_migrations(migrations)
    }

    /// Enable sync on a table using the writer connection.
    pub fn enable_sync(&self, table: &str, primary_key: &str) -> Result<()> {
        self.inner.writer.enable_sync(table, primary_key)
    }

    /// Get changes after a cursor using the writer connection.
    pub fn changes_after(&self, after: i64, limit: usize) -> Result<Vec<crate::Change>> {
        self.inner.writer.changes_after(after, limit)
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
        let pool =
            Arc::new(NovaDbPool::open(PoolConfig::new(&path).with_read_pool_size(2)).unwrap());

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
        let result = pool2.query("SELECT COUNT(*) as cnt FROM t").unwrap();
        assert_eq!(result.rows.len(), 1);
    }
}
