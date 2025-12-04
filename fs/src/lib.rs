use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ignore::WalkBuilder;
use notify::event::{CreateKind, ModifyKind, RemoveKind};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rayon::prelude::*;
use source_fast_core::{IndexError, PersistentIndex};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

pub fn initial_scan(root: &Path, index: Arc<PersistentIndex>) -> Result<(), IndexError> {
    // First pass: count how many files we will index.
    let mut total_files = 0usize;
    {
        let exclude_dir = root.join(".source_fast");
        let walker = WalkBuilder::new(root)
            .hidden(false)
            .ignore(true)
            .git_ignore(true)
            .git_exclude(true)
            .parents(true)
            .filter_entry(move |entry| {
                let path = entry.path();
                if path.starts_with(&exclude_dir) {
                    return false;
                }
                if let Some(name) = path.file_name().and_then(|n| n.to_str())
                    && name == ".git"
                {
                    return false;
                }
                true
            })
            .build();

        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    warn!("initial_scan: failed to read entry while counting: {err}");
                    continue;
                }
            };

            if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                total_files += 1;
            }
        }
    }

    if total_files == 0 {
        info!("initial_scan: no files to index");
        return Ok(());
    }

    info!("initial_scan: found {} files to index", total_files);

    // Second pass: parallel indexing with progress.
    let counter = Arc::new(AtomicUsize::new(0));
    let index_for_scan = Arc::clone(&index);
    let counter_for_scan = Arc::clone(&counter);

    let exclude_dir = root.join(".source_fast");
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .filter_entry(move |entry| {
            let path = entry.path();
            if path.starts_with(&exclude_dir) {
                return false;
            }
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && name == ".git"
            {
                return false;
            }
            true
        })
        .build();

    walker.par_bridge().for_each(move |entry| {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                warn!("initial_scan: failed to read entry: {err}");
                return;
            }
        };

        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            return;
        }

        let done = counter_for_scan.fetch_add(1, Ordering::Relaxed) + 1;
        if done.is_multiple_of(500) {
            let remaining = total_files.saturating_sub(done);
            let percent = (done as f64 / total_files as f64) * 100.0;
            info!(
                "initial_scan: indexed {}/{} files ({percent:.1}%), {} remaining",
                done, total_files, remaining
            );
        }

        if let Err(err) = index_for_scan.index_path(entry.path()) {
            warn!(
                "initial_scan worker: failed to index {}: {:?}",
                entry.path().display(),
                err
            );
        }
    });

    index.flush()?;
    let done = counter.load(Ordering::Relaxed);
    let remaining = total_files.saturating_sub(done);
    info!(
        "initial_scan: completed, indexed {}/{} files, {} remaining",
        done, total_files, remaining
    );
    Ok(())
}

pub async fn background_watcher(root: PathBuf, index: Arc<PersistentIndex>) -> notify::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<notify::Result<Event>>();

    let mut watcher: RecommendedWatcher = RecommendedWatcher::new(
        move |res| {
            let _ = tx.send(res);
        },
        Config::default(),
    )?;

    watcher.watch(&root, RecursiveMode::Recursive)?;

    let exclude_dir = root.join(".source_fast");

    while let Some(res) = rx.recv().await {
        match res {
            Ok(event) => {
                handle_event(event, &index, &exclude_dir).await;
            }
            Err(err) => {
                warn!("file watcher error: {err}");
            }
        }
    }

    Ok(())
}

async fn handle_event(event: Event, index: &Arc<PersistentIndex>, exclude_dir: &Path) {
    let paths = event.paths;
    match event.kind {
        EventKind::Modify(ModifyKind::Data(_))
        | EventKind::Modify(ModifyKind::Any)
        | EventKind::Create(CreateKind::File) => {
            tokio::time::sleep(Duration::from_millis(500)).await;
            for path in paths {
                if path.starts_with(exclude_dir) {
                    continue;
                }
                let index_clone = Arc::clone(index);
                let path_for_thread = path.clone();
                let path_display = path.display().to_string();
                if let Err(join_err) =
                    tokio::task::spawn_blocking(move || index_clone.index_path(&path_for_thread))
                        .await
                {
                    error!(
                        "watcher: indexing task panicked for {}: {join_err}",
                        path_display
                    );
                }
            }
        }
        EventKind::Remove(RemoveKind::File) => {
            for path in paths {
                if path.starts_with(exclude_dir) {
                    continue;
                }
                let index_clone = Arc::clone(index);
                let path_for_thread = path.clone();
                let path_display = path.display().to_string();
                if let Err(join_err) =
                    tokio::task::spawn_blocking(move || index_clone.remove_path(&path_for_thread))
                        .await
                {
                    error!(
                        "watcher: remove task panicked for {}: {join_err}",
                        path_display
                    );
                }
            }
        }
        _ => {}
    }
}
