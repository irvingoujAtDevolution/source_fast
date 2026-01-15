use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use bincode::config;
use regex::Regex;
use roaring::RoaringBitmap;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use tracing::{debug, error};

use crate::error::{IndexError, IndexResult};
use crate::model::{SearchHit, SearchResult};
use crate::text::{collect_trigrams, file_modified_timestamp, normalize_path, read_text_file};

struct FileIdState {
    file_ids: HashMap<String, u32>,
    next_file_id: u32,
}

struct SqliteStorage {
    conn: Connection,
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
    Flush,
}

struct IndexJob {
    payload: IndexPayload,
    resp: mpsc::Sender<IndexResult<()>>,
}

pub struct PersistentIndex {
    db_path: PathBuf,
    sender: mpsc::Sender<IndexJob>,
    write_enabled: Arc<AtomicBool>,
}

impl PersistentIndex {
    pub fn open_or_create(path: &Path) -> IndexResult<Self> {
        let conn = Connection::open(path)?;
        configure_connection(&conn)?;
        init_schema(&conn)?;

        let mut file_ids = HashMap::new();
        let mut max_id = 0u32;

        {
            let mut stmt = conn.prepare("SELECT id, path FROM files")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let id: i64 = row.get(0)?;
                let path: String = row.get(1)?;
                if id >= 0 {
                    let id_u32 = id as u32;
                    if id_u32 > max_id {
                        max_id = id_u32;
                    }
                    file_ids.insert(path, id_u32);
                }
            }
        }

        let ids = FileIdState {
            file_ids,
            next_file_id: max_id.saturating_add(1),
        };

        let storage = SqliteStorage { conn, ids };

        let (tx, rx) = mpsc::channel::<IndexJob>();
        let write_enabled = Arc::new(AtomicBool::new(true));
        let write_enabled_for_thread = Arc::clone(&write_enabled);
        thread::spawn(move || writer_loop(storage, rx, write_enabled_for_thread));

        Ok(Self {
            db_path: path.to_path_buf(),
            sender: tx,
            write_enabled,
        })
    }

    pub fn set_write_enabled(&self, enabled: bool) {
        self.write_enabled.store(enabled, Ordering::SeqCst);
    }

    pub fn write_enabled(&self) -> bool {
        self.write_enabled.load(Ordering::SeqCst)
    }

    pub fn index_path(&self, path: &Path) -> IndexResult<()> {
        if !self.write_enabled() {
            return Ok(());
        }
        let normalized = normalize_path(path);

        let content = match read_text_file(path)? {
            Some(c) => c,
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

        self.sender
            .send(job)
            .map_err(|_| IndexError::Encode("index writer thread terminated".to_string()))?;

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

        self.sender
            .send(job)
            .map_err(|_| IndexError::Encode("index writer thread terminated".to_string()))?;

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

        self.sender
            .send(job)
            .map_err(|_| IndexError::Encode("index writer thread terminated".to_string()))?;

        match resp_rx.recv() {
            Ok(result) => result,
            Err(_) => Err(IndexError::Encode(
                "index writer thread terminated".to_string(),
            )),
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
        let conn = Connection::open(&self.db_path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        search_with_conn(&conn, query, file_regex)
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

    /// Read a value from the meta table, if present.
    pub fn get_meta(&self, key: &str) -> IndexResult<Option<String>> {
        let conn = Connection::open(&self.db_path)?;
        conn.busy_timeout(Duration::from_secs(5))?;

        let mut stmt = conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
        let value: Option<String> = stmt.query_row([key], |row| row.get(0)).optional()?;
        Ok(value)
    }

    /// Set a value in the meta table. Used for lightweight bookkeeping like
    /// storing the last indexed git HEAD.
    pub fn set_meta(&self, key: &str, value: &str) -> IndexResult<()> {
        let conn = Connection::open(&self.db_path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn try_acquire_writer_lease(&self, holder: &str, ttl: Duration) -> IndexResult<bool> {
        let mut conn = Connection::open(&self.db_path)?;
        conn.busy_timeout(Duration::from_secs(1))?;

        let now = now_millis();
        let expires_at = now.saturating_add(ttl.as_millis().min(i64::MAX as u128) as i64);

        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT OR IGNORE INTO leader (name, holder, expires_at_ms) VALUES ('writer', '', 0)",
            [],
        )?;
        let changed = tx.execute(
            "UPDATE leader
             SET holder = ?1, expires_at_ms = ?2
             WHERE name = 'writer' AND (expires_at_ms < ?3 OR holder = ?1)",
            params![holder, expires_at, now],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }

    pub fn renew_writer_lease(&self, holder: &str, ttl: Duration) -> IndexResult<bool> {
        let mut conn = Connection::open(&self.db_path)?;
        conn.busy_timeout(Duration::from_secs(1))?;

        let now = now_millis();
        let expires_at = now.saturating_add(ttl.as_millis().min(i64::MAX as u128) as i64);

        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE leader
             SET expires_at_ms = ?2
             WHERE name = 'writer' AND holder = ?1",
            params![holder, expires_at],
        )?;
        tx.commit()?;
        Ok(changed == 1)
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
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    search_with_conn(&conn, query, file_regex)
}

pub fn search_files_in_database(path: &Path, pattern: &str) -> IndexResult<Vec<SearchHit>> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;

    if pattern.is_empty() {
        return Ok(Vec::new());
    }

    let lower_pattern = pattern.to_lowercase();
    let like_pattern = format!("%{}%", lower_pattern);

    let mut stmt =
        conn.prepare("SELECT id, path FROM files WHERE lower(path) LIKE ?1 ORDER BY path")?;

    let rows = stmt.query_map([like_pattern], |row| {
        let id: i64 = row.get(0)?;
        let path: String = row.get(1)?;
        Ok(SearchHit {
            file_id: id as u32,
            path,
        })
    })?;

    let mut hits = Vec::new();
    for hit in rows {
        hits.push(hit?);
    }

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

    let mut conn = Connection::open(db_path)?;
    conn.busy_timeout(Duration::from_secs(5))?;

    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare("SELECT id, path FROM files")?;
        let rows = stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            let path: String = row.get(1)?;
            Ok((id, path))
        })?;

        for row in rows {
            let (id, path) = row?;
            if path.starts_with(&old_prefix) {
                let suffix = &path[old_prefix.len()..];
                let new_path = format!("{new_prefix}{suffix}");
                tx.execute(
                    "UPDATE files SET path = ?1 WHERE id = ?2",
                    params![new_path, id],
                )?;
            }
        }
    }

    tx.commit()?;
    Ok(())
}

impl FileIdState {
    fn get_or_create_file_id(&mut self, path: &str) -> u32 {
        if let Some(&id) = self.file_ids.get(path) {
            return id;
        }
        let id = self.next_file_id;
        self.next_file_id = self.next_file_id.saturating_add(1);
        self.file_ids.insert(path.to_string(), id);
        id
    }

    fn remove_file_id(&mut self, path: &str) -> Option<u32> {
        self.file_ids.remove(path)
    }
}

fn upsert_file<'conn>(
    ids: &mut FileIdState,
    tx: &Transaction<'conn>,
    path: &str,
    modified_ts: u64,
    trigrams: &[[u8; 3]],
) -> IndexResult<()> {
    let file_id = ids.get_or_create_file_id(path);

    let existing_last: Option<i64> = tx
        .query_row(
            "SELECT last_modified FROM files WHERE id = ?1",
            [file_id as i64],
            |row| row.get(0),
        )
        .optional()?;

    if let Some(last) = existing_last
        && last as u64 >= modified_ts
    {
        return Ok(());
    }

    tx.execute(
        "INSERT INTO files (id, path, last_modified)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(id) DO UPDATE
             SET path = excluded.path,
                 last_modified = excluded.last_modified",
        params![file_id as i64, path, modified_ts as i64],
    )?;

    let old_trigrams_blob: Option<Vec<u8>> = tx
        .query_row(
            "SELECT trigrams FROM file_trigrams WHERE file_id = ?1",
            [file_id as i64],
            |row| row.get(0),
        )
        .optional()?;

    if let Some(blob) = old_trigrams_blob {
        let config = config::standard();
        let (old_trigrams, _) =
            bincode::serde::decode_from_slice::<Vec<[u8; 3]>, _>(&blob, config)?;

        for trigram in old_trigrams {
            let key = trigram;

            let bitmap_blob_opt: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT file_ids FROM trigrams WHERE trigram = ?1",
                    [&key[..]],
                    |row| row.get(0),
                )
                .optional()?;

            if let Some(bitmap_blob) = bitmap_blob_opt {
                let config = config::standard();
                let (mut bitmap, _) =
                    bincode::serde::decode_from_slice::<RoaringBitmap, _>(&bitmap_blob, config)?;
                bitmap.remove(file_id);
                if bitmap.is_empty() {
                    tx.execute("DELETE FROM trigrams WHERE trigram = ?1", [&key[..]])?;
                } else {
                    let config = config::standard();
                    let encoded = bincode::serde::encode_to_vec(&bitmap, config)?;
                    tx.execute(
                        "UPDATE trigrams SET file_ids = ?1 WHERE trigram = ?2",
                        params![encoded, &key[..]],
                    )?;
                }
            }
        }
    }

    let config = config::standard();
    let encoded_trigrams = bincode::serde::encode_to_vec(trigrams, config)?;
    tx.execute(
        "INSERT INTO file_trigrams (file_id, trigrams) VALUES (?1, ?2)
         ON CONFLICT(file_id) DO UPDATE SET trigrams = excluded.trigrams",
        params![file_id as i64, encoded_trigrams],
    )?;

    for trigram in trigrams {
        let key = trigram;

        let bitmap_blob_opt: Option<Vec<u8>> = tx
            .query_row(
                "SELECT file_ids FROM trigrams WHERE trigram = ?1",
                [&key[..]],
                |row| row.get(0),
            )
            .optional()?;

        let mut bitmap = if let Some(bitmap_blob) = bitmap_blob_opt {
            let config = config::standard();
            let (bm, _) =
                bincode::serde::decode_from_slice::<RoaringBitmap, _>(&bitmap_blob, config)?;
            bm
        } else {
            RoaringBitmap::new()
        };

        bitmap.insert(file_id);

        let config = config::standard();
        let encoded_bitmap = bincode::serde::encode_to_vec(&bitmap, config)?;
        tx.execute(
            "INSERT INTO trigrams (trigram, file_ids) VALUES (?1, ?2)
             ON CONFLICT(trigram) DO UPDATE SET file_ids = excluded.file_ids",
            params![&key[..], encoded_bitmap],
        )?;
    }

    Ok(())
}

fn remove_file<'conn>(
    ids: &mut FileIdState,
    tx: &Transaction<'conn>,
    path: &str,
) -> IndexResult<()> {
    let file_id = match ids.remove_file_id(path) {
        Some(id) => id,
        None => return Ok(()),
    };

    let old_trigrams_blob: Option<Vec<u8>> = tx
        .query_row(
            "SELECT trigrams FROM file_trigrams WHERE file_id = ?1",
            [file_id as i64],
            |row| row.get(0),
        )
        .optional()?;

    if let Some(blob) = old_trigrams_blob {
        let config = config::standard();
        let (old_trigrams, _) =
            bincode::serde::decode_from_slice::<Vec<[u8; 3]>, _>(&blob, config)?;

        for trigram in old_trigrams {
            let key = trigram;

            let bitmap_blob_opt: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT file_ids FROM trigrams WHERE trigram = ?1",
                    [&key[..]],
                    |row| row.get(0),
                )
                .optional()?;

            if let Some(bitmap_blob) = bitmap_blob_opt {
                let config = config::standard();
                let (mut bitmap, _) =
                    bincode::serde::decode_from_slice::<RoaringBitmap, _>(&bitmap_blob, config)?;
                bitmap.remove(file_id);
                if bitmap.is_empty() {
                    tx.execute("DELETE FROM trigrams WHERE trigram = ?1", [&key[..]])?;
                } else {
                    let config = config::standard();
                    let encoded = bincode::serde::encode_to_vec(&bitmap, config)?;
                    tx.execute(
                        "UPDATE trigrams SET file_ids = ?1 WHERE trigram = ?2",
                        params![encoded, &key[..]],
                    )?;
                }
            }
        }
    }

    tx.execute(
        "DELETE FROM file_trigrams WHERE file_id = ?1",
        [file_id as i64],
    )?;
    tx.execute("DELETE FROM files WHERE id = ?1", [file_id as i64])?;

    Ok(())
}

fn writer_loop(
    mut storage: SqliteStorage,
    rx: mpsc::Receiver<IndexJob>,
    write_enabled: Arc<AtomicBool>,
) {
    loop {
        let first = match rx.recv() {
            Ok(job) => job,
            Err(_) => {
                debug!("writer_loop: sender dropped, exiting");
                break;
            }
        };

        let mut batch = Vec::with_capacity(128);
        batch.push(first);

        while batch.len() < 128 {
            match rx.try_recv() {
                Ok(job) => batch.push(job),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    debug!("writer_loop: channel disconnected while draining");
                    return;
                }
            }
        }

        debug!("writer_loop: processing batch of {} jobs", batch.len());
        process_batch(&mut storage, batch, &write_enabled);
    }
}

fn process_batch(storage: &mut SqliteStorage, batch: Vec<IndexJob>, write_enabled: &AtomicBool) {
    use IndexPayload::*;

    if !write_enabled.load(Ordering::SeqCst) {
        for job in batch {
            let _ = job.resp.send(Ok(()));
        }
        return;
    }

    let tx = match storage.conn.transaction() {
        Ok(tx) => tx,
        Err(e) => {
            error!("failed to begin index transaction: {e}");
            let err = IndexError::Db(e);
            broadcast_batch_error(batch, err);
            return;
        }
    };

    let ids = &mut storage.ids;
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
                if let Err(err) = upsert_file(ids, &tx, path, *modified_ts, trigrams.as_slice()) {
                    batch_error = Some(err);
                    break;
                }
            }
            RemoveFile { path } => {
                removes += 1;
                if let Err(err) = remove_file(ids, &tx, path) {
                    batch_error = Some(err);
                    break;
                }
            }
            Flush => {
                flushes += 1;
            }
        }
    }

    debug!(
        "process_batch: upserts={}, removes={}, flushes={}",
        upserts, removes, flushes
    );

    if let Some(err) = batch_error {
        drop(tx);
        error!("index batch failed before commit: {err}");
        broadcast_batch_error(batch, err);
        return;
    }

    if let Err(e) = tx.commit() {
        error!("failed to commit index batch: {e}");
        let err = IndexError::Db(e);
        broadcast_batch_error(batch, err);
        return;
    }

    debug!("process_batch: commit succeeded");

    for job in batch {
        let _ = job.resp.send(Ok(()));
    }
}

fn broadcast_batch_error(batch: Vec<IndexJob>, err: IndexError) {
    let msg = format!("batch failed: {err}");
    for job in batch {
        let _ = job.resp.send(Err(IndexError::Encode(msg.clone())));
    }
}

fn configure_connection(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(())
}

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS files (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL UNIQUE,
            last_modified INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS trigrams (
            trigram BLOB PRIMARY KEY,
            file_ids BLOB NOT NULL
        );
        CREATE TABLE IF NOT EXISTS file_trigrams (
            file_id INTEGER PRIMARY KEY,
            trigrams BLOB NOT NULL,
            FOREIGN KEY(file_id) REFERENCES files(id) ON DELETE CASCADE
        );
        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS leader (
            name TEXT PRIMARY KEY,
            holder TEXT NOT NULL,
            expires_at_ms INTEGER NOT NULL
        );
        ",
    )?;
    Ok(())
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn search_with_conn(
    conn: &Connection,
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

    let mut bitmaps: Vec<RoaringBitmap> = Vec::new();
    let mut stmt = conn.prepare("SELECT file_ids FROM trigrams WHERE trigram = ?1")?;

    for trigram in &query_trigrams {
        let key = trigram;
        let blob_opt: Option<Vec<u8>> = stmt.query_row([&key[..]], |row| row.get(0)).optional()?;
        let Some(blob) = blob_opt else {
            return Ok(Vec::new());
        };
        let config = config::standard();
        let (bitmap, _) = bincode::serde::decode_from_slice::<RoaringBitmap, _>(&blob, config)?;
        bitmaps.push(bitmap);
    }

    if bitmaps.is_empty() {
        return Ok(Vec::new());
    }

    bitmaps.sort_by_key(|b| b.len());
    let mut iter = bitmaps.into_iter();
    let mut result = iter.next().unwrap_or_default();

    for bm in iter {
        result &= bm;
        if result.is_empty() {
            return Ok(Vec::new());
        }
    }

    let mut hits = Vec::new();
    let mut stmt_files = conn.prepare("SELECT path FROM files WHERE id = ?1")?;
    for file_id in result {
        let path: String = stmt_files.query_row([file_id as i64], |row| row.get(0))?;
        if let Some(re) = file_regex
            && !re.is_match(&path) {
                continue;
            }
        hits.push(SearchHit { file_id, path });
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
        let db_path = temp_dir.path().join("test_index.db");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();
        (temp_dir, index)
    }

    // ============ PersistentIndex Basic Tests ============

    #[test]
    fn test_create_new_index() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("new_index.db");

        let index = PersistentIndex::open_or_create(&db_path);
        assert!(index.is_ok());
        assert!(db_path.exists());
    }

    #[test]
    fn test_reopen_existing_index() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("existing_index.db");

        // Create and drop
        {
            let _index = PersistentIndex::open_or_create(&db_path).unwrap();
        }

        // Reopen - should succeed
        let index = PersistentIndex::open_or_create(&db_path);
        assert!(index.is_ok());
    }

    #[test]
    fn test_index_and_search_file() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.db");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        // Create a test file
        let test_file = temp_dir.path().join("test.rs");
        let mut f = std::fs::File::create(&test_file).unwrap();
        writeln!(f, "fn hello_world() {{").unwrap();
        writeln!(f, "    println!(\"Hello, World!\");").unwrap();
        writeln!(f, "}}").unwrap();
        f.flush().unwrap();

        // Index the file
        index.index_path(&test_file).unwrap();
        index.flush().unwrap();

        // Search for content
        let hits = index.search("hello_world").unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.contains("test.rs"));
    }

    #[test]
    fn test_search_query_too_short() {
        let (_temp_dir, index) = create_test_index();

        // Queries < 3 chars should return empty
        let hits = index.search("").unwrap();
        assert!(hits.is_empty());

        let hits = index.search("ab").unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn test_search_with_snippets() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.db");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        let test_file = temp_dir.path().join("test.rs");
        std::fs::write(&test_file, "fn main() { /* unique_snippet_marker */ }\n").unwrap();
        index.index_path(&test_file).unwrap();
        index.flush().unwrap();

        let results = index.search_with_snippets("unique_snippet_marker").unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].snippet.is_some(), "Expected snippet for match");
    }

    #[test]
    fn test_search_no_matches() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.db");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        // Create and index a file
        let test_file = temp_dir.path().join("test.txt");
        std::fs::write(&test_file, "hello world").unwrap();
        index.index_path(&test_file).unwrap();
        index.flush().unwrap();

        // Search for non-existent content
        let hits = index.search("nonexistent_string_xyz").unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn test_remove_file_from_index() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.db");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        // Create and index a file
        let test_file = temp_dir.path().join("removeme.txt");
        std::fs::write(&test_file, "unique_content_for_removal").unwrap();
        index.index_path(&test_file).unwrap();
        index.flush().unwrap();

        // Verify it's searchable
        let hits = index.search("unique_content_for_removal").unwrap();
        assert_eq!(hits.len(), 1);

        // Remove from index
        index.remove_path(&test_file).unwrap();
        index.flush().unwrap();

        // Should no longer be found
        let hits = index.search("unique_content_for_removal").unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn test_update_file_content() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.db");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        let test_file = temp_dir.path().join("update.txt");

        // Initial content
        std::fs::write(&test_file, "original_content_abc").unwrap();
        index.index_path(&test_file).unwrap();
        index.flush().unwrap();

        let hits = index.search("original_content").unwrap();
        assert_eq!(hits.len(), 1);

        // Update content - need to wait for mtime to change.
        // Windows NTFS has ~2 second timestamp resolution, so we need to wait longer.
        #[cfg(windows)]
        std::thread::sleep(std::time::Duration::from_secs(2));
        #[cfg(not(windows))]
        std::thread::sleep(std::time::Duration::from_millis(100));

        std::fs::write(&test_file, "updated_content_xyz").unwrap();
        index.index_path(&test_file).unwrap();
        index.flush().unwrap();

        // New content should be searchable
        let hits = index.search("updated_content").unwrap();
        assert_eq!(hits.len(), 1);
    }

    // ============ Meta Table Tests ============

    #[test]
    fn test_meta_get_set() {
        let (_temp_dir, index) = create_test_index();

        // Initially empty
        let val = index.get_meta("test_key").unwrap();
        assert!(val.is_none());

        // Set a value
        index.set_meta("test_key", "test_value").unwrap();

        // Should be retrievable
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

    // ============ Search with File Filter Tests ============

    #[test]
    fn test_search_with_file_filter() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.db");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        // Create multiple files with same content
        let rs_file = temp_dir.path().join("code.rs");
        let txt_file = temp_dir.path().join("notes.txt");

        std::fs::write(&rs_file, "shared_content_abc").unwrap();
        std::fs::write(&txt_file, "shared_content_abc").unwrap();

        index.index_path(&rs_file).unwrap();
        index.index_path(&txt_file).unwrap();
        index.flush().unwrap();

        // Without filter - both files
        let hits = index.search("shared_content").unwrap();
        assert_eq!(hits.len(), 2);

        // With filter - only .rs files
        let re = Regex::new(r"\.rs$").unwrap();
        let hits = index.search_filtered("shared_content", Some(&re)).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.ends_with(".rs"));
    }

    // ============ File Path Search Tests ============

    #[test]
    fn test_search_files_by_path() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.db");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        // Create files in subdirectories
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

        // Search by filename pattern
        let hits = search_files_in_database(&db_path, "main").unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.contains("main.rs"));

        // Search by extension
        let hits = search_files_in_database(&db_path, ".rs").unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn test_search_files_empty_pattern() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.db");
        let _index = PersistentIndex::open_or_create(&db_path).unwrap();

        let hits = search_files_in_database(&db_path, "").unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn test_search_files_case_insensitive() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.db");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        let test_file = temp_dir.path().join("MyFile.TXT");
        std::fs::write(&test_file, "content").unwrap();
        index.index_path(&test_file).unwrap();
        index.flush().unwrap();

        // Should match regardless of case
        let hits = search_files_in_database(&db_path, "myfile").unwrap();
        assert_eq!(hits.len(), 1);

        let hits = search_files_in_database(&db_path, "MYFILE").unwrap();
        assert_eq!(hits.len(), 1);
    }

    // ============ Binary File Handling Tests ============

    #[test]
    fn test_binary_file_skipped() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.db");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        // Create a binary file
        let binary_file = temp_dir.path().join("binary.bin");
        std::fs::write(&binary_file, b"hello\x00world").unwrap();

        // Should not error
        index.index_path(&binary_file).unwrap();
        index.flush().unwrap();

        // Binary content should not be searchable
        let hits = index.search("hello").unwrap();
        assert!(hits.is_empty());
    }

    // ============ Multiple Files Tests ============

    #[test]
    fn test_index_multiple_files() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.db");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        // Create multiple files
        for i in 0..10 {
            let file = temp_dir.path().join(format!("file{}.txt", i));
            std::fs::write(&file, format!("content_{}_unique", i)).unwrap();
            index.index_path(&file).unwrap();
        }
        index.flush().unwrap();

        // Each should be findable
        for i in 0..10 {
            let hits = index.search(&format!("content_{}_unique", i)).unwrap();
            assert_eq!(hits.len(), 1, "File {} should be found", i);
        }
    }

    // ============ Concurrent Access Tests ============

    #[test]
    fn test_concurrent_search() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("index.db");
        let index = PersistentIndex::open_or_create(&db_path).unwrap();

        // Index a file
        let test_file = temp_dir.path().join("concurrent.txt");
        std::fs::write(&test_file, "concurrent_test_content").unwrap();
        index.index_path(&test_file).unwrap();
        index.flush().unwrap();

        // Multiple concurrent searches
        let handles: Vec<_> = (0..5)
            .map(|_| {
                let path = db_path.clone();
                std::thread::spawn(move || {
                    let result = search_database_file(&path, "concurrent_test");
                    result.is_ok() && result.unwrap().len() == 1
                })
            })
            .collect();

        for handle in handles {
            assert!(handle.join().unwrap(), "Concurrent search should succeed");
        }
    }
}
