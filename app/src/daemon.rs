use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use source_fast_core::PersistentIndex;
use source_fast_fs::{background_watcher, smart_scan};
use tokio::task;
use tracing::{debug, error, info, warn};

/// Meta keys used for daemon IPC via SQLite.
pub mod meta_keys {
    pub const SHUTDOWN_REQUESTED: &str = "shutdown_requested";
    pub const INDEX_STATUS: &str = "index_status";
    pub const DAEMON_PID: &str = "daemon_pid";
    pub const DAEMON_VERSION: &str = "daemon_version";
}

pub mod index_status {
    pub const BUILDING: &str = "building";
    pub const COMPLETE: &str = "complete";
}

/// Information about a running daemon discovered from the leader table.
#[derive(Debug)]
pub struct DaemonInfo {
    pub root: PathBuf,
    pub pid: Option<u32>,
    pub version: Option<String>,
    pub index_status: Option<String>,
    pub leader_holder: Option<String>,
    pub leader_expires_ms: Option<i64>,
}

/// Entry in the global daemons registry (~/.source_fast/daemons.json).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DaemonEntry {
    pub root: String,
    pub db_path: String,
    pub pid: u32,
}

// ---------------------------------------------------------------------------
// Daemon main loop (Step 3)
// ---------------------------------------------------------------------------

/// Initialize tracing for the daemon process (logs to .source_fast/daemon.log).
fn init_daemon_tracing(db_path: &Path) {
    use std::fs::OpenOptions;
    use tracing_subscriber::{EnvFilter, fmt};

    let log_path = db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("daemon.log");

    if OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .is_err()
    {
        return;
    }

    let make_writer = move || {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .expect("failed to open daemon log")
    };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(make_writer)
        .init();
}

/// The actual daemon main loop (invoked by `sf _daemon`).
/// Extracted from the MCP server's election loop in mcp.rs.
pub async fn run_daemon(root: PathBuf, db_path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    init_daemon_tracing(&db_path);

    info!(root = %root.display(), db = %db_path.display(), "daemon starting");

    let index = Arc::new(crate::cli::open_index_with_worktree_copy(&root, &db_path)?);

    // Clear stale state from a previous run.
    index.set_meta(meta_keys::SHUTDOWN_REQUESTED, "false")?;
    index.set_meta(meta_keys::INDEX_STATUS, index_status::BUILDING)?;
    index.set_meta(meta_keys::DAEMON_PID, &std::process::id().to_string())?;
    index.set_meta(meta_keys::DAEMON_VERSION, env!("CARGO_PKG_VERSION"))?;

    // Register in the global daemon list.
    let _ = register_daemon(&root, &db_path, std::process::id());

    // Leader election setup (same pattern as mcp.rs lines 148-156).
    let holder = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("pid:{}:{nanos}", std::process::id())
    };
    let lease_ttl = Duration::from_secs(5);
    let is_writer = Arc::new(AtomicBool::new(false));
    let index_ready = Arc::new(AtomicBool::new(false));

    let mut writer_started = false;
    let mut give_up_count = 0u32;
    // If we cannot acquire the lease after 20 iterations (10 s), another
    // daemon or MCP server already owns this repo.
    const MAX_GIVE_UP: u32 = 20;

    loop {
        // ---- Graceful shutdown check ----
        if let Ok(Some(val)) = index.get_meta(meta_keys::SHUTDOWN_REQUESTED) {
            if val == "true" {
                info!("daemon: shutdown requested, exiting gracefully");
                break;
            }
        }

        // ---- Leader election (replicates mcp.rs lines 168-196) ----
        if !is_writer.load(Ordering::SeqCst) {
            let idx = Arc::clone(&index);
            let holder_clone = holder.clone();
            let acquired = match task::spawn_blocking(move || {
                idx.try_acquire_writer_lease(&holder_clone, lease_ttl)
            })
            .await
            {
                Ok(Ok(v)) => v,
                Ok(Err(err)) => {
                    warn!("daemon: lease acquire failed: {err}");
                    false
                }
                Err(join_err) => {
                    warn!("daemon: lease acquire task panicked: {join_err}");
                    false
                }
            };

            if acquired {
                index.set_write_enabled(true);
                is_writer.store(true, Ordering::SeqCst);
                give_up_count = 0;
                info!(role = "writer", "daemon promoted role=writer");
            } else {
                give_up_count += 1;
                if give_up_count >= MAX_GIVE_UP {
                    info!("daemon: another writer holds the lease, exiting (not needed)");
                    break;
                }
            }
        }

        // ---- Writer duties (replicates mcp.rs lines 198-264) ----
        if is_writer.load(Ordering::SeqCst) {
            if !writer_started {
                writer_started = true;

                // Kick off initial scan.
                let index_for_scan = Arc::clone(&index);
                let root_for_scan = root.clone();
                let ready_for_scan = Arc::clone(&index_ready);
                task::spawn(async move {
                    let res = task::spawn_blocking(move || {
                        smart_scan(&root_for_scan, index_for_scan)
                    })
                    .await;
                    match res {
                        Ok(Ok(())) => {
                            ready_for_scan.store(true, Ordering::SeqCst);
                            info!("daemon: initial index build completed");
                        }
                        Ok(Err(err)) => {
                            error!("daemon: initial index build failed: {err}");
                        }
                        Err(join_err) => {
                            error!("daemon: initial index task panicked: {join_err}");
                        }
                    }
                });

                // Persist index_status = complete once scan finishes.
                let index_meta = Arc::clone(&index);
                let ready_poll = Arc::clone(&index_ready);
                task::spawn(async move {
                    // Poll until the scan task sets the ready flag.
                    loop {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        if ready_poll.load(Ordering::SeqCst) {
                            let _ = index_meta.set_meta(
                                meta_keys::INDEX_STATUS,
                                index_status::COMPLETE,
                            );
                            debug!("daemon: persisted index_status=complete");
                            break;
                        }
                    }
                });

                // Start file watcher.
                let index_for_watcher = Arc::clone(&index);
                let root_for_watcher = root.clone();
                task::spawn(async move {
                    if let Err(err) =
                        background_watcher(root_for_watcher, index_for_watcher).await
                    {
                        error!("daemon: file watcher stopped: {err}");
                    }
                });
            }

            // Renew lease.
            let idx = Arc::clone(&index);
            let holder_clone = holder.clone();
            let renewed = match task::spawn_blocking(move || {
                idx.renew_writer_lease(&holder_clone, lease_ttl)
            })
            .await
            {
                Ok(Ok(v)) => v,
                Ok(Err(err)) => {
                    warn!("daemon: lease renew failed: {err}");
                    false
                }
                Err(join_err) => {
                    warn!("daemon: lease renew task panicked: {join_err}");
                    false
                }
            };

            if !renewed {
                index.set_write_enabled(false);
                is_writer.store(false, Ordering::SeqCst);
                writer_started = false;
                index_ready.store(false, Ordering::SeqCst);
                info!(role = "reader", "daemon demoted role=reader");
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Cleanup: release the leader lease so `is_leader_active()` returns
    // false immediately (no need to wait for TTL expiry).
    let _ = index.release_writer_lease(&holder);
    let _ = deregister_daemon(&root);
    info!("daemon exiting");
    Ok(())
}

// ---------------------------------------------------------------------------
// Spawn / detect daemon (Steps 4 & 5)
// ---------------------------------------------------------------------------

/// Spawn a detached daemon process for the given root.
pub fn spawn_daemon(root: &Path, db_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe()?;

    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::CommandExt;
        use std::sync::Mutex;

        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const HANDLE_FLAG_INHERIT: u32 = 0x0000_0001;

        unsafe extern "system" {
            fn SetHandleInformation(hObject: isize, dwMask: u32, dwFlags: u32) -> i32;
        }

        // Mutex to protect the global handle-inherit flag manipulation.
        // Without this, concurrent threads (e.g. parallel tests) can race:
        // one thread restores the inherit flag before another's spawn completes.
        static SPAWN_LOCK: Mutex<()> = Mutex::new(());
        let _guard = SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // Temporarily clear the INHERIT flag on stdout/stderr so the daemon
        // does not inherit pipe handles from the parent. This prevents test
        // harnesses (and any caller using `Command::output()`) from hanging
        // while waiting for pipe EOF.
        let stdout_h = std::io::stdout().as_raw_handle() as isize;
        let stderr_h = std::io::stderr().as_raw_handle() as isize;
        unsafe {
            SetHandleInformation(stdout_h, HANDLE_FLAG_INHERIT, 0);
            SetHandleInformation(stderr_h, HANDLE_FLAG_INHERIT, 0);
        }

        let result = Command::new(&exe)
            .arg("_daemon")
            .arg("--root")
            .arg(root)
            .arg("--db")
            .arg(db_path)
            .creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        // Restore inherit flags (best-effort, needed if caller spawns more children).
        unsafe {
            SetHandleInformation(stdout_h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
            SetHandleInformation(stderr_h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
        }

        result?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // Safety: setsid() is safe to call in a pre_exec hook. It creates a new
        // session so the child isn't killed when the parent terminal closes.
        unsafe {
            Command::new(&exe)
                .arg("_daemon")
                .arg("--root")
                .arg(root)
                .arg("--db")
                .arg(db_path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .pre_exec(|| {
                    libc::setsid();
                    Ok(())
                })
                .spawn()?;
        }
    }

    info!(root = %root.display(), "daemon process spawned");
    Ok(())
}

/// Ensure a daemon is running for the given repo root.
/// Returns `Ok(true)` if a daemon was already running, `Ok(false)` if we spawned one.
pub fn ensure_daemon(root: &Path, db_path: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    // If the DB doesn't exist yet, create it so the daemon can open it.
    if !db_path.exists() {
        let _ = crate::cli::open_index_with_worktree_copy(root, db_path)?;
        spawn_daemon(root, db_path)?;
        return Ok(false);
    }

    // Existing DBs still need the worktree-copy/corruption recovery path.
    // WT9 exercises the case where the worktree DB exists but contains
    // invalid SQLite bytes; opening directly would fail before recovery runs.
    let index = crate::cli::open_index_with_worktree_copy(root, db_path)?;

    // Version mismatch: stop old daemon, wait, then spawn new one.
    if let Ok(Some(ver)) = index.get_meta(meta_keys::DAEMON_VERSION) {
        if ver != env!("CARGO_PKG_VERSION") {
            info!(
                old_version = %ver,
                new_version = env!("CARGO_PKG_VERSION"),
                "daemon version mismatch, restarting"
            );
            let _ = stop_daemon(db_path);
            // Wait for the old daemon to pick up the shutdown request.
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    // Check if a leader (daemon or MCP server) is already active.
    if index.is_leader_active()? {
        return Ok(true);
    }

    // Clear stale shutdown request and index_status before spawning so that
    // `--wait` doesn't see the old daemon's "complete" and return prematurely.
    let _ = index.set_meta(meta_keys::SHUTDOWN_REQUESTED, "false");
    let _ = index.set_meta(meta_keys::INDEX_STATUS, index_status::BUILDING);

    spawn_daemon(root, db_path)?;
    Ok(false)
}

/// Open a lightweight SQLite connection for polling.
/// Unlike `PersistentIndex::open_or_create`, this does NOT run `init_schema`
/// or load file IDs, avoiding write contention with the daemon.
fn open_poll_conn(db_path: &Path) -> Option<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(db_path).ok()?;
    let _ = conn.busy_timeout(Duration::from_secs(2));
    // Ensure we can read WAL data (daemon writes in WAL mode).
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    Some(conn)
}

/// Wait for the daemon to become active (leader acquired).
/// Polls the leader table every 100 ms. Returns `true` if confirmed, `false` on timeout.
pub fn wait_for_daemon(db_path: &Path, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    let poll_interval = Duration::from_millis(100);

    while start.elapsed() < timeout {
        if let Some(conn) = open_poll_conn(db_path) {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            let active: bool = conn
                .query_row(
                    "SELECT 1 FROM leader WHERE name = 'writer' AND expires_at_ms > ?1",
                    [now_ms],
                    |_row| Ok(true),
                )
                .unwrap_or(false);
            if active {
                return true;
            }
        }
        std::thread::sleep(poll_interval);
    }

    false
}

/// Wait for the index to reach "complete" status.
/// Polls the meta table. Returns `true` if complete, `false` on timeout.
pub fn wait_for_index_complete(db_path: &Path, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    let poll_interval = Duration::from_millis(500);

    while start.elapsed() < timeout {
        if let Some(conn) = open_poll_conn(db_path) {
            let status: Option<String> = conn
                .query_row(
                    "SELECT value FROM meta WHERE key = ?1",
                    [meta_keys::INDEX_STATUS],
                    |row| row.get(0),
                )
                .ok();
            if status.as_deref() == Some(index_status::COMPLETE) {
                return true;
            }
        }
        std::thread::sleep(poll_interval);
    }

    false
}

// ---------------------------------------------------------------------------
// Stop / status (Step 6)
// ---------------------------------------------------------------------------

/// Request graceful shutdown of the daemon for the given repo.
pub fn stop_daemon(db_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if !db_path.exists() {
        info!("no index database found, nothing to stop");
        return Ok(());
    }

    let index = PersistentIndex::open_or_create(db_path)?;
    index.set_meta(meta_keys::SHUTDOWN_REQUESTED, "true")?;
    info!(db = %db_path.display(), "shutdown request written");
    Ok(())
}

/// Read status of the daemon for the given repo.
pub fn daemon_status(db_path: &Path) -> Result<Option<DaemonInfo>, Box<dyn std::error::Error>> {
    if !db_path.exists() {
        return Ok(None);
    }

    let index = PersistentIndex::open_or_create(db_path)?;

    let leader_info = index.read_leader_info()?;
    let pid = index
        .get_meta(meta_keys::DAEMON_PID)?
        .and_then(|s| s.parse::<u32>().ok());
    let version = index.get_meta(meta_keys::DAEMON_VERSION)?;
    let idx_status = index.get_meta(meta_keys::INDEX_STATUS)?;

    if leader_info.is_none() && pid.is_none() {
        return Ok(None);
    }

    Ok(Some(DaemonInfo {
        root: db_path
            .parent()
            .and_then(|p| p.parent())
            .unwrap_or(Path::new("."))
            .to_path_buf(),
        pid,
        version,
        index_status: idx_status,
        leader_holder: leader_info.as_ref().map(|(h, _)| h.clone()),
        leader_expires_ms: leader_info.map(|(_, e)| e),
    }))
}

// ---------------------------------------------------------------------------
// Global daemon registry (Step 9)
// ---------------------------------------------------------------------------

fn daemons_json_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let dir = home.join(".source_fast");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("daemons.json")
}

fn read_registry() -> Vec<DaemonEntry> {
    let path = daemons_json_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn write_registry(entries: &[DaemonEntry]) -> std::io::Result<()> {
    let path = daemons_json_path();
    let content = serde_json::to_string_pretty(entries)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(&path, content)
}

/// Register a daemon in the global registry.
fn register_daemon(root: &Path, db_path: &Path, pid: u32) -> std::io::Result<()> {
    let mut entries = read_registry();
    let root_str = root.display().to_string();

    // Remove stale entry for the same root.
    entries.retain(|e| e.root != root_str);

    entries.push(DaemonEntry {
        root: root_str,
        db_path: db_path.display().to_string(),
        pid,
    });

    write_registry(&entries)
}

/// Remove a daemon from the global registry.
fn deregister_daemon(root: &Path) -> std::io::Result<()> {
    let mut entries = read_registry();
    let root_str = root.display().to_string();
    entries.retain(|e| e.root != root_str);
    write_registry(&entries)
}

/// List all known daemons from the global registry, validating each entry.
pub fn list_all_daemons() -> Result<Vec<DaemonInfo>, Box<dyn std::error::Error>> {
    let entries = read_registry();
    let mut result = Vec::new();

    for entry in &entries {
        let db = PathBuf::from(&entry.db_path);
        match daemon_status(&db)? {
            Some(info) => result.push(info),
            None => {
                // Stale entry — daemon is gone. We don't remove here to avoid
                // file-locking complexity; `sf stop --all` cleans up.
            }
        }
    }

    Ok(result)
}
