use super::{
    CancelToken, DownloadCancelled, DownloadEvent,
    chunk::{ChunkDownload, ChunkSpanDownload, download_chunk, download_chunk_span},
    download_cancelled,
    http::{RemoteInfo, RequestHeaders, apply_request_headers},
    manifest::{Chunk, ResumeManifest, build_chunks, chunk_len},
    parts::{
        compute_resume_offset, merge_parts, part_dir_for, part_path_for, prepare_part_dir,
        remove_file_if_exists, replace_output,
    },
    tuning::plan_transfer,
};
use crate::config::ConnectionStrategy;
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::{Client, Url, header::ACCEPT_ENCODING};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
    sync::{Mutex, mpsc},
    task::JoinSet,
};

#[derive(Clone)]
pub(super) struct ParallelDownload<'a> {
    pub(super) client: &'a Client,
    pub(super) url: &'a Url,
    pub(super) output: &'a Path,
    pub(super) remote: &'a RemoteInfo,
    pub(super) total_size: u64,
    pub(super) connections: ConnectionStrategy,
    pub(super) worker_limit: Option<usize>,
    pub(super) max_request_bytes: Option<u64>,
    pub(super) retries: usize,
    pub(super) retry_rate_limits: bool,
    pub(super) no_resume: bool,
    pub(super) cancel: CancelToken,
    pub(super) tx: mpsc::UnboundedSender<DownloadEvent>,
}

pub(super) async fn download_single(
    client: &Client,
    url: &Url,
    request_headers: &RequestHeaders,
    output: &Path,
    cancel: CancelToken,
    tx: mpsc::UnboundedSender<DownloadEvent>,
) -> Result<u64> {
    let resp = tokio::select! {
        _ = cancel.cancelled() => {
            return Err(DownloadCancelled::new(0).into());
        }
        result = apply_request_headers(client.get(url.clone()), request_headers)
            .header(ACCEPT_ENCODING, "identity")
            .send() => result.context("failed to start download")?,
    };
    if !resp.status().is_success() {
        bail!("download request failed with status {}", resp.status());
    }

    let tmp_output = temp_output_path(output);
    remove_file_if_exists(&tmp_output).await?;
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp_output)
        .await
        .with_context(|| format!("failed to create {}", tmp_output.display()))?;

    let mut transferred = 0u64;
    let mut stream = resp.bytes_stream();
    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => {
                file.flush().await.context("failed to flush cancelled output file")?;
                remove_file_if_exists(&tmp_output).await?;
                return Err(DownloadCancelled::new(0).into());
            }
            next = stream.next() => next,
        };
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.context("failed while reading response body")?;
        file.write_all(&chunk)
            .await
            .context("failed writing output file")?;
        let bytes = chunk.len() as u64;
        transferred = transferred.saturating_add(bytes);
        let _ = tx.send(DownloadEvent::Advanced(bytes));
    }
    file.flush().await.context("failed to flush output file")?;
    replace_output(&tmp_output, output).await?;
    Ok(transferred)
}

pub(super) async fn download_parallel(plan: ParallelDownload<'_>) -> Result<u64> {
    let ParallelDownload {
        client,
        url,
        output,
        remote,
        total_size,
        connections,
        worker_limit,
        max_request_bytes,
        retries,
        retry_rate_limits,
        no_resume,
        cancel,
        tx,
    } = plan;

    if total_size == 0 {
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(output)
            .await
            .with_context(|| format!("failed to create {}", output.display()))?;
        return Ok(0);
    }

    let plan = plan_transfer(total_size, connections);
    let active_workers = worker_limit
        .map(|limit| limit.clamp(1, plan.workers))
        .unwrap_or(plan.workers);
    let _ = tx.send(DownloadEvent::Phase(plan.description()));
    let _ = tx.send(DownloadEvent::PlanSelected {
        strategy: plan.strategy.label(),
        workers: active_workers,
        segments: plan.segments,
        segment_size: plan.segment_size,
    });

    let chunks = build_chunks(total_size, plan.segments);
    let part_dir = part_dir_for(output)?;
    let expected_manifest = ResumeManifest::new(url, remote, &chunks, total_size);
    prepare_part_dir(&part_dir, &expected_manifest, no_resume, tx.clone()).await?;

    let already_downloaded = compute_resume_offset(&part_dir, &chunks).await?;
    if already_downloaded > 0 {
        let _ = tx.send(DownloadEvent::ResumeOffset(already_downloaded));
        let _ = tx.send(DownloadEvent::Phase(format!(
            "Resuming from {} verified part bytes...",
            already_downloaded
        )));
    }

    let request_span_chunks = request_span_chunks(max_request_bytes, plan.segment_size);
    if request_span_chunks > 1 {
        let _ = tx.send(DownloadEvent::Phase(format!(
            "Rate-limit retry: grouping adjacent parts into requests up to {} MiB...",
            max_request_bytes.unwrap_or(plan.segment_size) / (1024 * 1024)
        )));
    }

    let transferred = Arc::new(AtomicU64::new(0));
    let jobs = build_chunk_jobs(&part_dir, &chunks, request_span_chunks).await?;
    let queue = Arc::new(Mutex::new(VecDeque::from(jobs)));
    let mut workers = JoinSet::new();
    for _ in 0..active_workers {
        let client = client.clone();
        let url = url.clone();
        let request_headers = remote.request_headers.clone();
        let part_dir = part_dir.clone();
        let queue = queue.clone();
        let transferred = transferred.clone();
        let cancel = cancel.clone();
        let tx = tx.clone();
        workers.spawn(async move {
            download_worker(WorkerDownload {
                client,
                url,
                request_headers,
                part_dir,
                queue,
                retries,
                retry_rate_limits,
                no_resume,
                cancel,
                transferred,
                tx,
            })
            .await
        });
    }

    let mut cancelled = false;
    while let Some(joined) = workers.join_next().await {
        match joined.context("download worker panicked")? {
            Ok(()) => {}
            Err(err) if download_cancelled(&err).is_some() => {
                cancelled = true;
            }
            Err(_) if cancel.is_cancelled() => {
                cancelled = true;
            }
            Err(err) => return Err(err),
        }
    }

    if cancelled || cancel.is_cancelled() {
        let saved_bytes = compute_resume_offset(&part_dir, &chunks).await?;
        return Err(DownloadCancelled::new(saved_bytes).into());
    }

    merge_parts(&part_dir, output, &chunks, total_size).await?;
    fs::remove_dir_all(&part_dir)
        .await
        .with_context(|| format!("failed to cleanup {}", part_dir.display()))?;

    Ok(transferred.load(Ordering::Relaxed))
}

struct WorkerDownload {
    client: Client,
    url: Url,
    request_headers: RequestHeaders,
    part_dir: PathBuf,
    queue: Arc<Mutex<VecDeque<ChunkJob>>>,
    retries: usize,
    retry_rate_limits: bool,
    no_resume: bool,
    cancel: CancelToken,
    transferred: Arc<AtomicU64>,
    tx: mpsc::UnboundedSender<DownloadEvent>,
}

struct ChunkJob {
    chunks: Vec<Chunk>,
}

async fn download_worker(job: WorkerDownload) -> Result<()> {
    let WorkerDownload {
        client,
        url,
        request_headers,
        part_dir,
        queue,
        retries,
        retry_rate_limits,
        no_resume,
        cancel,
        transferred,
        tx,
    } = job;

    loop {
        if cancel.is_cancelled() {
            return Err(DownloadCancelled::new(0).into());
        }
        let job = {
            let mut queue = queue.lock().await;
            queue.pop_front()
        };
        let Some(job) = job else {
            return Ok(());
        };
        let chunks = job.chunks;

        if chunks.len() == 1 {
            let chunk = chunks[0];
            download_chunk(ChunkDownload {
                client: client.clone(),
                url: url.clone(),
                request_headers: request_headers.clone(),
                chunk,
                part_path: part_path_for(&part_dir, chunk.index),
                retries,
                retry_rate_limits,
                no_resume,
                cancel: cancel.clone(),
                transferred: transferred.clone(),
                tx: tx.clone(),
            })
            .await?;
            continue;
        }

        download_chunk_span(ChunkSpanDownload {
            client: client.clone(),
            url: url.clone(),
            request_headers: request_headers.clone(),
            chunks,
            part_dir: part_dir.clone(),
            retries,
            retry_rate_limits,
            cancel: cancel.clone(),
            transferred: transferred.clone(),
            tx: tx.clone(),
        })
        .await?;
    }
}

fn request_span_chunks(max_request_bytes: Option<u64>, segment_size: u64) -> usize {
    let Some(max_request_bytes) = max_request_bytes else {
        return 1;
    };
    (max_request_bytes / segment_size.max(1)).max(1) as usize
}

async fn build_chunk_jobs(
    part_dir: &Path,
    chunks: &[Chunk],
    request_span_chunks: usize,
) -> Result<Vec<ChunkJob>> {
    let request_span_chunks = request_span_chunks.max(1);
    let mut jobs = Vec::new();
    let mut index = 0usize;
    while index < chunks.len() {
        let local_len = part_len_or_reset(part_dir, chunks[index]).await?;
        if local_len == chunk_len(chunks[index]) {
            index += 1;
            continue;
        }

        let mut job_chunks = vec![chunks[index]];
        index += 1;
        while job_chunks.len() < request_span_chunks && index < chunks.len() {
            let local_len = part_len_or_reset(part_dir, chunks[index]).await?;
            if local_len != 0 {
                break;
            }
            job_chunks.push(chunks[index]);
            index += 1;
        }
        jobs.push(ChunkJob { chunks: job_chunks });
    }
    Ok(jobs)
}

async fn part_len_or_reset(part_dir: &Path, chunk: Chunk) -> Result<u64> {
    let part_path = part_path_for(part_dir, chunk.index);
    let len = match fs::metadata(&part_path).await {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).with_context(|| format!("failed stat {}", part_path.display())),
    };
    if len <= chunk_len(chunk) {
        return Ok(len);
    }
    fs::remove_file(&part_path)
        .await
        .with_context(|| format!("failed resetting {}", part_path.display()))?;
    Ok(0)
}

fn temp_output_path(output: &Path) -> PathBuf {
    let file = output
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "download".into());
    output
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{file}.blitzer.tmp"))
}
