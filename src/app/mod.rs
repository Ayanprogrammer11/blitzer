mod events;
mod form;
mod render;
mod render_form;
mod render_status;
mod runner;

pub use runner::run_app;

use crate::{config::DownloadConfig, download::DownloadSummary};
use form::FormState;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiMode {
    Form,
    Downloading,
    Done,
    Failed,
}

#[derive(Debug)]
struct ProgressState {
    url: String,
    output: String,
    total_size: Option<u64>,
    downloaded: u64,
    newly_transferred: u64,
    phase: String,
    mode: String,
    connections: String,
    retries: usize,
    timeout_secs: u64,
    no_resume: bool,
    started: Instant,
}

impl ProgressState {
    fn new(cfg: &DownloadConfig) -> Self {
        Self {
            url: cfg.url.to_string(),
            output: cfg
                .output
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(auto)".to_string()),
            total_size: None,
            downloaded: 0,
            newly_transferred: 0,
            phase: "Probing remote server...".to_string(),
            mode: "Detecting server capabilities...".to_string(),
            connections: cfg.connections.label(),
            retries: cfg.retries,
            timeout_secs: cfg.timeout_secs,
            no_resume: cfg.no_resume,
            started: Instant::now(),
        }
    }
}

enum AppAction {
    None,
    Start(DownloadConfig),
    CancelDownload,
    Quit,
}

struct App {
    mode: UiMode,
    form: FormState,
    progress: Option<ProgressState>,
    summary: Option<DownloadSummary>,
    failure: Option<String>,
    should_quit: bool,
    spinner_idx: usize,
}

impl Default for App {
    fn default() -> Self {
        Self {
            mode: UiMode::Form,
            form: FormState::default(),
            progress: None,
            summary: None,
            failure: None,
            should_quit: false,
            spinner_idx: 0,
        }
    }
}
