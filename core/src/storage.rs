use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

use bincode::config;
use heed::byteorder::NativeEndian;
use heed::types::{Bytes, Str, U32};
use heed::{Database, Env, EnvOpenOptions, RoTxn, RwTxn};
use regex::Regex;
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tracing::{debug, error};

use crate::error::{IndexError, IndexResult};
use crate::model::{SearchHit, SearchResult};
use crate::text::{collect_trigrams, file_modified_timestamp, normalize_path, read_text_file};

const MAP_SIZE: usize = 1024 * 1024 * 1024;
const MAX_DBS: u32 = 6;
const WRITER_LEADER_KEY: &str = "writer";

/// Maximum batch size in bytes before the writer thread commits.
/// Larger batches = fewer commits = faster bulk indexing.
/// 64 MB is a good balance: ~4k files per batch on typical source code.
const BATCH_MEMORY_LIMIT: usize = 64 * 1024 * 1024;

type FilesDb = Database<U32<NativeEndian>, Bytes>;
type FilesByPathDb = Database<Str, U32<NativeEndian>>;
type TrigramsDb = Database<Bytes, Bytes>;
type FileTrigramsDb = Database<U32<NativeEndian>, Bytes>;
type MetaDb = Database<Str, Str>;
type LeaderDb = Database<Str, Bytes>;

#[derive(Serialize, Deserialize)]
struct FileRecord {
    path: String,
    last_modified: u64,
}

#[derive(Serialize, Deserialize)]
struct LeaderRecord {
    holder: String,
    expires_at_ms: i64,
}

struct FileIdState {
    file_ids: HashMap<String, u32>,
    next_file_id: u32,
}

#[derive(Clone)]
struct DbHandles {
    files: FilesDb,
    files_by_path: FilesByPathDb,
    trigrams: TrigramsDb,
    file_trigrams: FileTrigramsDb,
    meta: MetaDb,
    leader: LeaderDb,
}

struct LmdbStorage {
    env: Env,
    dbs: DbHandles,
    ids: FileIdState,
}

enum IndexPayload {
    UpsertFile {
        path: String,
        modified_ts: u64,
        trigrams: Vec<[u8; 3]>,
    },
    RemoveFile {
        path: String,
    },
    SetMeta {
        key: String,
        value: String,
    },
    Flush,
}

impl IndexPayload {
    fn estimated_bytes(&self) -> usize {
        match self {
            IndexPayload::UpsertFile { path, trigrams, .. } => {
                path.len() + trigrams.len() * 3 + 64 // 64 bytes overhead estimate
            }
            IndexPayload::RemoveFile { path } => path.len() + 64,
            IndexPayload::SetMeta { key, value } => key.len() + value.len(),
            IndexPayload::Flush => 0,
        }
    }
}

struct IndexJob {
    payload: IndexPayload,
    resp: mpsc::Sender<IndexResult<()>>,
}

pub struct PersistentIndex {
    db_path: PathBuf,
    env: Env,
    dbs: DbHandles,
    sender: Option<mpsc::Sender<IndexJob>>,
    writer_handle: Option<JoinHandle<()>>,
    write_enabled: Arc<AtomicBool>,
}

impl PersistentIndex {
    pub fn open_or_create(path: &Path) -> IndexResult<Self> {
        std::fs::create_dir_all(path)?;

        let env = open_env(path)?;
        let dbs = create_databases(&env)?;
        let ids = load_file_id_state(&env, &dbs)?;

        let storage = LmdbStorage {
            env: env.clone(),
            dbs: dbs.clone(),
            ids,
        };

        let (tx, rx) = mpsc::channel::<IndexJob>();
        let write_enabled = Arc::new(AtomicBool::new(true));
        let write_enabled_for_thread = Arc::clone(&write_enabled);
        let writer_handle =
            thread::spawn(move || writer_loop(storage, rx, write_enabled_for_thread));

        Ok(Self {
            db_path: path.to_path_buf(),
            env,
            dbs,
            sender: Some(tx),
            writer_handle: Some(writer_handle),
            write_enabled,
        })
    }

    pub fn set_write_enabled(&self, enabled: bool) {
        self.write_enabled.store(enabled, Ordering::SeqCst);
    }

    pub fn write_enabled(&self) -> bool {
        self.write_enabled.load(Ordering::SeqCst)
    }

    fn sender(&self) -> IndexResult<&mpsc::Sender<IndexJob>> {
        self.sender
            .as_ref()
            .ok_or_else(|| IndexError::Encode("index has been shut down".to_string()))
    }

    pub fn index_path(&self, path: &Path) -> IndexResult<()> {
        if !self.write_enabled() {
            return Ok(());
        }

        let normalized = normalize_path(path);
        let content = match read_text_file(path)? {
            Some(content) => content,
            None => return Ok(()),
        };
        let modified_ts = file_modified_timestamp(path);
        let trigrams = collect_trigrams(&content);
        let (resp_tx, _resp_rx) = mpsc::channel();
        let job = IndexJob {
            payload: IndexPayload::UpsertFile {
                path: normalized,
                modified_ts,
                trigrams,
            },
            resp: resp_tx,
        };

        self.sender()?
            .send(job)
            .map_err(|_| IndexError::Encode("writer thread has shut down".to_string()))?;
        Ok(())
    }

    pub fn remove_path(&self, path: &Path) -> IndexResult<()> {
        if !self.write_enabled() {
            return Ok(());
        }

        let normalized = normalize_path(path);
        let (resp_tx, _resp_rx) = mpsc::channel();
        let job = IndexJob {
            payload: IndexPayload::RemoveFile { path: normalized },
            resp: resp_tx,
        };

        self.sender()?
            .send(job)
            .map_err(|_| IndexError::Encode("writer thread has shut down".to_string()))?;
        Ok(())
    }

    pub fn flush(&self) -> IndexResult<()> {
        if !self.write_enabled() {
            return Ok(());
        }

        let (resp_tx, resp_rx) = mpsc::channel();
        let job = IndexJob {
            payload: IndexPayload::Flush,
            resp: resp_tx,
        };

        self.sender()?
            .send(job)
            .map_err(|_| IndexError::Encode("writer thread has shut down".to_string()))?;

        match resp_rx.recv() {
            Ok(result) => result,
            Err(_) => Err(IndexError::Encode("writer thread dropped response".to_string())),
        }
    }

    pub fn search(&self, query: &str) -> IndexResult<Vec<SearchHit>> {
        self.search_filtered(query, None)
    }

    pub fn search_filtered(
        &self,
        query: &str,
        file_regex: Option<&Regex>,
    ) -> IndexResult<Vec<SearchHit>> {
        let rtxn = self.env.read_txn()?;
        let hits = search_with_rtxn(&rtxn, &self.dbs, query, file_regex)?;
        drop(rtxn);
        Ok(hits)
    }

    pub fn search_with_snippets(&self, query: &str) -> IndexResult<Vec<SearchResult>> {
        self.search_with_snippets_filtered(query, None)
    }

    pub fn search_with_snippets_filtered(
        &self,
        query: &str,
        file_regex: Option<&Regex>,
    ) -> IndexResult<Vec<SearchResult>> {
        let hits = self.search_filtered(query, file_regex)?;
        Ok(crate::search::attach_snippets(hits, query))
    }

    pub fn get_meta(&self, key: &str) -> IndexResult<Option<String>> {
        let rtxn = self.env.read_txn()?;
        let value = self.dbs.meta.get(&rtxn, key)?.map(str::to_string);
        drop(rtxn);
        Ok(value)
    }

    /// Write meta directly via a write transaction. Use when no writer thread
    /// is active (e.g., during daemon startup/shutdown or from CLI processes).
    pub fn set_meta(&self, key: &str, value: &str) -> IndexResult<()> {
        let mut wtxn = self.env.write_txn()?;
        self.dbs.meta.put(&mut wtxn, key, value)?;
        wtxn.commit()?;
        Ok(())
    }

    /// Queue a meta write through the writer thread channel. Use when the
    /// writer thread is running to avoid competing for the LMDB write lock.
    /// Fire-and-forget: errors are logged by the writer thread, not returned.
    pub fn set_meta_queued(&self, key: &str, value: &str) -> IndexResult<()> {
        let (resp_tx, _resp_rx) = mpsc::channel();
        let job = IndexJob {
            payload: IndexPayload::SetMeta {
                key: key.to_string(),
                value: value.to_string(),
            },
            resp: resp_tx,
        };
        self.sender()?
            .send(job)
            .map_err(|_| IndexError::Encode("writer thread has shut down".to_string()))?;
        Ok(())
    }

    pub fn try_acquire_writer_lease(&self, holder: &str, ttl: Duration) -> IndexResult<bool> {
        let now = now_millis();
        let expires_at = now.saturating_add(ttl.as_millis().min(i64::MAX as u128) as i64);

        let mut wtxn = self.env.write_txn()?;
        let current = self
            .dbs
            .leader
            .get(&wtxn, WRITER_LEADER_KEY)?
            .map(decode_bytes::<LeaderRecord>)
            .transpose()?;

        let can_acquire = match current {
            Some(ref record) => record.expires_at_ms < now || record.holder == holder,
            None => true,
        };

        if can_acquire {
            let record = LeaderRecord {
                holder: holder.to_string(),
                expires_at_ms: expires_at,
            };
            let encoded = encode_bytes(&record)?;
            self.dbs
                .leader
                .put(&mut wtxn, WRITER_LEADER_KEY, &encoded)?;
        }

        wtxn.commit()?;
        Ok(can_acquire)
    }

    pub fn renew_writer_lease(&self, holder: &str, ttl: Duration) -> IndexResult<bool> {
        let now = now_millis();
        let expires_at = now.saturating_add(ttl.as_millis().min(i64::MAX as u128) as i64);

        let mut wtxn = self.env.write_txn()?;
        let current = self
            .dbs
            .leader
            .get(&wtxn, WRITER_LEADER_KEY)?
            .map(decode_bytes::<LeaderRecord>)
            .transpose()?;

        let renewed = match current {
            Some(current) if current.holder == holder => {
                let record = LeaderRecord {
                    holder: holder.to_string(),
                    expires_at_ms: expires_at.max(now),
                };
                let encoded = encode_bytes(&record)?;
                self.dbs
                    .leader
                    .put(&mut wtxn, WRITER_LEADER_KEY, &encoded)?;
                true
            }
            _ => false,
        };

        wtxn.commit()?;
        Ok(renewed)
    }

    pub fn release_writer_lease(&self, holder: &str) -> IndexResult<()> {
        let mut wtxn = self.env.write_txn()?;
        let current = self
            .dbs
            .leader
            .get(&wtxn, WRITER_LEADER_KEY)?
            .map(decode_bytes::<LeaderRecord>)
            .transpose()?;

        if let Some(current) = current
            && current.holder == holder
        {
            let record = LeaderRecord {
                holder: current.holder,
                expires_at_ms: 0,
            };
            let encoded = encode_bytes(&record)?;
            self.dbs
                .leader
                .put(&mut wtxn, WRITER_LEADER_KEY, &encoded)?;
        }

        wtxn.commit()?;
        Ok(())
    }

    pub fn is_leader_active(&self) -> IndexResult<bool> {
        Ok(self.read_leader_info()?.is_some())
    }

    pub fn read_leader_info(&self) -> IndexResult<Option<(String, i64)>> {
        let now = now_millis();
        let rtxn = self.env.read_txn()?;
        let current = self
            .dbs
            .leader
            .get(&rtxn, WRITER_LEADER_KEY)?
            .map(decode_bytes::<LeaderRecord>)
            .transpose()?;
        drop(rtxn);

        match current {
            Some(current) if current.expires_at_ms > now => {
                Ok(Some((current.holder, current.expires_at_ms)))
            }
            _ => Ok(None),
        }
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
}

impl Drop for PersistentIndex {
    fn drop(&mut self) {
        let _ = self.sender.take();
        if let Some(handle) = self.writer_handle.take() {
            let _ = handle.join();
        }
    }
}

pub fn search_database_file(path: &Path, query: &str) -> IndexResult<Vec<SearchHit>> {
    search_database_file_filtered(path, query, None)
}

pub fn search_database_file_filtered(
    path: &Path,
    query: &str,
    file_regex: Option<&Regex>,
) -> IndexResult<Vec<SearchHit>> {
    let (env, dbs) = open_readonly_env(path)?;
    let rtxn = env.read_txn()?;
    let hits = search_with_rtxn(&rtxn, &dbs, query, file_regex)?;
    drop(rtxn);
    Ok(hits)
}

pub fn search_files_in_database(path: &Path, pattern: &str) -> IndexResult<Vec<SearchHit>> {
    if pattern.is_empty() {
        return Ok(Vec::new());
    }

    let (env, dbs) = open_readonly_env(path)?;
    let rtxn = env.read_txn()?;
    let lower_pattern = pattern.to_lowercase();
    let mut hits = Vec::new();

    for entry in dbs.files.iter(&rtxn)? {
        let (file_id, value) = entry?;
        let record: FileRecord = decode_bytes(value)?;
        if record.path.to_lowercase().contains(&lower_pattern) {
            hits.push(SearchHit {
                file_id,
                path: record.path,
            });
        }
    }

    drop(rtxn);
    hits.sort_by(|lhs, rhs| lhs.path.cmp(&rhs.path));
    Ok(hits)
}

fn ensure_trailing_separator(path: &str) -> String {
    let sep = std::path::MAIN_SEPARATOR;
    if path.ends_with(sep) {
        path.to_string()
    } else {
        format!("{path}{sep}")
    }
}

pub(crate) fn diff_sorted_trigrams(old: &[[u8; 3]], new: &[[u8; 3]]) -> (Vec<[u8; 3]>, Vec<[u8; 3]>) {
    let mut removed = Vec::new();
    let mut added = Vec::new();
    let mut old_idx = 0usize;
    let mut new_idx = 0usize;

    while old_idx < old.len() && new_idx < new.len() {
        match old[old_idx].cmp(&new[new_idx]) {
            std::cmp::Ordering::Less => {
                removed.push(old[old_idx]);
                old_idx += 1;
            }
            std::cmp::Ordering::Equal => {
                old_idx += 1;
                new_idx += 1;
            }
            std::cmp::Ordering::Greater => {
                added.push(new[new_idx]);
                new_idx += 1;
            }
        }
    }

    removed.extend_from_slice(&old[old_idx..]);
    added.extend_from_slice(&new[new_idx..]);

    (removed, added)
}

/// Rewrite all file paths in the index from `old_root` to `new_root`.
///
/// Opens the LMDB environment directly and performs a write transaction
/// without going through the writer thread. Only safe when no
/// `PersistentIndex` is active for this `db_path` (no daemon or MCP
/// server running). Called during worktree copy setup before a daemon starts.
pub fn rewrite_root_paths(
    db_path: &Path,
    old_root: &Path,
    new_root: &Path,
) -> IndexResult<()> {
    let old_norm = normalize_path(old_root);
    let new_norm = normalize_path(new_root);
    let old_prefix = ensure_trailing_separator(&old_norm);
    let new_prefix = ensure_trailing_separator(&new_norm);

    if old_prefix == new_prefix {
        return Ok(());
    }

    let env = open_env(db_path)?;
    let mut wtxn = env.write_txn()?;
    let files: FilesDb = env
        .open_database(&wtxn, Some("files"))?
        .ok_or_else(|| IndexError::Db("files db missing".to_string()))?;
    let files_by_path: FilesByPathDb = env
        .open_database(&wtxn, Some("files_by_path"))?
        .ok_or_else(|| IndexError::Db("files_by_path db missing".to_string()))?;

    let mut updates = Vec::new();
    {
        let iter = files.iter(&wtxn)?;
        for entry in iter {
            let (file_id, value) = entry?;
            let record: FileRecord = decode_bytes(value)?;
            if record.path.starts_with(&old_prefix) {
                let suffix = &record.path[old_prefix.len()..];
                let new_path = format!("{new_prefix}{suffix}");
                updates.push((
                    file_id,
                    record.path,
                    FileRecord {
                        path: new_path,
                        last_modified: record.last_modified,
                    },
                ));
            }
        }
    }

    for (file_id, old_path, new_record) in updates {
        let encoded = encode_bytes(&new_record)?;
        files.put(&mut wtxn, &file_id, &encoded)?;
        let _ = files_by_path.delete(&mut wtxn, old_path.as_str())?;
        files_by_path.put(&mut wtxn, new_record.path.as_str(), &file_id)?;
    }

    wtxn.commit()?;
    Ok(())
}

pub fn read_meta_readonly(db_path: &Path, key: &str) -> IndexResult<Option<String>> {
    let (env, dbs) = open_readonly_env(db_path)?;
    let rtxn = env.read_txn()?;
    let value = dbs.meta.get(&rtxn, key)?.map(str::to_string);
    drop(rtxn);
    Ok(value)
}

pub fn read_leader_readonly(db_path: &Path) -> IndexResult<Option<(String, i64)>> {
    let now = now_millis();
    let (env, dbs) = open_readonly_env(db_path)?;
    let rtxn = env.read_txn()?;
    let current = dbs
        .leader
        .get(&rtxn, WRITER_LEADER_KEY)?
        .map(decode_bytes::<LeaderRecord>)
        .transpose()?;
    drop(rtxn);

    match current {
        Some(record) if record.expires_at_ms > now => {
            Ok(Some((record.holder, record.expires_at_ms)))
        }
        _ => Ok(None),
    }
}

pub fn is_leader_active_readonly(db_path: &Path) -> IndexResult<bool> {
    Ok(read_leader_readonly(db_path)?.is_some())
}

impl FileIdState {
    fn get_or_create_file_id(&mut self, path: &str) -> IndexResult<u32> {
        if let Some(&id) = self.file_ids.get(path) {
            return Ok(id);
        }
        let file_id = self.next_file_id;
        self.next_file_id = self.next_file_id
            .checked_add(1)
            .ok_or_else(|| IndexError::Encode("file ID space exhausted (u32::MAX)".to_string()))?;
        self.file_ids.insert(path.to_string(), file_id);
        Ok(file_id)
    }

    fn remove_file_id(&mut self, path: &str) -> Option<u32> {
        self.file_ids.remove(path)
    }
}

fn open_env(path: &Path) -> IndexResult<Env> {
    unsafe {
        Ok(EnvOpenOptions::new()
            .max_dbs(MAX_DBS)
            .map_size(MAP_SIZE)
            .open(path)?)
    }
}

fn create_databases(env: &Env) -> IndexResult<DbHandles> {
    let mut wtxn = env.write_txn()?;
    let dbs = DbHandles {
        files: env.create_database(&mut wtxn, Some("files"))?,
        files_by_path: env.create_database(&mut wtxn, Some("files_by_path"))?,
        trigrams: env.create_database(&mut wtxn, Some("trigrams"))?,
        file_trigrams: env.create_database(&mut wtxn, Some("file_trigrams"))?,
        meta: env.create_database(&mut wtxn, Some("meta"))?,
        leader: env.create_database(&mut wtxn, Some("leader"))?,
    };
    wtxn.commit()?;
    Ok(dbs)
}

fn load_file_id_state(env: &Env, dbs: &DbHandles) -> IndexResult<FileIdState> {
    let rtxn = env.read_txn()?;
    let mut file_ids = HashMap::new();
    let mut max_id = 0u32;
    for entry in dbs.files_by_path.iter(&rtxn)? {
        let (path, file_id) = entry?;
        max_id = max_id.max(file_id);
        file_ids.insert(path.to_string(), file_id);
    }
    drop(rtxn);
    Ok(FileIdState {
        file_ids,
        next_file_id: max_id.saturating_add(1),
    })
}

/// Open the LMDB environment for read-only access. Only read transactions
/// should be created on this env. In cross-process scenarios (CLI reading
/// while daemon writes), LMDB handles concurrent access via MVCC.
fn open_readonly_env(path: &Path) -> IndexResult<(Env, DbHandles)> {
    let env = open_env(path)?;
    // LMDB requires a write transaction to open named databases for the first
    // time in a given env handle (mdb_dbi_open with named DBs needs MDB_CREATE
    // or at least a write txn). We open with a write txn, then only use read
    // txns afterwards. This is safe for cross-process access because the write
    // txn is brief (no actual data is written) and LMDB serializes it.
    let wtxn = env.write_txn()?;
    let dbs = DbHandles {
        files: env
            .open_database(&wtxn, Some("files"))?
            .ok_or_else(|| IndexError::Db("index not initialized".to_string()))?,
        files_by_path: env
            .open_database(&wtxn, Some("files_by_path"))?
            .ok_or_else(|| IndexError::Db("index not initialized".to_string()))?,
        trigrams: env
            .open_database(&wtxn, Some("trigrams"))?
            .ok_or_else(|| IndexError::Db("index not initialized".to_string()))?,
        file_trigrams: env
            .open_database(&wtxn, Some("file_trigrams"))?
            .ok_or_else(|| IndexError::Db("index not initialized".to_string()))?,
        meta: env
            .open_database(&wtxn, Some("meta"))?
            .ok_or_else(|| IndexError::Db("index not initialized".to_string()))?,
        leader: env
            .open_database(&wtxn, Some("leader"))?
            .ok_or_else(|| IndexError::Db("index not initialized".to_string()))?,
    };
    wtxn.commit()?;
    Ok((env, dbs))
}

fn writer_loop(
    mut storage: LmdbStorage,
    rx: mpsc::Receiver<IndexJob>,
    write_enabled: Arc<AtomicBool>,
) {
    loop {
        let first = match rx.recv() {
            Ok(job) => job,
            Err(_) => {
                debug!("writer_loop sender dropped, exiting");
                break;
            }
        };

        let mut batch = Vec::with_capacity(4096);
        let mut batch_bytes = first.payload.estimated_bytes();
        batch.push(first);

        while batch_bytes < BATCH_MEMORY_LIMIT {
            match rx.try_recv() {
                Ok(job) => {
                    batch_bytes += job.payload.estimated_bytes();
                    batch.push(job);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    debug!("writer_loop channel disconnected while draining, processing remaining batch");
                    break;
                }
            }
        }

        debug!(batch_len = batch.len(), "writer_loop processing batch");
        process_batch(&mut storage, batch, &write_enabled);
    }
}

fn process_batch(storage: &mut LmdbStorage, batch: Vec<IndexJob>, write_enabled: &AtomicBool) {
    use IndexPayload::*;

    if !write_enabled.load(Ordering::SeqCst) {
        for job in batch {
            let _ = job.resp.send(Ok(()));
        }
        return;
    }

    let mut wtxn = match storage.env.write_txn() {
        Ok(wtxn) => wtxn,
        Err(err) => {
            error!(error = %err, "failed to begin write transaction");
            broadcast_batch_error(batch, IndexError::Db(err.to_string()));
            return;
        }
    };

    let ids = &mut storage.ids;
    let dbs = &storage.dbs;
    let mut batch_error: Option<IndexError> = None;
    let mut upserts = 0usize;
    let mut removes = 0usize;
    let mut flushes = 0usize;

    for job in &batch {
        match &job.payload {
            UpsertFile {
                path,
                modified_ts,
                trigrams,
            } => {
                upserts += 1;
                if let Err(err) = upsert_file(ids, dbs, &mut wtxn, path, *modified_ts, trigrams) {
                    batch_error = Some(err);
                    break;
                }
            }
            RemoveFile { path } => {
                removes += 1;
                if let Err(err) = remove_file(ids, dbs, &mut wtxn, path) {
                    batch_error = Some(err);
                    break;
                }
            }
            SetMeta { key, value } => {
                if let Err(err) = dbs.meta.put(&mut wtxn, key.as_str(), value.as_str()) {
                    batch_error = Some(IndexError::from(err));
                    break;
                }
            }
            Flush => {
                flushes += 1;
            }
        }
    }

    debug!(upserts, removes, flushes, "process_batch finished");

    if let Some(err) = batch_error {
        drop(wtxn);
        error!(error = %err, "index batch failed before commit");
        broadcast_batch_error(batch, err);
        return;
    }

    if let Err(err) = wtxn.commit() {
        error!(error = %err, "failed to commit index batch");
        broadcast_batch_error(batch, IndexError::Db(err.to_string()));
        return;
    }

    debug!("process_batch commit succeeded");
    for job in batch {
        let _ = job.resp.send(Ok(()));
    }
}

fn broadcast_batch_error(batch: Vec<IndexJob>, err: IndexError) {
    let msg = err.to_string();
    for job in batch {
        let _ = job.resp.send(Err(IndexError::Db(msg.clone())));
    }
}

fn upsert_file(
    ids: &mut FileIdState,
    dbs: &DbHandles,
    wtxn: &mut RwTxn,
    path: &str,
    modified_ts: u64,
    trigrams: &[[u8; 3]],
) -> IndexResult<()> {
    let file_id = ids.get_or_create_file_id(path)?;

    let existing_record = dbs
        .files
        .get(wtxn, &file_id)?
        .map(decode_bytes::<FileRecord>)
        .transpose()?;

    if let Some(existing_record) = &existing_record
        && existing_record.last_modified >= modified_ts
    {
        return Ok(());
    }

    if let Some(existing_record) = &existing_record
        && existing_record.path != path
    {
        let _ = dbs
            .files_by_path
            .delete(wtxn, existing_record.path.as_str())?;
    }

    let record = FileRecord {
        path: path.to_string(),
        last_modified: modified_ts,
    };
    let encoded = encode_bytes(&record)?;
    dbs.files.put(wtxn, &file_id, &encoded)?;
    dbs.files_by_path.put(wtxn, path, &file_id)?;

    let old_trigrams = dbs
        .file_trigrams
        .get(wtxn, &file_id)?
        .map(decode_bytes::<Vec<[u8; 3]>>)
        .transpose()?;

    let (removed_trigrams, added_trigrams, needs_write) = match old_trigrams {
        Some(old_trigrams) => {
            let (removed, added) = diff_sorted_trigrams(&old_trigrams, trigrams);
            let needs_write = !(removed.is_empty() && added.is_empty());
            (removed, added, needs_write)
        }
        None => (Vec::new(), trigrams.to_vec(), true),
    };

    for trigram in removed_trigrams {
        if let Some(blob) = dbs.trigrams.get(wtxn, &trigram[..])? {
            let mut bitmap: RoaringBitmap = decode_bytes(blob)?;
            bitmap.remove(file_id);
            if bitmap.is_empty() {
                let _ = dbs.trigrams.delete(wtxn, &trigram[..])?;
            } else {
                let encoded = encode_bytes(&bitmap)?;
                dbs.trigrams.put(wtxn, &trigram[..], &encoded)?;
            }
        }
    }

    if needs_write {
        let encoded = encode_bytes(trigrams)?;
        dbs.file_trigrams.put(wtxn, &file_id, &encoded)?;
    }

    for trigram in added_trigrams {
        let mut bitmap = dbs
            .trigrams
            .get(wtxn, &trigram[..])?
            .map(decode_bytes::<RoaringBitmap>)
            .transpose()?
            .unwrap_or_default();
        bitmap.insert(file_id);
        let encoded = encode_bytes(&bitmap)?;
        dbs.trigrams.put(wtxn, &trigram[..], &encoded)?;
    }

    Ok(())
}

fn remove_file(
    ids: &mut FileIdState,
    dbs: &DbHandles,
    wtxn: &mut RwTxn,
    path: &str,
) -> IndexResult<()> {
    let Some(file_id) = ids.remove_file_id(path) else {
        return Ok(());
    };

    let old_trigrams = dbs
        .file_trigrams
        .get(wtxn, &file_id)?
        .map(decode_bytes::<Vec<[u8; 3]>>)
        .transpose()?
        .unwrap_or_default();

    for trigram in old_trigrams {
        if let Some(blob) = dbs.trigrams.get(wtxn, &trigram[..])? {
            let mut bitmap: RoaringBitmap = decode_bytes(blob)?;
            bitmap.remove(file_id);
            if bitmap.is_empty() {
                let _ = dbs.trigrams.delete(wtxn, &trigram[..])?;
            } else {
                let encoded = encode_bytes(&bitmap)?;
                dbs.trigrams.put(wtxn, &trigram[..], &encoded)?;
            }
        }
    }

    let _ = dbs.file_trigrams.delete(wtxn, &file_id)?;
    let _ = dbs.files.delete(wtxn, &file_id)?;
    let _ = dbs.files_by_path.delete(wtxn, path)?;
    Ok(())
}

fn encode_bytes<T: Serialize + ?Sized>(value: &T) -> IndexResult<Vec<u8>> {
    let config = config::standard();
    bincode::serde::encode_to_vec(value, config).map_err(Into::into)
}

fn decode_bytes<T: DeserializeOwned>(bytes: &[u8]) -> IndexResult<T> {
    let config = config::standard();
    let (value, _) = bincode::serde::decode_from_slice(bytes, config)?;
    Ok(value)
}

pub fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn search_with_rtxn(
    rtxn: &RoTxn,
    dbs: &DbHandles,
    query: &str,
    file_regex: Option<&Regex>,
) -> IndexResult<Vec<SearchHit>> {
    if query.len() < 3 {
        return Ok(Vec::new());
    }

    let query_trigrams = collect_trigrams(query);
    if query_trigrams.is_empty() {
        return Ok(Vec::new());
    }

    let mut bitmaps = Vec::new();
    for trigram in &query_trigrams {
        let Some(blob) = dbs.trigrams.get(rtxn, &trigram[..])? else {
            return Ok(Vec::new());
        };
        let bitmap: RoaringBitmap = decode_bytes(blob)?;
        bitmaps.push(bitmap);
    }

    bitmaps.sort_by_key(|bitmap| bitmap.len());
    let mut iter = bitmaps.into_iter();
    let mut result = iter.next().unwrap_or_default();

    for bitmap in iter {
        result &= bitmap;
        if result.is_empty() {
            return Ok(Vec::new());
        }
    }

    let mut hits = Vec::new();
    for file_id in result {
        let Some(value) = dbs.files.get(rtxn, &file_id)? else {
            continue;
        };
        let record: FileRecord = decode_bytes(value)?;
        if let Some(file_regex) = file_regex
            && !file_regex.is_match(&record.path)
        {
            continue;
        }
        hits.push(SearchHit {
            file_id,
            path: record.path,
        });
    }

    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn create_test_index() -> (TempDir, PersistentIndex) {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test_index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();
        (temp_dir, index)
    }

    #[test]
    fn test_create_new_index() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("new_index.mdb");

        let index = PersistentIndex::open_or_create(&db_path);
        assert!(index.is_ok());
        assert!(db_path.exists());
    }

    #[test]
    fn test_reopen_existing_index() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("existing_index.mdb");

        {
            let _index = PersistentIndex::open_or_create(&db_path).unwrap();
        }

        let index = PersistentIndex::open_or_create(&db_path);
        assert!(index.is_ok());
    }

    #[test]
    fn test_index_and_search_file() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        let test_file = temp_dir.path().join("test.rs");
        let mut f = std::fs::File::create(&test_file).unwrap();
        writeln!(f, "fn hello_world() {{").unwrap();
        writeln!(f, "    println!(\"Hello, World!\");").unwrap();
        writeln!(f, "}}").unwrap();
        f.flush().unwrap();

        index.index_path(&test_file).unwrap();
        index.flush().unwrap();

        let hits = index.search("hello_world").unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.contains("test.rs"));
    }

    #[test]
    fn test_search_query_too_short() {
        let (_temp_dir, index) = create_test_index();

        let hits = index.search("").unwrap();
        assert!(hits.is_empty());

        let hits = index.search("ab").unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn test_search_with_snippets() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        let test_file = temp_dir.path().join("test.rs");
        std::fs::write(&test_file, "fn main() { /* unique_snippet_marker */ }\n").unwrap();
        index.index_path(&test_file).unwrap();
        index.flush().unwrap();

        let results = index.search_with_snippets("unique_snippet_marker").unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].snippet.is_some());
    }

    #[test]
    fn test_search_no_matches() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        let test_file = temp_dir.path().join("test.txt");
        std::fs::write(&test_file, "hello world").unwrap();
        index.index_path(&test_file).unwrap();
        index.flush().unwrap();

        let hits = index.search("nonexistent_string_xyz").unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn test_remove_file_from_index() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        let test_file = temp_dir.path().join("removeme.txt");
        std::fs::write(&test_file, "unique_content_for_removal").unwrap();
        index.index_path(&test_file).unwrap();
        index.flush().unwrap();

        let hits = index.search("unique_content_for_removal").unwrap();
        assert_eq!(hits.len(), 1);

        index.remove_path(&test_file).unwrap();
        index.flush().unwrap();

        let hits = index.search("unique_content_for_removal").unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn test_update_file_content() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        let test_file = temp_dir.path().join("update.txt");
        std::fs::write(&test_file, "original_content_abc").unwrap();
        index.index_path(&test_file).unwrap();
        index.flush().unwrap();

        let hits = index.search("original_content").unwrap();
        assert_eq!(hits.len(), 1);

        #[cfg(windows)]
        std::thread::sleep(std::time::Duration::from_secs(2));
        #[cfg(not(windows))]
        std::thread::sleep(std::time::Duration::from_millis(100));

        std::fs::write(&test_file, "updated_content_xyz").unwrap();
        index.index_path(&test_file).unwrap();
        index.flush().unwrap();

        let hits = index.search("updated_content").unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn test_meta_get_set() {
        let (_temp_dir, index) = create_test_index();

        let val = index.get_meta("test_key").unwrap();
        assert!(val.is_none());

        index.set_meta("test_key", "test_value").unwrap();

        let val = index.get_meta("test_key").unwrap();
        assert_eq!(val, Some("test_value".to_string()));
    }

    #[test]
    fn test_meta_update() {
        let (_temp_dir, index) = create_test_index();

        index.set_meta("key", "value1").unwrap();
        index.set_meta("key", "value2").unwrap();

        let val = index.get_meta("key").unwrap();
        assert_eq!(val, Some("value2".to_string()));
    }

    #[test]
    fn test_search_with_file_filter() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        let rs_file = temp_dir.path().join("code.rs");
        let txt_file = temp_dir.path().join("notes.txt");

        std::fs::write(&rs_file, "shared_content_abc").unwrap();
        std::fs::write(&txt_file, "shared_content_abc").unwrap();

        index.index_path(&rs_file).unwrap();
        index.index_path(&txt_file).unwrap();
        index.flush().unwrap();

        let hits = index.search("shared_content").unwrap();
        assert_eq!(hits.len(), 2);

        let re = Regex::new(r"\.rs$").unwrap();
        let hits = index.search_filtered("shared_content", Some(&re)).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.ends_with(".rs"));
    }

    #[test]
    fn test_search_files_by_path() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        let src_dir = temp_dir.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();

        let main_rs = src_dir.join("main.rs");
        let lib_rs = src_dir.join("lib.rs");
        let readme = temp_dir.path().join("README.md");

        std::fs::write(&main_rs, "fn main() {}").unwrap();
        std::fs::write(&lib_rs, "pub mod test;").unwrap();
        std::fs::write(&readme, "# Project").unwrap();

        index.index_path(&main_rs).unwrap();
        index.index_path(&lib_rs).unwrap();
        index.index_path(&readme).unwrap();
        index.flush().unwrap();
        drop(index);

        let hits = search_files_in_database(&db_path, "main").unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.contains("main.rs"));

        let hits = search_files_in_database(&db_path, ".rs").unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn test_search_files_empty_pattern() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let _index = PersistentIndex::open_or_create(&db_path).unwrap();

        let hits = search_files_in_database(&db_path, "").unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn test_search_files_case_insensitive() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        let test_file = temp_dir.path().join("MyFile.TXT");
        std::fs::write(&test_file, "content").unwrap();
        index.index_path(&test_file).unwrap();
        index.flush().unwrap();
        drop(index);

        let hits = search_files_in_database(&db_path, "myfile").unwrap();
        assert_eq!(hits.len(), 1);

        let hits = search_files_in_database(&db_path, "MYFILE").unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn test_binary_file_skipped() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        let binary_file = temp_dir.path().join("binary.bin");
        std::fs::write(&binary_file, b"hello\x00world").unwrap();

        index.index_path(&binary_file).unwrap();
        index.flush().unwrap();

        let hits = index.search("hello").unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn test_index_multiple_files() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        for i in 0..10 {
            let file = temp_dir.path().join(format!("file{}.txt", i));
            std::fs::write(&file, format!("content_{}_unique", i)).unwrap();
            index.index_path(&file).unwrap();
        }
        index.flush().unwrap();

        for i in 0..10 {
            let hits = index.search(&format!("content_{}_unique", i)).unwrap();
            assert_eq!(hits.len(), 1, "File {} should be found", i);
        }
    }

    #[test]
    fn test_concurrent_search() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = Arc::new(PersistentIndex::open_or_create(&db_path).unwrap());

        let test_file = temp_dir.path().join("concurrent.txt");
        std::fs::write(&test_file, "concurrent_test_content").unwrap();
        index.index_path(&test_file).unwrap();
        index.flush().unwrap();

        let handles: Vec<_> = (0..5)
            .map(|_| {
                let index = Arc::clone(&index);
                std::thread::spawn(move || {
                    let result = index.search("concurrent_test");
                    result.is_ok() && result.unwrap().len() == 1
                })
            })
            .collect();

        for handle in handles {
            assert!(handle.join().unwrap(), "Concurrent search should succeed");
        }
    }

    // ============ diff_sorted_trigrams tests ============

    #[test]
    fn test_diff_sorted_trigrams_both_empty() {
        let (removed, added) = diff_sorted_trigrams(&[], &[]);
        assert!(removed.is_empty());
        assert!(added.is_empty());
    }

    #[test]
    fn test_diff_sorted_trigrams_identical() {
        let trigrams = vec![[1, 2, 3], [4, 5, 6]];
        let (removed, added) = diff_sorted_trigrams(&trigrams, &trigrams);
        assert!(removed.is_empty());
        assert!(added.is_empty());
    }

    #[test]
    fn test_diff_sorted_trigrams_disjoint() {
        let old = vec![[1, 2, 3], [4, 5, 6]];
        let new = vec![[7, 8, 9], [10, 11, 12]];
        let (removed, added) = diff_sorted_trigrams(&old, &new);
        assert_eq!(removed, old);
        assert_eq!(added, new);
    }

    #[test]
    fn test_diff_sorted_trigrams_partial_overlap() {
        let old = vec![[1, 2, 3], [4, 5, 6], [7, 8, 9]];
        let new = vec![[4, 5, 6], [7, 8, 9], [10, 11, 12]];
        let (removed, added) = diff_sorted_trigrams(&old, &new);
        assert_eq!(removed, vec![[1, 2, 3]]);
        assert_eq!(added, vec![[10, 11, 12]]);
    }

    #[test]
    fn test_diff_sorted_trigrams_old_empty() {
        let new = vec![[1, 2, 3], [4, 5, 6]];
        let (removed, added) = diff_sorted_trigrams(&[], &new);
        assert!(removed.is_empty());
        assert_eq!(added, new);
    }

    #[test]
    fn test_diff_sorted_trigrams_new_empty() {
        let old = vec![[1, 2, 3], [4, 5, 6]];
        let (removed, added) = diff_sorted_trigrams(&old, &[]);
        assert_eq!(removed, old);
        assert!(added.is_empty());
    }

    // ============ rewrite_root_paths tests ============

    #[test]
    fn test_rewrite_root_paths_basic() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        let old_root = temp_dir.path().join("old_root");
        let src = old_root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let file = src.join("main.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        index.index_path(&file).unwrap();
        index.flush().unwrap();
        drop(index);

        let new_root = temp_dir.path().join("new_root");
        rewrite_root_paths(&db_path, &old_root, &new_root).unwrap();

        let hits = search_files_in_database(&db_path, "main.rs").unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].path.contains("new_root"),
            "path should contain new_root: {}",
            hits[0].path
        );
        assert!(
            !hits[0].path.contains("old_root"),
            "path should not contain old_root: {}",
            hits[0].path
        );
    }

    #[test]
    fn test_rewrite_root_paths_same_prefix_noop() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        let root = temp_dir.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("test.rs");
        std::fs::write(&file, "fn test() {}").unwrap();

        index.index_path(&file).unwrap();
        index.flush().unwrap();
        drop(index);

        // Same root → no-op
        rewrite_root_paths(&db_path, &root, &root).unwrap();

        let hits = search_files_in_database(&db_path, "test.rs").unwrap();
        assert_eq!(hits.len(), 1);
    }

    // ============ Leader election tests ============

    #[test]
    fn test_lease_acquire_empty() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        let acquired = index
            .try_acquire_writer_lease("holder_a", Duration::from_secs(5))
            .unwrap();
        assert!(acquired);
        assert!(index.is_leader_active().unwrap());
    }

    #[test]
    fn test_lease_acquire_blocks_different_holder() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        index
            .try_acquire_writer_lease("holder_a", Duration::from_secs(5))
            .unwrap();

        let acquired = index
            .try_acquire_writer_lease("holder_b", Duration::from_secs(5))
            .unwrap();
        assert!(!acquired, "different holder should not acquire active lease");
    }

    #[test]
    fn test_lease_reacquire_same_holder() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        index
            .try_acquire_writer_lease("holder_a", Duration::from_secs(5))
            .unwrap();

        let acquired = index
            .try_acquire_writer_lease("holder_a", Duration::from_secs(5))
            .unwrap();
        assert!(acquired, "same holder should re-acquire");
    }

    #[test]
    fn test_lease_acquire_after_expiry() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        // Acquire with 1ms TTL
        index
            .try_acquire_writer_lease("holder_a", Duration::from_millis(1))
            .unwrap();

        // Wait for expiry
        std::thread::sleep(Duration::from_millis(50));

        let acquired = index
            .try_acquire_writer_lease("holder_b", Duration::from_secs(5))
            .unwrap();
        assert!(acquired, "should acquire after expiry");
    }

    #[test]
    fn test_lease_renew_correct_holder() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        index
            .try_acquire_writer_lease("holder_a", Duration::from_secs(5))
            .unwrap();

        let renewed = index
            .renew_writer_lease("holder_a", Duration::from_secs(10))
            .unwrap();
        assert!(renewed);
    }

    #[test]
    fn test_lease_renew_wrong_holder() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        index
            .try_acquire_writer_lease("holder_a", Duration::from_secs(5))
            .unwrap();

        let renewed = index
            .renew_writer_lease("holder_b", Duration::from_secs(10))
            .unwrap();
        assert!(!renewed);
    }

    #[test]
    fn test_lease_renew_no_lease() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        let renewed = index
            .renew_writer_lease("holder_a", Duration::from_secs(5))
            .unwrap();
        assert!(!renewed);
    }

    #[test]
    fn test_lease_release() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        index
            .try_acquire_writer_lease("holder_a", Duration::from_secs(5))
            .unwrap();
        assert!(index.is_leader_active().unwrap());

        index.release_writer_lease("holder_a").unwrap();
        assert!(!index.is_leader_active().unwrap());
    }

    #[test]
    fn test_lease_release_wrong_holder_noop() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        index
            .try_acquire_writer_lease("holder_a", Duration::from_secs(5))
            .unwrap();

        // Release by wrong holder should be a no-op
        index.release_writer_lease("holder_b").unwrap();
        assert!(
            index.is_leader_active().unwrap(),
            "lease should still be active after wrong-holder release"
        );
    }

    // ============ set_meta_queued tests ============

    #[test]
    fn test_set_meta_queued() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        index.set_meta_queued("test_key", "test_value").unwrap();
        index.flush().unwrap();

        let value = index.get_meta("test_key").unwrap();
        assert_eq!(value.as_deref(), Some("test_value"));
    }

    // ============ write_enabled gate tests ============

    #[test]
    fn test_write_enabled_false_blocks_indexing() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        let test_file = temp_dir.path().join("test.txt");
        std::fs::write(&test_file, "fn write_enabled_test() {}").unwrap();

        index.set_write_enabled(false);
        index.index_path(&test_file).unwrap();
        // flush is a no-op when writes disabled
        index.flush().unwrap();

        // Re-enable to allow search (search doesn't check write_enabled)
        let results = index.search("write_enabled_test").unwrap();
        assert!(results.is_empty(), "file should not be indexed when writes disabled");
    }

    #[test]
    fn test_write_enabled_true_allows_indexing() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.mdb");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        let test_file = temp_dir.path().join("test.txt");
        std::fs::write(&test_file, "fn write_enabled_test_positive() {}").unwrap();

        index.set_write_enabled(true);
        index.index_path(&test_file).unwrap();
        index.flush().unwrap();

        let results = index.search("write_enabled_test_positive").unwrap();
        assert_eq!(results.len(), 1);
    }
}
