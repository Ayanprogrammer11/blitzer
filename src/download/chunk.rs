use super::{
    CancelToken, DownloadCancelled, DownloadEvent,
    http::{RequestHeaders, apply_request_headers, validate_content_range},
    manifest::{Chunk, chunk_len},
    parts::{part_path_for, path_exists},
};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::{
    Client, StatusCode, Url,
    header::{ACCEPT_ENCODING, CONTENT_RANGE, HeaderValue, RANGE, RETRY_AFTER},
};
use std::{
    cmp::min,
    io::ErrorKind,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
    sync::mpsc,
    time::sleep,
};

pub(super) struct ChunkDownload {
    pub(super) client: Client,
    pub(super) url: Url,
    pub(super) request_headers: RequestHeaders,
    pub(super) chunk: Chunk,
    pub(super) part_path: PathBuf,
    pub(super) retries: usize,
    pub(super) retry_rate_limits: bool,
    pub(super) no_resume: bool,
    pub(super) cancel: CancelToken,
    pub(super) transferred: Arc<AtomicU64>,
    pub(super) tx: mpsc::UnboundedSender<DownloadEvent>,
}

pub(super) struct ChunkSpanDownload {
    pub(super) client: Client,
    pub(super) url: Url,
    pub(super) request_headers: RequestHeaders,
    pub(super) chunks: Vec<Chunk>,
    pub(super) part_dir: PathBuf,
    pub(super) retries: usize,
    pub(super) retry_rate_limits: bool,
    pub(super) cancel: CancelToken,
    pub(super) transferred: Arc<AtomicU64>,
    pub(super) tx: mpsc::UnboundedSender<DownloadEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RangeDownloadErrorKind {
    Status(StatusCode),
    InvalidContentRange,
    TooManyBytes,
}

#[derive(Debug)]
pub(super) struct RangeDownloadError {
    kind: RangeDownloadErrorKind,
    message: String,
    retry_after: Option<Duration>,
}

impl RangeDownloadError {
    fn new(kind: RangeDownloadErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retry_after: None,
        }
    }

    fn status(status: StatusCode, retry_after: Option<Duration>) -> Self {
        Self {
            kind: RangeDownloadErrorKind::Status(status),
            message: format!("range request was rejected with status {status}"),
            retry_after,
        }
    }

    pub(super) fn can_retry_without_ranges(&self) -> bool {
        matches!(
            self.kind,
            RangeDownloadErrorKind::Status(StatusCode::OK)
                | RangeDownloadErrorKind::InvalidContentRange
                | RangeDownloadErrorKind::TooManyBytes
        )
    }

    pub(super) fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    pub(super) fn status_code(&self) -> Option<StatusCode> {
        match self.kind {
            RangeDownloadErrorKind::Status(status) => Some(status),
            _ => None,
        }
    }
}

impl std::fmt::Display for RangeDownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(f)
    }
}

impl std::error::Error for RangeDownloadError {}

pub(super) async fn download_chunk(job: ChunkDownload) -> Result<()> {
    let ChunkDownload {
        client,
        url,
        request_headers,
        chunk,
        part_path,
        retries,
        retry_rate_limits,
        no_resume,
        cancel,
        transferred,
        tx,
    } = job;

    let expected_len = chunk_len(chunk);
    if no_resume && path_exists(&part_path).await? {
        fs::remove_file(&part_path)
            .await
            .with_context(|| format!("failed removing {}", part_path.display()))?;
    }

    let mut local_len = match fs::metadata(&part_path).await {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == ErrorKind::NotFound => 0,
        Err(e) => return Err(e).with_context(|| format!("failed stat {}", part_path.display())),
    };

    if local_len > expected_len {
        local_len = 0;
        fs::remove_file(&part_path)
            .await
            .with_context(|| format!("failed resetting {}", part_path.display()))?;
    }
    if local_len == expected_len {
        return Ok(());
    }

    for attempt in 0..=retries {
        if cancel.is_cancelled() {
            return Err(DownloadCancelled::new(0).into());
        }
        let range_start = chunk.start + local_len;
        let range_header = format!("bytes={}-{}", range_start, chunk.end);
        let resp = tokio::select! {
            _ = cancel.cancelled() => {
                return Err(DownloadCancelled::new(0).into());
            }
            resp = apply_request_headers(client.get(url.clone()), &request_headers)
                .header(RANGE, range_header)
                .header(ACCEPT_ENCODING, "identity")
                .send() => resp,
        };

        match resp {
            Ok(resp) => {
                if resp.status() != StatusCode::PARTIAL_CONTENT {
                    let status = resp.status();
                    let retry_after = parse_retry_after(resp.headers().get(RETRY_AFTER));
                    if status == StatusCode::TOO_MANY_REQUESTS
                        && retry_rate_limits
                        && attempt < retries
                    {
                        sleep_or_cancel(
                            &cancel,
                            retry_after.unwrap_or_else(|| rate_limit_backoff(attempt)),
                        )
                        .await?;
                        continue;
                    }

                    return Err(RangeDownloadError::status(status, retry_after).into());
                }

                validate_content_range(resp.headers().get(CONTENT_RANGE), range_start, chunk.end)
                    .map_err(|err| {
                    RangeDownloadError::new(
                        RangeDownloadErrorKind::InvalidContentRange,
                        format!("{err:#}"),
                    )
                })?;
                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&part_path)
                    .await
                    .with_context(|| format!("failed to open {}", part_path.display()))?;

                let mut stream = resp.bytes_stream();
                let mut stream_error = None;
                loop {
                    let next = tokio::select! {
                        _ = cancel.cancelled() => {
                            file.flush()
                                .await
                                .with_context(|| format!("failed flushing {}", part_path.display()))?;
                            return Err(DownloadCancelled::new(0).into());
                        }
                        next = stream.next() => next,
                    };
                    let Some(next) = next else {
                        break;
                    };
                    match next {
                        Ok(buf) => {
                            let bytes = buf.len() as u64;
                            let remaining = expected_len.saturating_sub(local_len);
                            if bytes > remaining {
                                return Err(RangeDownloadError::new(
                                    RangeDownloadErrorKind::TooManyBytes,
                                    format!(
                                        "chunk {} received too many bytes (got {}, remaining {})",
                                        chunk.index, bytes, remaining
                                    ),
                                )
                                .into());
                            }
                            file.write_all(&buf).await.with_context(|| {
                                format!("failed writing {}", part_path.display())
                            })?;
                            local_len = local_len.saturating_add(bytes);
                            transferred.fetch_add(bytes, Ordering::Relaxed);
                            let _ = tx.send(DownloadEvent::Advanced(bytes));
                        }
                        Err(e) => {
                            stream_error = Some(e);
                            break;
                        }
                    }
                }

                file.flush()
                    .await
                    .with_context(|| format!("failed flushing {}", part_path.display()))?;

                if let Some(e) = stream_error {
                    if attempt >= retries {
                        return Err(e).context("stream failed after retries");
                    }
                    sleep_or_cancel(&cancel, backoff(attempt)).await?;
                    continue;
                }

                if local_len == expected_len {
                    return Ok(());
                }
                if attempt >= retries {
                    bail!(
                        "chunk {} incomplete after retries (have {}, expected {})",
                        chunk.index,
                        local_len,
                        expected_len
                    );
                }
            }
            Err(e) => {
                if attempt >= retries {
                    return Err(e).context("request failed after retries");
                }
            }
        }
        sleep_or_cancel(&cancel, backoff(attempt)).await?;
    }

    bail!("chunk {} failed unexpectedly", chunk.index)
}

pub(super) async fn download_chunk_span(job: ChunkSpanDownload) -> Result<()> {
    let ChunkSpanDownload {
        client,
        url,
        request_headers,
        chunks,
        part_dir,
        retries,
        retry_rate_limits,
        cancel,
        transferred,
        tx,
    } = job;

    if chunks.is_empty() {
        return Ok(());
    }

    for attempt in 0..=retries {
        if cancel.is_cancelled() {
            return Err(DownloadCancelled::new(0).into());
        }

        let Some((span_chunks, first_local_len)) = next_span_request(&part_dir, &chunks).await?
        else {
            return Ok(());
        };
        if span_chunks.len() == 1 {
            let chunk = span_chunks[0];
            return download_chunk(ChunkDownload {
                client,
                url,
                request_headers,
                chunk,
                part_path: part_path_for(&part_dir, chunk.index),
                retries,
                retry_rate_limits,
                no_resume: false,
                cancel,
                transferred,
                tx,
            })
            .await;
        }

        let first = span_chunks[0];
        let last = *span_chunks.last().expect("span has at least one chunk");
        let range_start = first.start + first_local_len;
        let range_header = format!("bytes={}-{}", range_start, last.end);
        let resp = tokio::select! {
            _ = cancel.cancelled() => {
                return Err(DownloadCancelled::new(0).into());
            }
            resp = apply_request_headers(client.get(url.clone()), &request_headers)
                .header(RANGE, range_header)
                .header(ACCEPT_ENCODING, "identity")
                .send() => resp,
        };

        match resp {
            Ok(resp) => {
                if resp.status() != StatusCode::PARTIAL_CONTENT {
                    let status = resp.status();
                    let retry_after = parse_retry_after(resp.headers().get(RETRY_AFTER));
                    if status == StatusCode::TOO_MANY_REQUESTS
                        && retry_rate_limits
                        && attempt < retries
                    {
                        sleep_or_cancel(
                            &cancel,
                            retry_after.unwrap_or_else(|| rate_limit_backoff(attempt)),
                        )
                        .await?;
                        continue;
                    }

                    return Err(RangeDownloadError::status(status, retry_after).into());
                }

                validate_content_range(resp.headers().get(CONTENT_RANGE), range_start, last.end)
                    .map_err(|err| {
                        RangeDownloadError::new(
                            RangeDownloadErrorKind::InvalidContentRange,
                            format!("{err:#}"),
                        )
                    })?;

                match write_span_response(
                    resp,
                    &span_chunks,
                    first_local_len,
                    &part_dir,
                    &cancel,
                    &transferred,
                    &tx,
                )
                .await
                {
                    Ok(()) => {
                        if next_span_request(&part_dir, &chunks).await?.is_none() {
                            return Ok(());
                        }
                    }
                    Err(err) => {
                        if err.chain().any(|cause| cause.is::<DownloadCancelled>()) {
                            return Err(err);
                        }
                        if attempt >= retries {
                            return Err(err).context("span stream failed after retries");
                        }
                        sleep_or_cancel(&cancel, backoff(attempt)).await?;
                        continue;
                    }
                }
            }
            Err(e) => {
                if attempt >= retries {
                    return Err(e).context("request failed after retries");
                }
            }
        }
        sleep_or_cancel(&cancel, backoff(attempt)).await?;
    }

    bail!("span download failed unexpectedly")
}

async fn next_span_request(
    part_dir: &std::path::Path,
    chunks: &[Chunk],
) -> Result<Option<(Vec<Chunk>, u64)>> {
    let mut first = None;
    for (idx, chunk) in chunks.iter().copied().enumerate() {
        let local_len = part_len_or_reset(part_dir, chunk).await?;
        if local_len < chunk_len(chunk) {
            first = Some((idx, local_len));
            break;
        }
    }

    let Some((first_idx, first_local_len)) = first else {
        return Ok(None);
    };

    let mut span = vec![chunks[first_idx]];
    for chunk in chunks.iter().copied().skip(first_idx + 1) {
        let local_len = part_len_or_reset(part_dir, chunk).await?;
        if local_len != 0 {
            break;
        }
        span.push(chunk);
    }
    Ok(Some((span, first_local_len)))
}

async fn write_span_response(
    resp: reqwest::Response,
    chunks: &[Chunk],
    first_local_len: u64,
    part_dir: &std::path::Path,
    cancel: &CancelToken,
    transferred: &Arc<AtomicU64>,
    tx: &mpsc::UnboundedSender<DownloadEvent>,
) -> Result<()> {
    let mut chunk_idx = 0usize;
    let mut local_len = first_local_len;
    let mut file = Some(open_span_part(part_dir, chunks[chunk_idx], true).await?);
    let mut stream = resp.bytes_stream();

    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => {
                if let Some(file) = file.as_mut() {
                    file.flush()
                        .await
                        .with_context(|| {
                            format!(
                                "failed flushing {}",
                                part_path_for(part_dir, chunks[chunk_idx].index).display()
                            )
                        })?;
                }
                return Err(DownloadCancelled::new(0).into());
            }
            next = stream.next() => next,
        };
        let Some(next) = next else {
            break;
        };
        let buf = next.context("failed while reading span response")?;
        let mut slice = buf.as_ref();

        while !slice.is_empty() {
            if chunk_idx >= chunks.len() {
                return Err(RangeDownloadError::new(
                    RangeDownloadErrorKind::TooManyBytes,
                    "span received too many bytes",
                )
                .into());
            }

            if file.is_none() {
                file = Some(open_span_part(part_dir, chunks[chunk_idx], false).await?);
            }

            let remaining = chunk_len(chunks[chunk_idx]).saturating_sub(local_len);
            let keep = remaining.min(slice.len() as u64) as usize;
            if keep == 0 {
                flush_span_part(file.as_mut(), part_dir, chunks[chunk_idx]).await?;
                file = None;
                chunk_idx += 1;
                local_len = 0;
                continue;
            }

            let Some(out) = file.as_mut() else {
                unreachable!("span part file is opened before writing");
            };
            out.write_all(&slice[..keep]).await.with_context(|| {
                format!(
                    "failed writing {}",
                    part_path_for(part_dir, chunks[chunk_idx].index).display()
                )
            })?;
            local_len = local_len.saturating_add(keep as u64);
            transferred.fetch_add(keep as u64, Ordering::Relaxed);
            let _ = tx.send(DownloadEvent::Advanced(keep as u64));
            slice = &slice[keep..];

            if local_len == chunk_len(chunks[chunk_idx]) {
                flush_span_part(file.as_mut(), part_dir, chunks[chunk_idx]).await?;
                file = None;
                chunk_idx += 1;
                local_len = 0;
            }
        }
    }

    if let Some(file) = file.as_mut() {
        file.flush().await.with_context(|| {
            format!(
                "failed flushing {}",
                part_path_for(part_dir, chunks[chunk_idx].index).display()
            )
        })?;
    }

    if chunk_idx == chunks.len() {
        return Ok(());
    }
    bail!(
        "span incomplete after response (completed {} of {} parts)",
        chunk_idx,
        chunks.len()
    )
}

async fn open_span_part(
    part_dir: &std::path::Path,
    chunk: Chunk,
    append: bool,
) -> Result<fs::File> {
    let path = part_path_for(part_dir, chunk.index);
    let mut options = OpenOptions::new();
    options.create(true).write(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    options
        .open(&path)
        .await
        .with_context(|| format!("failed to open {}", path.display()))
}

async fn flush_span_part(
    file: Option<&mut fs::File>,
    part_dir: &std::path::Path,
    chunk: Chunk,
) -> Result<()> {
    if let Some(file) = file {
        file.flush().await.with_context(|| {
            format!(
                "failed flushing {}",
                part_path_for(part_dir, chunk.index).display()
            )
        })?;
    }
    Ok(())
}

async fn part_len_or_reset(part_dir: &std::path::Path, chunk: Chunk) -> Result<u64> {
    let part_path = part_path_for(part_dir, chunk.index);
    let len = match fs::metadata(&part_path).await {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(0),
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

pub(super) fn backoff(attempt: usize) -> Duration {
    let capped = min(attempt as u32, 6);
    Duration::from_millis(250 * (1u64 << capped))
}

fn rate_limit_backoff(attempt: usize) -> Duration {
    backoff(attempt).max(Duration::from_secs(1))
}

fn parse_retry_after(value: Option<&HeaderValue>) -> Option<Duration> {
    let seconds = value
        .and_then(|v| v.to_str().ok())
        .and_then(|raw| raw.trim().parse::<u64>().ok())?;
    Some(Duration::from_secs(seconds.min(60)))
}

async fn sleep_or_cancel(cancel: &CancelToken, duration: Duration) -> Result<()> {
    tokio::select! {
        _ = cancel.cancelled() => Err(DownloadCancelled::new(0).into()),
        _ = sleep(duration) => Ok(()),
    }
}
