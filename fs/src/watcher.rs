use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use notify::event::{CreateKind, ModifyKind, RemoveKind};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use source_fast_core::PersistentIndex;
use tokio::sync::mpsc;
use tracing::{error, warn};

pub async fn background_watcher(root: PathBuf, index: Arc<PersistentIndex>) -> notify::Result<()> {
    background_watcher_with_cancel(root, index, Arc::new(AtomicBool::new(false))).await
}

pub async fn background_watcher_with_cancel(
    root: PathBuf,
    index: Arc<PersistentIndex>,
    cancel: Arc<AtomicBool>,
) -> notify::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<notify::Result<Event>>();

    let mut watcher: RecommendedWatcher = RecommendedWatcher::new(
        move |res| {
            let _ = tx.send(res);
        },
        Config::default(),
    )?;

    watcher.watch(&root, RecursiveMode::Recursive)?;

    let exclude_dir = root.join(".source_fast");
    let mut pending: HashMap<PathBuf, PendingAction> = HashMap::new();
    let debounce = Duration::from_millis(500);
    let poll = Duration::from_millis(100);
    let mut last_event_at: Option<std::time::Instant> = None;

    while !cancel.load(Ordering::Relaxed) {
        match tokio::time::timeout(poll, rx.recv()).await {
            Ok(Some(Ok(event))) => {
                collect_event(event, &exclude_dir, &mut pending);
                last_event_at = Some(std::time::Instant::now());
            }
            Ok(Some(Err(err))) => {
                warn!("file watcher error: {err}");
            }
            Ok(None) => break,
            Err(_) => {}
        }

        if !pending.is_empty()
            && last_event_at
                .map(|last| last.elapsed() >= debounce)
                .unwrap_or(false)
        {
            drain_pending(&mut pending, &index).await;
            last_event_at = None;
        }
    }

    if !pending.is_empty() && !cancel.load(Ordering::Relaxed) {
        drain_pending(&mut pending, &index).await;
    }

    Ok(())
}

#[derive(Clone, Copy)]
enum PendingAction {
    Upsert,
    Remove,
}

fn collect_event(event: Event, exclude_dir: &Path, pending: &mut HashMap<PathBuf, PendingAction>) {
    match event.kind {
        EventKind::Modify(ModifyKind::Data(_))
        | EventKind::Modify(ModifyKind::Any)
        | EventKind::Create(CreateKind::File) => {
            for path in event.paths {
                if path.starts_with(exclude_dir) {
                    continue;
                }
                pending.insert(path, PendingAction::Upsert);
            }
        }
        EventKind::Remove(RemoveKind::File) => {
            for path in event.paths {
                if path.starts_with(exclude_dir) {
                    continue;
                }
                pending.insert(path, PendingAction::Remove);
            }
        }
        _ => {}
    }
}

async fn drain_pending(
    pending: &mut HashMap<PathBuf, PendingAction>,
    index: &Arc<PersistentIndex>,
) {
    let events = std::mem::take(pending);
    for (path, action) in events {
        let index_clone = Arc::clone(index);
        let path_for_thread = path.clone();
        let path_display = path.display().to_string();
        let result = match action {
            PendingAction::Upsert => {
                tokio::task::spawn_blocking(move || index_clone.index_path(&path_for_thread)).await
            }
            PendingAction::Remove => {
                tokio::task::spawn_blocking(move || index_clone.remove_path(&path_for_thread)).await
            }
        };

        if let Err(join_err) = result {
            error!(
                path = %path_display,
                error = %join_err,
                "watcher task panicked"
            );
        }
    }
}
