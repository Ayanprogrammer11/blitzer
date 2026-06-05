mod fetch;
mod merge;

#[cfg(test)]
mod tests;

use super::{
    CancelToken, DownloadEvent,
    http::RequestHeaders,
    parts::{ensure_parent_dir, path_exists, remove_file_if_exists},
};
use crate::config::NoRangeStrategy;
use anyhow::{Context, Result, bail};
use fetch::{FetchPlan, fetch_segments};
use merge::merge_overlap_segments;
use reqwest::{Client, Url};
use std::path::{Path, PathBuf};
use tokio::{fs, sync::mpsc};

const PAYLOAD_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
struct SegmentMeta {
    index: usize,
    len: u64,
    eof: bool,
}

pub(super) struct NoRangeDownload<'a> {
    pub(super) client: &'a Client,
    pub(super) url: &'a Url,
    pub(super) output: &'a Path,
    pub(super) total_size: Option<u64>,
    pub(super) request_headers: &'a RequestHeaders,
    pub(super) strategy: NoRangeStrategy,
    pub(super) retries: usize,
    pub(super) cancel: CancelToken,
    pub(super) tx: mpsc::UnboundedSender<DownloadEvent>,
}

pub(super) async fn download_no_range_overlap(plan: NoRangeDownload<'_>) -> Result<u64> {
    let NoRangeDownload {
        client,
        url,
        output,
        total_size,
        request_headers,
        strategy,
        retries,
        cancel,
        tx,
    } = plan;
    let NoRangeStrategy::Overlap {
        workers,
        overlap_bytes,
    } = strategy
    else {
        bail!("invalid no-range overlap strategy");
    };

    let part_dir = no_range_part_dir_for(output)?;
    if path_exists(&part_dir).await? {
        fs::remove_dir_all(&part_dir)
            .await
            .with_context(|| format!("failed to reset {}", part_dir.display()))?;
    }
    fs::create_dir_all(&part_dir)
        .await
        .with_context(|| format!("failed to create {}", part_dir.display()))?;
    ensure_parent_dir(output).await?;

    let _ = tx.send(DownloadEvent::Phase(format!(
        "No-range overlap strategy: {workers} workers, {} payload, {} overlap",
        human_mib(PAYLOAD_BYTES),
        human_mib(overlap_bytes)
    )));
    let _ = tx.send(DownloadEvent::PlanSelected {
        strategy: "no-range overlap".to_string(),
        workers,
        segments: total_size
            .map(|size| segment_count(size, PAYLOAD_BYTES))
            .unwrap_or(0),
        segment_size: PAYLOAD_BYTES,
    });

    let result = async {
        let metas = fetch_segments(FetchPlan {
            client: client.clone(),
            url: url.clone(),
            request_headers: request_headers.clone(),
            part_dir: part_dir.clone(),
            total_size,
            workers,
            payload_bytes: PAYLOAD_BYTES,
            overlap_bytes,
            retries,
            cancel,
            tx: tx.clone(),
        })
        .await?;

        let final_size =
            merge_overlap_segments(&part_dir, output, &metas, PAYLOAD_BYTES, overlap_bytes).await?;
        if let Some(expected) = total_size
            && final_size != expected
        {
            bail!("no-range merge size mismatch: got {final_size}, expected {expected}");
        }

        Ok(final_size)
    }
    .await;

    match result {
        Ok(final_size) => {
            fs::remove_dir_all(&part_dir)
                .await
                .with_context(|| format!("failed to cleanup {}", part_dir.display()))?;
            Ok(final_size)
        }
        Err(err) => {
            let _ = fs::remove_dir_all(&part_dir).await;
            let _ = remove_file_if_exists(&temp_output_path(output)).await;
            Err(err)
        }
    }
}

fn segment_count(total_size: u64, payload_bytes: u64) -> usize {
    total_size.div_ceil(payload_bytes).max(1) as usize
}

fn no_range_part_dir_for(output: &Path) -> Result<PathBuf> {
    let file = output.file_name().context("invalid output filename")?;
    Ok(output
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{}.norange.parts", file.to_string_lossy())))
}

fn segment_path_for(dir: &Path, index: usize) -> PathBuf {
    dir.join(format!("segment-{index:06}.bin"))
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

fn human_mib(bytes: u64) -> String {
    format!("{:.2} MiB", bytes as f64 / (1024.0 * 1024.0))
}
