mod scanner;
mod watcher;

pub use scanner::{initial_scan, smart_scan};
pub use watcher::background_watcher;
