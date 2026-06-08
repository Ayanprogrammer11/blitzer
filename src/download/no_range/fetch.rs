use super::{
    super::{CancelToken, DownloadCancelled, chunk::backoff, parts::remove_file_if_exists},
    DownloadEvent, SegmentMeta, segment_count, segment_path_for,
};
use crate::download::http::{RequestHeaders, apply_request_headers};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::{Client, Url, header::ACCEPT_ENCODING};
use std::{cmp::min, path::PathBuf};
use tokio::{fs::OpenOptions, io::AsyncWriteExt, sync::mpsc, task::JoinSet};

pub(super) struct FetchPlan {
    pub(super) client: Client,
    pub(super) url: Url,
    pub(super) request_headers: RequestHeaders,
    pub(super) part_dir: PathBuf,
    pub(super) total_size: Option<u64>,
    pub(super) workers: usize,
    pub(super) payload_bytes: u64,
    pub(super) overlap_bytes: u64,
    pub(super) retries: usize,
    pub(super) cancel: CancelToken,
    pub(super) tx: mpsc::UnboundedSender<DownloadEvent>,
}

#[derive(Clone)]
struct SegmentJob {
    client: Client,
    url: Url,
    request_headers: RequestHeaders,
    part_dir: PathBuf,
    index: usize,
    payload_bytes: u64,
    overlap_bytes: u64,
    retries: usize,
    cancel: CancelToken,
    tx: mpsc::UnboundedSender<DownloadEvent>,
}

pub(super) async fn fetch_segments(plan: FetchPlan) -> Result<Vec<SegmentMeta>> {
    let mut next_index = 0usize;
    let mut metas = Vec::new();
    let known_segments = plan
        .total_size
        .map(|size| segment_count(size, plan.payload_bytes));

    if known_segments.is_none() {
        let first = fetch_segment(SegmentJob {
            client: plan.client.clone(),
            url: plan.url.clone(),
            request_headers: plan.request_headers.clone(),
            part_dir: plan.part_dir.clone(),
            index: 0,
            payload_bytes: plan.payload_bytes,
            overlap_bytes: plan.overlap_bytes,
            retries: plan.retries,
            cancel: plan.cancel.clone(),
            tx: plan.tx.clone(),
        })
        .await?;
        let saw_eof = first.eof;
        metas.push(first);
        if saw_eof {
            return Ok(metas);
        }
        next_index = 1;
    }

    loop {
        let remaining = known_segments
            .map(|segments| segments.saturating_sub(next_index))
            .unwrap_or(plan.workers);
        if remaining == 0 {
            break;
        }

        let batch = remaining.min(plan.workers);
        let mut tasks = JoinSet::new();
        for index in next_index..next_index + batch {
            tasks.spawn(fetch_segment(SegmentJob {
                client: plan.client.clone(),
                url: plan.url.clone(),
                request_headers: plan.request_headers.clone(),
                part_dir: plan.part_dir.clone(),
                index,
                payload_bytes: plan.payload_bytes,
                overlap_bytes: plan.overlap_bytes,
                retries: plan.retries,
                cancel: plan.cancel.clone(),
                tx: plan.tx.clone(),
            }));
        }

        let mut batch_metas = Vec::with_capacity(batch);
        while let Some(joined) = tasks.join_next().await {
            batch_metas.push(joined.context("no-range worker panicked")??);
        }
        batch_metas.sort_by_key(|meta| meta.index);
        let saw_eof = batch_metas.iter().any(|meta| meta.eof);
        metas.extend(batch_metas);
        next_index += batch;

        if known_segments.is_none() && saw_eof {
            break;
        }
    }

    Ok(metas)
}

async fn fetch_segment(job: SegmentJob) -> Result<SegmentMeta> {
    let mut last_error = None;
    for attempt in 0..=job.retries {
        match fetch_segment_once(&job).await {
            Ok(meta) => return Ok(meta),
            Err(err) => {
                last_error = Some(err);
                if attempt < job.retries {
                    tokio::select! {
                        _ = job.cancel.cancelled() => {
                            return Err(DownloadCancelled::new(0).into());
                        }
                        _ = tokio::time::sleep(backoff(attempt)) => {}
                    }
                }
            }
        }
    }

    Err(last_error.context("no-range segment failed unexpectedly")?)
}

async fn fetch_segment_once(job: &SegmentJob) -> Result<SegmentMeta> {
    let start = job.index as u64 * job.payload_bytes;
    let capture_limit = job.payload_bytes + job.overlap_bytes;
    let path = segment_path_for(&job.part_dir, job.index);
    remove_file_if_exists(&path).await?;

    let resp = tokio::select! {
        _ = job.cancel.cancelled() => {
            return Err(DownloadCancelled::new(0).into());
        }
        result = apply_request_headers(job.client.get(job.url.clone()), &job.request_headers)
            .header(ACCEPT_ENCODING, "identity")
            .send() => result.context("failed to start no-range stream")?,
    };
    if !resp.status().is_success() {
        bail!("no-range request failed with status {}", resp.status());
    }

    let mut out = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .await
        .with_context(|| format!("failed to create {}", path.display()))?;

    let mut discard_remaining = start;
    let mut capture_remaining = capture_limit;
    let mut captured = 0u64;
    let mut stream = resp.bytes_stream();

    loop {
        let next = tokio::select! {
            _ = job.cancel.cancelled() => {
                out.flush()
                    .await
                    .with_context(|| format!("failed flushing {}", path.display()))?;
                return Err(DownloadCancelled::new(0).into());
            }
            next = stream.next() => next,
        };
        let Some(next) = next else {
            break;
        };
        let bytes = next.context("failed while reading no-range stream")?;
        let mut slice = bytes.as_ref();
        if discard_remaining > 0 {
            let discard = min(discard_remaining, slice.len() as u64) as usize;
            discard_remaining -= discard as u64;
            slice = &slice[discard..];
        }
        if discard_remaining > 0 || slice.is_empty() {
            continue;
        }

        let keep = min(capture_remaining, slice.len() as u64) as usize;
        if keep > 0 {
            out.write_all(&slice[..keep])
                .await
                .with_context(|| format!("failed writing {}", path.display()))?;
            let before = captured;
            captured += keep as u64;
            capture_remaining -= keep as u64;
            let useful_from = if job.index == 0 { 0 } else { job.overlap_bytes };
            let useful_before = before.saturating_sub(useful_from);
            let useful_after = captured.saturating_sub(useful_from);
            let useful = useful_after.saturating_sub(useful_before);
            if useful > 0 {
                let _ = job.tx.send(DownloadEvent::Advanced(useful));
            }
        }
        if capture_remaining == 0 {
            out.flush()
                .await
                .with_context(|| format!("failed flushing {}", path.display()))?;
            return Ok(SegmentMeta {
                index: job.index,
                len: captured,
                eof: false,
            });
        }
    }

    out.flush()
        .await
        .with_context(|| format!("failed flushing {}", path.display()))?;
    Ok(SegmentMeta {
        index: job.index,
        len: captured,
        eof: true,
    })
}
