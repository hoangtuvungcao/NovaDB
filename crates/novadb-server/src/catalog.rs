use std::{
    collections::HashMap,
    ffi::OsStr,
    fs, io,
    ops::Deref,
    path::{Path, PathBuf},
    sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard},
    time::{SystemTime, UNIX_EPOCH},
};

use novadb_core::{NovaDbPool, PoolConfig};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::{DatabaseIdError, validate_database_id};

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error(transparent)]
    InvalidDatabaseId(#[from] DatabaseIdError),
    #[error("database `{0}` does not exist")]
    NotFound(String),
    #[error("database path for `{0}` must be a regular file and cannot be a symbolic link")]
    UnsafeFile(String),
    #[error("backup directory must be a real directory and cannot be a symbolic link")]
    UnsafeBackupDirectory,
    #[error("database catalog I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("database error: {0}")]
    Database(#[from] novadb_core::Error),
    #[error("database catalog lock was poisoned")]
    LockPoisoned,
}

/// Filesystem metadata for one managed database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseMetadata {
    pub id: String,
    pub size_bytes: u64,
    pub modified_at_ms: Option<u64>,
    pub open: bool,
    pub device_id: Option<String>,
}

/// Metadata for a server-owned, no-clobber online backup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupReport {
    /// Path relative to the catalog data directory.
    pub backup_id: String,
    pub size_bytes: u64,
}

/// Default number of parallel read connections per database: one per CPU.
#[must_use]
pub fn default_read_pool_size() -> usize {
    std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get)
}

/// One open database: a writer plus lazily-opened `query_only` readers
/// ([`NovaDbPool`]), and an async permit pool that keeps the number of
/// blocking threads running reads at the reader count (excess requests wait
/// asynchronously instead of occupying blocking-pool threads).
pub struct ManagedDatabase {
    pool: NovaDbPool,
    reads: Arc<Semaphore>,
}

impl ManagedDatabase {
    fn new(pool: NovaDbPool) -> Self {
        let permits = pool.read_pool_size();
        Self {
            pool,
            reads: Arc::new(Semaphore::new(permits)),
        }
    }

    /// Permits bounding concurrent blocking reads on this database.
    #[must_use]
    pub fn read_permits(&self) -> &Arc<Semaphore> {
        &self.reads
    }

    /// The underlying pool (also reachable through `Deref`).
    #[must_use]
    pub fn pool(&self) -> &NovaDbPool {
        &self.pool
    }
}

impl Deref for ManagedDatabase {
    type Target = NovaDbPool;

    fn deref(&self) -> &NovaDbPool {
        &self.pool
    }
}

/// Maps validated database IDs to direct children of one data directory and
/// keeps at most one open [`NovaDbPool`] (writer + parallel readers) per
/// database in this process.
pub struct DatabaseCatalog {
    data_dir: PathBuf,
    read_pool_size: usize,
    databases: RwLock<HashMap<String, Arc<ManagedDatabase>>>,
}

impl std::fmt::Debug for DatabaseCatalog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DatabaseCatalog")
            .field("data_dir", &self.data_dir)
            .finish_non_exhaustive()
    }
}

impl DatabaseCatalog {
    /// Creates the data directory when missing and anchors all future paths to
    /// its canonical location.
    pub fn new(data_dir: impl AsRef<Path>) -> Result<Self, CatalogError> {
        Self::with_read_pool_size(data_dir, default_read_pool_size())
    }

    /// Like [`DatabaseCatalog::new`] with an explicit maximum number of read
    /// connections per database.
    pub fn with_read_pool_size(
        data_dir: impl AsRef<Path>,
        read_pool_size: usize,
    ) -> Result<Self, CatalogError> {
        fs::create_dir_all(data_dir.as_ref())?;
        let metadata = fs::metadata(data_dir.as_ref())?;
        if !metadata.is_dir() {
            return Err(CatalogError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "database data path is not a directory",
            )));
        }
        let data_dir = fs::canonicalize(data_dir.as_ref())?;
        Ok(Self {
            data_dir,
            read_pool_size: read_pool_size.max(1),
            databases: RwLock::new(HashMap::new()),
        })
    }

    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Lists regular `*.novadb` files whose stems are valid database IDs.
    pub fn list(&self) -> Result<Vec<DatabaseMetadata>, CatalogError> {
        let open_databases: HashMap<String, String> = self
            .read()?
            .iter()
            .map(|(id, database)| (id.clone(), database.device_id()))
            .collect();
        let mut databases = Vec::new();

        for entry in fs::read_dir(&self.data_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_symlink() || !entry.file_type()?.is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension() != Some(OsStr::new("novadb")) {
                continue;
            }
            let Some(id) = path.file_stem().and_then(OsStr::to_str) else {
                continue;
            };
            if validate_database_id(id).is_err() {
                continue;
            }
            databases.push(metadata_for(
                id,
                &path,
                open_databases.get(id).map(String::as_str),
            )?);
        }

        databases.sort_unstable_by(|left, right| left.id.cmp(&right.id));
        Ok(databases)
    }

    /// Creates a missing database or returns the cached/opened existing one.
    pub fn create_or_open(&self, database_id: &str) -> Result<Arc<ManagedDatabase>, CatalogError> {
        self.open_inner(database_id, true)
    }

    /// Opens an existing database without implicitly creating it.
    pub fn open_existing(&self, database_id: &str) -> Result<Arc<ManagedDatabase>, CatalogError> {
        self.open_inner(database_id, false)
    }

    /// Returns the database when it is already open, without any file I/O
    /// (async handlers use this to skip `spawn_blocking` on the hot path).
    #[must_use]
    pub fn cached(&self, database_id: &str) -> Option<Arc<ManagedDatabase>> {
        self.databases
            .read()
            .ok()
            .and_then(|databases| databases.get(database_id).cloned())
    }

    /// Returns current metadata for a managed database.
    pub fn metadata(&self, database_id: &str) -> Result<DatabaseMetadata, CatalogError> {
        let path = self.database_path(database_id)?;
        ensure_safe_existing_file(database_id, &path)?;
        let device_id = self
            .read()?
            .get(database_id)
            .map(|database| database.device_id());
        metadata_for(database_id, &path, device_id.as_deref())
    }

    /// Creates an online backup below `<data_dir>/.backups` using a
    /// server-generated, no-clobber filename.
    pub fn backup(&self, database_id: &str) -> Result<BackupReport, CatalogError> {
        let database = self.open_existing(database_id)?;
        let backup_directory = self.ensure_backup_directory()?;
        let timestamp = unix_time_ms();
        let filename = format!("{database_id}-{timestamp}-{}.novadb", Uuid::new_v4());
        let path = backup_directory.join(&filename);
        database.writer().backup_to(&path)?;
        let size_bytes = fs::metadata(&path)?.len();
        Ok(BackupReport {
            backup_id: format!(".backups/{filename}"),
            size_bytes,
        })
    }

    fn open_inner(
        &self,
        database_id: &str,
        create: bool,
    ) -> Result<Arc<ManagedDatabase>, CatalogError> {
        let path = self.database_path(database_id)?;
        if let Some(database) = self.read()?.get(database_id) {
            return Ok(Arc::clone(database));
        }
        let mut databases = self.write()?;
        if let Some(database) = databases.get(database_id) {
            return Ok(Arc::clone(database));
        }

        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(CatalogError::UnsafeFile(database_id.to_owned()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound && create => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(CatalogError::NotFound(database_id.to_owned()));
            }
            Err(error) => return Err(error.into()),
        }

        // Keep the catalog lock while opening so concurrent first access cannot
        // create two independent `NovaDb` handles for the same file.
        let pool =
            NovaDbPool::open(PoolConfig::new(path).with_read_pool_size(self.read_pool_size))?;
        let database = Arc::new(ManagedDatabase::new(pool));
        databases.insert(database_id.to_owned(), Arc::clone(&database));
        Ok(database)
    }

    fn database_path(&self, database_id: &str) -> Result<PathBuf, CatalogError> {
        validate_database_id(database_id)?;
        Ok(self.data_dir.join(format!("{database_id}.novadb")))
    }

    fn ensure_backup_directory(&self) -> Result<PathBuf, CatalogError> {
        let path = self.data_dir.join(".backups");
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(CatalogError::UnsafeBackupDirectory);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match fs::create_dir(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
                let metadata = fs::symlink_metadata(&path)?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(CatalogError::UnsafeBackupDirectory);
                }
            }
            Err(error) => return Err(error.into()),
        }
        Ok(path)
    }

    fn read(
        &self,
    ) -> Result<RwLockReadGuard<'_, HashMap<String, Arc<ManagedDatabase>>>, CatalogError> {
        self.databases
            .read()
            .map_err(|_| CatalogError::LockPoisoned)
    }

    fn write(
        &self,
    ) -> Result<RwLockWriteGuard<'_, HashMap<String, Arc<ManagedDatabase>>>, CatalogError> {
        self.databases
            .write()
            .map_err(|_| CatalogError::LockPoisoned)
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn ensure_safe_existing_file(database_id: &str, path: &Path) -> Result<(), CatalogError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(CatalogError::UnsafeFile(database_id.to_owned()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err(CatalogError::NotFound(database_id.to_owned()))
        }
        Err(error) => Err(error.into()),
    }
}

fn metadata_for(
    database_id: &str,
    path: &Path,
    device_id: Option<&str>,
) -> Result<DatabaseMetadata, CatalogError> {
    let metadata = fs::metadata(path)?;
    let modified_at_ms = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));
    Ok(DatabaseMetadata {
        id: database_id.to_owned(),
        size_bytes: metadata.len(),
        modified_at_ms,
        open: device_id.is_some(),
        device_id: device_id.map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn rejects_path_traversal_and_absolute_ids() {
        let directory = tempdir().unwrap();
        let catalog = DatabaseCatalog::new(directory.path().join("data")).unwrap();

        for invalid in ["../escape", "..", "/tmp/escape", "nested/name", ".hidden"] {
            assert!(matches!(
                catalog.create_or_open(invalid),
                Err(CatalogError::InvalidDatabaseId(_))
            ));
        }
        assert!(!directory.path().join("escape.novadb").exists());
    }

    #[test]
    fn creates_caches_and_lists_databases() {
        let directory = tempdir().unwrap();
        let catalog = DatabaseCatalog::new(directory.path().join("data")).unwrap();
        let database = catalog.create_or_open("alpha").unwrap();
        database
            .execute_batch("CREATE TABLE notes(id TEXT PRIMARY KEY, title TEXT);")
            .unwrap();
        assert_eq!(
            database.device_id(),
            catalog.create_or_open("alpha").unwrap().device_id()
        );

        let listed = catalog.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "alpha");
        assert!(listed[0].open);
        assert!(listed[0].device_id.is_some());
        assert!(listed[0].size_bytes > 0);
    }

    #[test]
    fn open_existing_fails_for_missing_database() {
        let directory = tempdir().unwrap();
        let catalog = DatabaseCatalog::new(directory.path().join("data")).unwrap();

        assert!(matches!(
            catalog.open_existing("nonexistent"),
            Err(CatalogError::NotFound(_))
        ));
    }

    #[test]
    fn metadata_returns_correct_info() {
        let directory = tempdir().unwrap();
        let catalog = DatabaseCatalog::new(directory.path().join("data")).unwrap();
        let db = catalog.create_or_open("meta-test").unwrap();
        db.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY);")
            .unwrap();

        let meta = catalog.metadata("meta-test").unwrap();
        assert_eq!(meta.id, "meta-test");
        assert!(meta.size_bytes > 0);
        assert!(meta.modified_at_ms.is_some());
        assert!(meta.open);
        assert!(meta.device_id.is_some());
    }

    #[test]
    fn backup_creates_a_new_file() {
        let directory = tempdir().unwrap();
        let catalog = DatabaseCatalog::new(directory.path().join("data")).unwrap();
        let db = catalog.create_or_open("backup-test").unwrap();
        db.execute_batch(
            "CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT);
             INSERT INTO items VALUES (1, 'hello');",
        )
        .unwrap();

        let report = catalog.backup("backup-test").unwrap();
        assert!(report.backup_id.starts_with(".backups/backup-test-"));
        assert!(report.backup_id.ends_with(".novadb"));
        assert!(report.size_bytes > 0);

        // Verify the backup file exists
        let backup_path = directory.path().join("data").join(&report.backup_id);
        assert!(backup_path.exists());
    }

    #[test]
    fn multiple_databases_are_listed_sorted() {
        let directory = tempdir().unwrap();
        let catalog = DatabaseCatalog::new(directory.path().join("data")).unwrap();

        // Create in non-alphabetical order
        catalog.create_or_open("charlie").unwrap();
        catalog.create_or_open("alpha").unwrap();
        catalog.create_or_open("bravo").unwrap();

        let listed = catalog.list().unwrap();
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0].id, "alpha");
        assert_eq!(listed[1].id, "bravo");
        assert_eq!(listed[2].id, "charlie");
    }

    #[test]
    fn data_dir_is_created_when_missing() {
        let directory = tempdir().unwrap();
        let deep_path = directory.path().join("a").join("b").join("data");
        assert!(!deep_path.exists());

        let catalog = DatabaseCatalog::new(&deep_path).unwrap();
        assert!(deep_path.exists());
        assert_eq!(catalog.data_dir(), deep_path.canonicalize().unwrap());
    }

    #[test]
    fn empty_catalog_lists_nothing() {
        let directory = tempdir().unwrap();
        let catalog = DatabaseCatalog::new(directory.path().join("data")).unwrap();
        let listed = catalog.list().unwrap();
        assert!(listed.is_empty());
    }
}
