use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::event::{CreateKind, ModifyKind, RemoveKind};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use source_fast_core::PersistentIndex;
use tokio::sync::mpsc;
use tracing::{error, warn};

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
