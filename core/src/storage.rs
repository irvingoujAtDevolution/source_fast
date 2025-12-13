use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use bincode::config;
use regex::Regex;
use roaring::RoaringBitmap;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use tracing::{debug, error};

use crate::error::{IndexError, IndexResult};
use crate::model::SearchHit;
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
        thread::spawn(move || writer_loop(storage, rx));

        Ok(Self {
            db_path: path.to_path_buf(),
            sender: tx,
        })
    }

    pub fn index_path(&self, path: &Path) -> IndexResult<()> {
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

fn writer_loop(mut storage: SqliteStorage, rx: mpsc::Receiver<IndexJob>) {
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
        process_batch(&mut storage, batch);
    }
}

fn process_batch(storage: &mut SqliteStorage, batch: Vec<IndexJob>) {
    use IndexPayload::*;

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
        ",
    )?;
    Ok(())
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
