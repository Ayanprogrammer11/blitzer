mod chunk;
mod http;
mod manifest;
mod no_range;
mod parts;
mod transfer;
mod tuning;

#[cfg(test)]
mod tests;

use crate::config::DownloadConfig;
use anyhow::{Context, Result};
use chunk::RangeDownloadError;
use http::{probe_remote, resolve_output_path};
use manifest::build_chunks;
use no_range::{NoRangeDownload, download_no_range_overlap};
use parts::{compute_resume_offset, ensure_parent_dir, part_dir_for, remove_state_dir_if_exists};
use reqwest::Client;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    fs,
    sync::{Notify, mpsc},
    time::sleep,
};
use transfer::{ParallelDownload, download_parallel, download_single};
use tuning::plan_transfer;

const RATE_LIMIT_REQUEST_SPAN_BYTES: u64 = 128 * 1024 * 1024;
const CONSERVATIVE_RANGE_WORKERS: usize = 4;

#[derive(Clone)]
pub struct CancelToken {
    inner: Arc<CancelState>,
}

struct CancelState {
    cancelled: AtomicBool,
    notify: Notify,
}

impl Default for CancelToken {
    fn default() -> Self {
        Self {
            inner: Arc::new(CancelState {
                cancelled: AtomicBool::new(false),
                notify: Notify::new(),
            }),
        }
    }
}

impl CancelToken {
    pub fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::SeqCst) {
            self.inner.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        loop {
            let notified = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
            if self.is_cancelled() {
                return;
            }
        }
    }
}

#[derive(Debug)]
pub(super) struct DownloadCancelled {
    saved_bytes: u64,
}

impl DownloadCancelled {
    pub(super) fn new(saved_bytes: u64) -> Self {
        Self { saved_bytes }
    }

    fn saved_bytes(&self) -> u64 {
        self.saved_bytes
    }
}

impl std::fmt::Display for DownloadCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "download cancelled after saving {} verified bytes",
            self.saved_bytes
        )
    }
}

impl std::error::Error for DownloadCancelled {}

#[derive(Debug, Clone)]
pub struct DownloadSummary {
    pub output: PathBuf,
    pub final_size: u64,
    pub newly_transferred: u64,
    pub elapsed: Duration,
    pub used_parallel: bool,
}

#[derive(Debug, Clone)]
pub enum DownloadEvent {
    Phase(String),
    TargetResolved {
        output: PathBuf,
        total_size: Option<u64>,
        supports_ranges: bool,
    },
    ResumeOffset(u64),
    PlanSelected {
        strategy: String,
        workers: usize,
        segments: usize,
        segment_size: u64,
    },
    ProgressReset,
    Advanced(u64),
    Cancelled {
        saved_bytes: u64,
    },
    Completed(DownloadSummary),
    Failed(String),
}

#[cfg(test)]
pub async fn run_download(
    cfg: DownloadConfig,
    tx: mpsc::UnboundedSender<DownloadEvent>,
) -> Result<()> {
    run_download_with_cancel(cfg, tx, CancelToken::default()).await
}

pub async fn run_download_with_cancel(
    cfg: DownloadConfig,
    tx: mpsc::UnboundedSender<DownloadEvent>,
    cancel: CancelToken,
) -> Result<()> {
    let client = Client::builder()
        .user_agent("blitzer/0.2.0")
        .timeout(Duration::from_secs(cfg.timeout_secs))
        .build()
        .context("failed to build HTTP client")?;

    let _ = tx.send(DownloadEvent::Phase("Probing remote server...".to_string()));
    let remote = tokio::select! {
        _ = cancel.cancelled() => {
            let _ = tx.send(DownloadEvent::Cancelled { saved_bytes: 0 });
            return Ok(());
        }
        result = probe_remote(&client, &cfg.url) => result?,
    };
    let output = resolve_output_path(
        &cfg.url,
        cfg.output.clone(),
        remote.suggested_filename.clone(),
    );
    ensure_parent_dir(&output).await?;

    let _ = tx.send(DownloadEvent::TargetResolved {
        output: output.clone(),
        total_size: remote.size,
        supports_ranges: remote.supports_ranges,
    });

    let start = Instant::now();
    let mut used_parallel = false;
    let transferred_new = if let (Some(total), true) = (remote.size, remote.supports_ranges) {
        let _ = tx.send(DownloadEvent::Phase(format!(
            "Selecting {} parallel strategy...",
            cfg.connections.label()
        )));
        let parallel = ParallelDownload {
            client: &client,
            url: &cfg.url,
            output: &output,
            remote: &remote,
            total_size: total,
            connections: cfg.connections,
            worker_limit: None,
            max_request_bytes: None,
            retries: cfg.retries,
            retry_rate_limits: false,
            no_resume: cfg.no_resume,
            cancel: cancel.clone(),
            tx: tx.clone(),
        };
        match download_parallel(parallel.clone()).await {
            Ok(bytes) => {
                used_parallel = true;
                bytes
            }
            Err(err) => {
                if let Some(cancelled) = download_cancelled(&err) {
                    let _ = tx.send(DownloadEvent::Cancelled {
                        saved_bytes: cancelled.saved_bytes(),
                    });
                    return Ok(());
                } else if range_error_requires_lower_concurrency(&err) {
                    if let Some(delay) = range_error_retry_after(&err) {
                        let _ = tx.send(DownloadEvent::Phase(format!(
                            "Waiting {}s before lowering range concurrency...",
                            delay.as_secs()
                        )));
                        tokio::select! {
                            _ = cancel.cancelled() => {
                                let saved_bytes = compute_saved_range_bytes(
                                    &output,
                                    total,
                                    cfg.connections,
                                )
                                .await
                                .unwrap_or(0);
                                let _ = tx.send(DownloadEvent::Cancelled { saved_bytes });
                                return Ok(());
                            }
                            _ = sleep(delay) => {}
                        }
                    }

                    let initial_workers = plan_transfer(total, cfg.connections).workers;
                    let limits = reduced_worker_limits(initial_workers);
                    if limits.is_empty() {
                        return Err(err).context("range request rejected at minimum concurrency");
                    }
                    let bytes = match retry_with_lower_range_concurrency(
                        parallel,
                        err,
                        &limits,
                        tx.clone(),
                    )
                    .await
                    {
                        Ok(bytes) => bytes,
                        Err(err) if download_cancelled(&err).is_some() => {
                            let saved_bytes =
                                download_cancelled(&err).map_or(0, DownloadCancelled::saved_bytes);
                            let _ = tx.send(DownloadEvent::Cancelled { saved_bytes });
                            return Ok(());
                        }
                        Err(err) => return Err(err),
                    };
                    used_parallel = true;
                    bytes
                } else if range_error_can_retry_without_ranges(&err) {
                    cleanup_range_parts(&output).await;
                    let _ = tx.send(DownloadEvent::ProgressReset);
                    let _ = tx.send(DownloadEvent::Phase(format!(
                        "Parallel range download failed ({err:#}); retrying without trusted ranges."
                    )));
                    match download_without_ranges(
                        &client,
                        &cfg,
                        &output,
                        None,
                        &remote.request_headers,
                        cancel.clone(),
                        tx.clone(),
                    )
                    .await
                    {
                        Ok(bytes) => bytes,
                        Err(err) if download_cancelled(&err).is_some() => {
                            let saved_bytes =
                                download_cancelled(&err).map_or(0, DownloadCancelled::saved_bytes);
                            let _ = tx.send(DownloadEvent::Cancelled { saved_bytes });
                            return Ok(());
                        }
                        Err(err) => return Err(err),
                    }
                } else {
                    return Err(err).context("parallel range download failed");
                }
            }
        }
    } else {
        match download_without_ranges(
            &client,
            &cfg,
            &output,
            remote.size,
            &remote.request_headers,
            cancel.clone(),
            tx.clone(),
        )
        .await
        {
            Ok(bytes) => bytes,
            Err(err) if download_cancelled(&err).is_some() => {
                let saved_bytes =
                    download_cancelled(&err).map_or(0, DownloadCancelled::saved_bytes);
                let _ = tx.send(DownloadEvent::Cancelled { saved_bytes });
                return Ok(());
            }
            Err(err) => return Err(err),
        }
    };

    let elapsed = start.elapsed();
    let final_size = fs::metadata(&output)
        .await
        .with_context(|| format!("failed to read output metadata: {}", output.display()))?
        .len();
    let _ = tx.send(DownloadEvent::Completed(DownloadSummary {
        output,
        final_size,
        newly_transferred: transferred_new,
        elapsed,
        used_parallel,
    }));
    Ok(())
}

async fn cleanup_range_parts(output: &std::path::Path) {
    if let Ok(part_dir) = part_dir_for(output) {
        let _ = remove_state_dir_if_exists(&part_dir).await;
    }
}

async fn compute_saved_range_bytes(
    output: &std::path::Path,
    total_size: u64,
    connections: crate::config::ConnectionStrategy,
) -> Result<u64> {
    let plan = plan_transfer(total_size, connections);
    let chunks = build_chunks(total_size, plan.segments);
    let part_dir = part_dir_for(output)?;
    compute_resume_offset(&part_dir, &chunks).await
}

async fn download_without_ranges(
    client: &Client,
    cfg: &DownloadConfig,
    output: &std::path::Path,
    total_size: Option<u64>,
    request_headers: &http::RequestHeaders,
    cancel: CancelToken,
    tx: mpsc::UnboundedSender<DownloadEvent>,
) -> Result<u64> {
    match cfg.no_range_strategy {
        crate::config::NoRangeStrategy::Single => {
            let _ = tx.send(DownloadEvent::Phase(
                "Server does not support byte ranges; switching to single stream.".to_string(),
            ));
            download_single(client, &cfg.url, request_headers, output, cancel, tx).await
        }
        crate::config::NoRangeStrategy::Overlap { .. } => {
            let _ = tx.send(DownloadEvent::Phase(format!(
                "Server has no byte ranges; trying {} speculative strategy.",
                cfg.no_range_strategy.label()
            )));
            match download_no_range_overlap(NoRangeDownload {
                client,
                url: &cfg.url,
                output,
                total_size,
                request_headers,
                strategy: cfg.no_range_strategy,
                retries: cfg.retries,
                cancel: cancel.clone(),
                tx: tx.clone(),
            })
            .await
            {
                Ok(bytes) => Ok(bytes),
                Err(err) => {
                    if download_cancelled(&err).is_some() {
                        return Err(err);
                    }
                    let _ = tx.send(DownloadEvent::ProgressReset);
                    let _ = tx.send(DownloadEvent::Phase(format!(
                        "No-range overlap proof failed ({err:#}); falling back to single stream."
                    )));
                    download_single(client, &cfg.url, request_headers, output, cancel, tx).await
                }
            }
        }
    }
}

fn range_error_requires_lower_concurrency(err: &anyhow::Error) -> bool {
    range_error(err)
        .and_then(RangeDownloadError::status_code)
        .map(|status| {
            matches!(
                status,
                reqwest::StatusCode::FORBIDDEN
                    | reqwest::StatusCode::TOO_MANY_REQUESTS
                    | reqwest::StatusCode::BAD_GATEWAY
                    | reqwest::StatusCode::SERVICE_UNAVAILABLE
            )
        })
        .unwrap_or(false)
}

fn range_error_can_retry_without_ranges(err: &anyhow::Error) -> bool {
    range_error(err)
        .map(RangeDownloadError::can_retry_without_ranges)
        .unwrap_or(false)
}

fn range_error_retry_after(err: &anyhow::Error) -> Option<Duration> {
    range_error(err).and_then(RangeDownloadError::retry_after)
}

fn range_error(err: &anyhow::Error) -> Option<&RangeDownloadError> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<RangeDownloadError>())
}

pub(super) fn download_cancelled(err: &anyhow::Error) -> Option<&DownloadCancelled> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<DownloadCancelled>())
}

fn reduced_worker_limits(initial_workers: usize) -> Vec<usize> {
    let mut limits = Vec::with_capacity(2);
    if initial_workers > CONSERVATIVE_RANGE_WORKERS {
        limits.push(CONSERVATIVE_RANGE_WORKERS);
    }
    if initial_workers > 1 {
        limits.push(1);
    }
    limits
}

async fn retry_with_lower_range_concurrency(
    parallel: ParallelDownload<'_>,
    initial_error: anyhow::Error,
    limits: &[usize],
    tx: mpsc::UnboundedSender<DownloadEvent>,
) -> Result<u64> {
    let mut last_error = initial_error;
    for (index, &limit) in limits.iter().enumerate() {
        let _ = tx.send(DownloadEvent::Phase(format!(
            "Server rejected higher range concurrency ({last_error:#}); retrying with {limit} worker{}.",
            if limit == 1 { "" } else { "s" }
        )));
        let retry = ParallelDownload {
            worker_limit: Some(limit),
            max_request_bytes: (limit == 1).then_some(RATE_LIMIT_REQUEST_SPAN_BYTES),
            retry_rate_limits: true,
            no_resume: false,
            ..parallel.clone()
        };
        match download_parallel(retry).await {
            Ok(bytes) => return Ok(bytes),
            Err(err)
                if download_cancelled(&err).is_none()
                    && range_error_requires_lower_concurrency(&err)
                    && index + 1 < limits.len() =>
            {
                last_error = err;
            }
            Err(err) => return Err(err),
        }
    }
    Err(last_error)
}
