//! Sharded databases: one logical database spread over N `SQLite` files.
//!
//! `SQLite` allows one writer per file, so write throughput of a single file
//! is bounded by one commit pipeline. A sharded database `game` consists of
//! the files `game.shard-0000.novadb` .. `game.shard-NNNN.novadb`, each with
//! its own [`NovaDbPool`], group-commit thread and write gate, so N shards
//! commit in parallel.
//!
//! # Routing (frozen, never changes between versions)
//!
//! `shard = jump_consistent_hash(fnv1a64(key bytes), shard_count)` where
//! `fnv1a64` is the 64-bit FNV-1a hash (offset `0xcbf29ce484222325`, prime
//! `0x100000001b3`) over the UTF-8 bytes of the key and
//! `jump_consistent_hash` is Lamping & Veach's algorithm (multiplier
//! `2862933555777941757`). The same key therefore always lands on the same
//! shard for a given shard count, and growing N moves only ~1/N of the keys.
//! Golden values are pinned in the tests (`routing_is_frozen`).
//!
//! # Application rules
//!
//! Tables must be *partitioned by the shard key* (all rows of one player in
//! the same shard). There are no cross-shard joins, transactions or foreign
//! keys; global data (config tables) is simply created in every shard by DDL
//! fan-out. Cross-shard reads ([`NovaDbSharded::query_all`]) run the same
//! statement on every shard in parallel; combine them with
//! [`merge_sorted`] (each shard's statement must itself `ORDER BY ... LIMIT`).

use std::{
    cmp::Ordering,
    collections::BinaryHeap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    DialectMode, Error, NovaDbPool, OpenOptions, OwnedParams, PoolConfig, Result, ResultLimit,
    TypedQueryResult,
};

/// Name of the routing algorithm stored in the shard manifest.
pub const SHARD_ROUTING: &str = "fnv1a64+jump-v1";
/// Most shards one logical database may have.
pub const MAX_SHARDS: usize = 256;

/// 64-bit FNV-1a over `bytes`.
#[must_use]
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Lamping & Veach jump consistent hash: maps `key` to `0..buckets`.
#[must_use]
pub fn jump_consistent_hash(mut key: u64, buckets: u32) -> u32 {
    let mut bucket: i64 = -1;
    let mut jump: i64 = 0;
    while jump < i64::from(buckets) {
        bucket = jump;
        key = key.wrapping_mul(2_862_933_555_777_941_757).wrapping_add(1);
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        {
            jump = ((bucket + 1) as f64 * (f64::from(1_u32 << 31) / (((key >> 33) + 1) as f64)))
                as i64;
        }
    }
    u32::try_from(bucket).unwrap_or(0)
}

/// Shard index (`0..shards`) for `key`.
#[must_use]
pub fn shard_for_key(key: &str, shards: usize) -> usize {
    let shards = u32::try_from(shards.clamp(1, MAX_SHARDS)).unwrap_or(1);
    jump_consistent_hash(fnv1a64(key.as_bytes()), shards) as usize
}

/// File name of shard `index` of database `name`.
#[must_use]
pub fn shard_file_name(name: &str, index: usize) -> String {
    format!("{name}.shard-{index:04}.novadb")
}

/// File name of the manifest that marks `name` as sharded.
#[must_use]
pub fn manifest_file_name(name: &str) -> String {
    format!("{name}.shards.json")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Manifest {
    shards: usize,
    routing: String,
}

/// Reads the shard count of `name` from its manifest in `dir` (`None` when the
/// database is not sharded).
pub fn read_shard_manifest(dir: &Path, name: &str) -> Result<Option<usize>> {
    let path = dir.join(manifest_file_name(name));
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let manifest: Manifest = serde_json::from_str(&text)?;
    if manifest.routing != SHARD_ROUTING {
        return Err(Error::Unsupported(format!(
            "shard manifest of `{name}` uses routing `{}`, this build implements `{SHARD_ROUTING}`",
            manifest.routing
        )));
    }
    if manifest.shards == 0 || manifest.shards > MAX_SHARDS {
        return Err(Error::Unsupported(format!(
            "shard manifest of `{name}` has an invalid shard count"
        )));
    }
    Ok(Some(manifest.shards))
}

fn write_shard_manifest(dir: &Path, name: &str, shards: usize) -> Result<()> {
    let manifest = Manifest {
        shards,
        routing: SHARD_ROUTING.to_owned(),
    };
    let tmp = dir.join(format!("{name}.shards.json.tmp-{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&manifest)?)?;
    std::fs::rename(&tmp, dir.join(manifest_file_name(name)))?;
    Ok(())
}

/// A logical database made of N independent shard files.
#[derive(Clone)]
pub struct NovaDbSharded {
    name: String,
    dir: PathBuf,
    shards: std::sync::Arc<Vec<NovaDbPool>>,
}

impl NovaDbSharded {
    /// Opens (creating on first use) database `name` with `shards` shards in
    /// `dir`. Re-opening with a different shard count fails: resharding is not
    /// supported.
    pub fn open(dir: impl AsRef<Path>, name: &str, shards: usize) -> Result<Self> {
        Self::open_with(
            dir,
            name,
            shards,
            crate::pool::DEFAULT_POOL_SIZE,
            &OpenOptions::new(),
        )
    }

    /// [`NovaDbSharded::open`] with an explicit read pool size per shard and
    /// connection options.
    pub fn open_with(
        dir: impl AsRef<Path>,
        name: &str,
        shards: usize,
        read_pool_size: usize,
        options: &OpenOptions,
    ) -> Result<Self> {
        crate::validate_identifier(&name.replace('-', "_"))?;
        if shards == 0 || shards > MAX_SHARDS {
            return Err(Error::Unsupported(format!(
                "shard count must be between 1 and {MAX_SHARDS}"
            )));
        }
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)?;
        match read_shard_manifest(dir, name)? {
            Some(existing) if existing != shards => {
                return Err(Error::Unsupported(format!(
                    "database `{name}` already has {existing} shards; resharding to {shards} is not supported"
                )));
            }
            Some(_) => {}
            None => {
                if dir.join(format!("{name}.novadb")).exists() {
                    return Err(Error::Unsupported(format!(
                        "database `{name}` already exists as an unsharded database"
                    )));
                }
            }
        }
        let pools = (0..shards)
            .map(|index| {
                NovaDbPool::open(
                    PoolConfig::new(dir.join(shard_file_name(name, index)))
                        .with_read_pool_size(read_pool_size)
                        .with_open_options(options.clone()),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        // The manifest is written last: a crash while creating shard files
        // leaves no manifest, and the next `open` simply finishes the job.
        write_shard_manifest(dir, name, shards)?;
        Ok(Self {
            name: name.to_owned(),
            dir: dir.to_path_buf(),
            shards: std::sync::Arc::new(pools),
        })
    }

    /// Opens an existing sharded database, reading the shard count from its
    /// manifest. Fails when `name` is not sharded.
    pub fn open_existing(dir: impl AsRef<Path>, name: &str) -> Result<Self> {
        let dir = dir.as_ref();
        let shards = read_shard_manifest(dir, name)?
            .ok_or_else(|| Error::TableNotFound(format!("sharded database `{name}`")))?;
        Self::open(dir, name, shards)
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.dir
    }

    #[must_use]
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// The pool of shard `index`.
    #[must_use]
    pub fn shard(&self, index: usize) -> Option<&NovaDbPool> {
        self.shards.get(index)
    }

    /// All shard pools in index order.
    #[must_use]
    pub fn pools(&self) -> &[NovaDbPool] {
        &self.shards
    }

    /// Shard index for `key` (see the module docs for the frozen algorithm).
    #[must_use]
    pub fn shard_index(&self, key: &str) -> usize {
        shard_for_key(key, self.shards.len())
    }

    /// The pool that owns `key`.
    #[must_use]
    pub fn route(&self, key: &str) -> &NovaDbPool {
        &self.shards[self.shard_index(key)]
    }

    /// Runs `sql` (a DDL script or any statement) on every shard, one shard
    /// after the other, each atomically. Failures do not stop the remaining
    /// shards; the per-shard outcomes let the caller repair stragglers by
    /// re-applying idempotent DDL (`IF NOT EXISTS`).
    pub fn execute_all(&self, sql: &str, mode: DialectMode) -> Vec<Result<()>> {
        self.shards
            .iter()
            .map(|pool| pool.writer().execute_batch_with(sql, mode))
            .collect()
    }

    /// Runs the same read on every shard in parallel.
    pub fn query_all(
        &self,
        sql: &str,
        params: Option<&OwnedParams>,
        mode: DialectMode,
        limit: ResultLimit,
    ) -> Vec<Result<TypedQueryResult>> {
        crate::metrics::global().shard_fanouts.inc();
        std::thread::scope(|scope| {
            let handles: Vec<_> = self
                .shards
                .iter()
                .map(|pool| {
                    scope.spawn(move || match params {
                        Some(params) => pool.query_limited_params(sql, params, mode, limit),
                        None => pool.query_limited(sql, mode, limit),
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .unwrap_or_else(|_| Err(Error::Unsupported("shard query panicked".into())))
                })
                .collect()
        })
    }
}

/// One `ORDER BY` key of a cross-shard merge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortKey {
    pub column: String,
    #[serde(default)]
    pub desc: bool,
}

/// SQLite-like ordering of result values: NULL < numbers < text < everything
/// else; numbers compare numerically, text bytewise (the BINARY collation).
#[must_use]
pub fn compare_values(left: &Value, right: &Value) -> Ordering {
    fn rank(value: &Value) -> u8 {
        match value {
            Value::Null => 0,
            Value::Number(_) | Value::Bool(_) => 1,
            Value::String(_) => 2,
            _ => 3,
        }
    }
    fn number(value: &Value) -> f64 {
        match value {
            Value::Number(n) => n.as_f64().unwrap_or(0.0),
            Value::Bool(true) => 1.0,
            _ => 0.0,
        }
    }
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => match (a.as_i64(), b.as_i64()) {
            (Some(a), Some(b)) => a.cmp(&b),
            _ => number(left).total_cmp(&number(right)),
        },
        (Value::String(a), Value::String(b)) => a.as_bytes().cmp(b.as_bytes()),
        _ => {
            let ranks = rank(left).cmp(&rank(right));
            if ranks == Ordering::Equal && rank(left) == 1 {
                number(left).total_cmp(&number(right))
            } else {
                ranks
            }
        }
    }
}

fn compare_rows(left: &Value, right: &Value, keys: &[SortKey]) -> Ordering {
    for key in keys {
        let a = left.get(&key.column).unwrap_or(&Value::Null);
        let b = right.get(&key.column).unwrap_or(&Value::Null);
        let ordering = compare_values(a, b);
        let ordering = if key.desc {
            ordering.reverse()
        } else {
            ordering
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

struct HeapEntry<'a> {
    row: &'a Value,
    shard: usize,
    position: usize,
    keys: &'a [SortKey],
}

impl PartialEq for HeapEntry<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for HeapEntry<'_> {}
impl PartialOrd for HeapEntry<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapEntry<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap: reverse so the smallest row pops first;
        // ties go to the lower shard index, then the earlier row.
        compare_rows(self.row, other.row, self.keys)
            .then(self.shard.cmp(&other.shard))
            .then(self.position.cmp(&other.position))
            .reverse()
    }
}

/// K-way merge of per-shard row lists that are each already sorted by `keys`
/// (every shard's statement must itself `ORDER BY` the same columns). Returns
/// `(shard, row)` pairs in global order, cut to `limit`. Ties are broken by
/// shard index, so the result is deterministic.
#[must_use]
pub fn merge_sorted(
    rows_by_shard: &[Vec<Value>],
    keys: &[SortKey],
    limit: Option<usize>,
) -> Vec<(usize, Value)> {
    let mut heap = BinaryHeap::new();
    for (shard, rows) in rows_by_shard.iter().enumerate() {
        if let Some(row) = rows.first() {
            heap.push(HeapEntry {
                row,
                shard,
                position: 0,
                keys,
            });
        }
    }
    let total: usize = rows_by_shard.iter().map(Vec::len).sum();
    let mut merged = Vec::with_capacity(limit.map_or(total, |limit| limit.min(total)));
    while let Some(entry) = heap.pop() {
        if limit.is_some_and(|limit| merged.len() >= limit) {
            break;
        }
        merged.push((entry.shard, entry.row.clone()));
        let next = entry.position + 1;
        if let Some(row) = rows_by_shard[entry.shard].get(next) {
            heap.push(HeapEntry {
                row,
                shard: entry.shard,
                position: next,
                keys,
            });
        }
    }
    merged
}

/// Concatenates per-shard results in shard order, adding a `shard` column to
/// every row (an existing `shard` column is overwritten). With `merge` the rows
/// are k-way merged by `keys` and cut to `limit` instead.
pub fn combine_shard_results(
    results: Vec<TypedQueryResult>,
    merge: Option<(&[SortKey], Option<usize>)>,
) -> TypedQueryResult {
    let mut columns: Vec<String> = Vec::new();
    for result in &results {
        if columns.is_empty() {
            columns.clone_from(&result.columns);
        }
    }
    if !columns.iter().any(|column| column == "shard") {
        columns.push("shard".to_owned());
    }
    let declared_types = vec![None; columns.len()];
    let tag = |shard: usize, row: &Value| -> Value {
        let mut object: Map<String, Value> = row.as_object().cloned().unwrap_or_default();
        object.insert("shard".to_owned(), Value::from(shard));
        Value::Object(object)
    };
    let rows = match merge {
        Some((keys, limit)) => {
            let per_shard: Vec<Vec<Value>> = results.into_iter().map(|r| r.rows).collect();
            merge_sorted(&per_shard, keys, limit)
                .into_iter()
                .map(|(shard, row)| tag(shard, &row))
                .collect()
        }
        None => results
            .iter()
            .enumerate()
            .flat_map(|(shard, result)| result.rows.iter().map(move |row| (shard, row)))
            .map(|(shard, row)| tag(shard, row))
            .collect(),
    };
    TypedQueryResult {
        columns,
        declared_types,
        rows,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn fnv1a64_matches_reference_vectors() {
        // Published FNV-1a 64-bit test vectors.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn jump_hash_matches_reference_vectors() {
        // Computed with an independent implementation of the paper's algorithm.
        assert_eq!(jump_consistent_hash(0, 1), 0);
        assert_eq!(jump_consistent_hash(0, 10), 0);
        assert_eq!(jump_consistent_hash(1, 2), 0);
        assert_eq!(jump_consistent_hash(2_862_933_555_777_941_757, 100), 71);
    }

    #[test]
    fn routing_is_frozen() {
        // Pinned forever: changing any of these breaks existing deployments.
        let golden: [(&str, usize, usize); 12] = [
            ("player-1", 4, GOLDEN[0]),
            ("player-2", 4, GOLDEN[1]),
            ("player-3", 4, GOLDEN[2]),
            ("12345", 8, GOLDEN[3]),
            ("12345", 4, GOLDEN[4]),
            ("alice", 2, GOLDEN[5]),
            ("alice", 16, GOLDEN[6]),
            ("", 8, GOLDEN[7]),
            ("héllo", 8, GOLDEN[8]),
            ("guild:42", 3, GOLDEN[9]),
            ("guild:42", 1, 0),
            ("x", 256, GOLDEN[10]),
        ];
        for (key, shards, expected) in golden {
            assert_eq!(shard_for_key(key, shards), expected, "{key:?}/{shards}");
        }
        assert_eq!(shard_file_name("game", 7), "game.shard-0007.novadb");
    }

    const GOLDEN: [usize; 11] = [0, 0, 1, 7, 3, 0, 4, 1, 4, 2, 41];

    #[test]
    fn keys_spread_and_grow_consistently() {
        let mut counts = [0_usize; 8];
        for i in 0..8000 {
            counts[shard_for_key(&format!("player-{i}"), 8)] += 1;
        }
        assert!(
            counts.iter().all(|&c| (800..1200).contains(&c)),
            "{counts:?}"
        );
        // Growing 8 -> 9 shards only moves keys onto the new shard.
        for i in 0..2000 {
            let key = format!("player-{i}");
            let (before, after) = (shard_for_key(&key, 8), shard_for_key(&key, 9));
            assert!(after == before || after == 8);
        }
    }

    fn keys(spec: &[(&str, bool)]) -> Vec<SortKey> {
        spec.iter()
            .map(|(column, desc)| SortKey {
                column: (*column).to_owned(),
                desc: *desc,
            })
            .collect()
    }

    #[test]
    fn merge_sorted_interleaves_and_limits() {
        let shards = vec![
            vec![
                json!({"score": 90, "n": "a"}),
                json!({"score": 50, "n": "b"}),
                json!({"score": 10, "n": "c"}),
            ],
            vec![
                json!({"score": 95, "n": "d"}),
                json!({"score": 50, "n": "e"}),
            ],
            vec![],
            vec![json!({"score": 70, "n": "f"})],
        ];
        let merged = merge_sorted(&shards, &keys(&[("score", true)]), Some(5));
        let names: Vec<_> = merged
            .iter()
            .map(|(_, r)| r["n"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["d", "a", "f", "b", "e"]);
        assert_eq!(merged[3].0, 0, "tie broken by lower shard first");
        let all = merge_sorted(&shards, &keys(&[("score", true)]), None);
        assert_eq!(all.len(), 6);
        let asc: Vec<_> = merge_sorted(
            &[
                vec![json!({"s": null}), json!({"s": 1})],
                vec![json!({"s": 0.5}), json!({"s": "x"})],
            ],
            &keys(&[("s", false)]),
            None,
        )
        .into_iter()
        .map(|(_, r)| r["s"].clone())
        .collect();
        assert_eq!(asc, [json!(null), json!(0.5), json!(1), json!("x")]);
    }

    #[test]
    fn merge_with_two_keys() {
        let shards = vec![
            vec![json!({"a": 1, "b": 2}), json!({"a": 2, "b": 1})],
            vec![json!({"a": 1, "b": 1}), json!({"a": 2, "b": 5})],
        ];
        let merged = merge_sorted(&shards, &keys(&[("a", false), ("b", true)]), None);
        let order: Vec<_> = merged
            .iter()
            .map(|(_, r)| (r["a"].as_i64().unwrap(), r["b"].as_i64().unwrap()))
            .collect();
        assert_eq!(order, [(1, 2), (1, 1), (2, 5), (2, 1)]);
    }
}
