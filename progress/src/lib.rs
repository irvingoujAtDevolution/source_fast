use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ScanMode {
    FullScan,
    GitInitial,
    Incremental,
}

impl ScanMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FullScan => "full-scan",
            Self::GitInitial => "git-initial",
            Self::Incremental => "incremental",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanPlan {
    pub mode: ScanMode,
    pub total_files: usize,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ScanEvent {
    Started(ScanPlan),
    /// Phase transition label (e.g., "reading packfile", "writing index").
    /// Does NOT reset counters — only updates the display label.
    PhaseChanged(String),
    FileStarted(String),
    FileFinished { path: String, bytes: u64 },
    Finished,
    Failed,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct IndexProgress {
    pub phase: String,
    pub mode: Option<String>,
    pub started_at_ms: Option<u64>,
    pub processed_files: usize,
    pub total_files: Option<usize>,
    pub processed_bytes: u64,
    pub total_bytes: Option<u64>,
    pub current_path: Option<String>,
    pub last_completed_path: Option<String>,
}

impl IndexProgress {
    pub fn building(started_at_ms: u64) -> Self {
        Self {
            phase: "building".to_string(),
            started_at_ms: Some(started_at_ms),
            ..Default::default()
        }
    }

    pub fn apply_event(&mut self, event: ScanEvent, now_ms: u64) {
        match event {
            ScanEvent::Started(plan) => {
                self.phase = "building".to_string();
                self.mode = Some(plan.mode.as_str().to_string());
                self.started_at_ms = Some(now_ms);
                self.processed_files = 0;
                self.total_files = Some(plan.total_files);
                self.processed_bytes = 0;
                self.total_bytes = Some(plan.total_bytes);
                self.current_path = None;
                self.last_completed_path = None;
            }
            ScanEvent::PhaseChanged(label) => {
                self.mode = Some(label);
                // Do NOT reset counters — progress is monotonic.
            }
            ScanEvent::FileStarted(path) => {
                self.current_path = Some(path);
            }
            ScanEvent::FileFinished { path, bytes } => {
                self.processed_files = self.processed_files.saturating_add(1);
                self.processed_bytes = self.processed_bytes.saturating_add(bytes);
                self.current_path = None;
                self.last_completed_path = Some(path);
            }
            ScanEvent::Finished => {
                self.phase = "complete".to_string();
                self.current_path = None;
            }
            ScanEvent::Failed => {
                self.phase = "failed".to_string();
                self.current_path = None;
            }
        }
    }
}
