use crate::{
    config::{
        DownloadConfig, MAX_RETRIES, MAX_TIMEOUT, MIN_TIMEOUT, default_connection_input,
        default_no_range_workers, default_overlap_bytes, parse_connection_strategy,
        parse_no_range_strategy,
    },
    download::{DownloadEvent, run_download},
};
use anyhow::{Context, Result, bail};
use indicatif::HumanBytes;
use std::{env, path::PathBuf};
use tokio::sync::mpsc;
use url::Url;

pub async fn run_from_args() -> Result<bool> {
    let mut args = env::args().skip(1).peekable();
    if args.peek().is_none() {
        return Ok(false);
    }

    let mut url = None;
    let mut output = None;
    let mut connections = default_connection_input();
    let mut retries = 4usize;
    let mut timeout_secs = 30u64;
    let mut no_resume = false;
    let mut no_range_strategy = "overlap".to_string();
    let mut no_range_workers = default_no_range_workers();
    let mut overlap_bytes = default_overlap_bytes();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_usage();
                return Ok(true);
            }
            "-o" | "--output" => {
                output = Some(PathBuf::from(next_value(&mut args, &arg)?));
            }
            "-c" | "--connections" => {
                connections = next_value(&mut args, &arg)?;
            }
            "--retries" => {
                retries = next_value(&mut args, &arg)?
                    .parse()
                    .context("Retries must be a valid integer")?;
            }
            "--timeout" => {
                timeout_secs = next_value(&mut args, &arg)?
                    .parse()
                    .context("Timeout must be a valid integer")?;
            }
            "--no-resume" | "--fresh" => no_resume = true,
            "--no-range-strategy" => {
                no_range_strategy = next_value(&mut args, &arg)?;
            }
            "--no-range-workers" => {
                no_range_workers = next_value(&mut args, &arg)?
                    .parse()
                    .context("No-range workers must be a valid integer")?;
            }
            "--overlap" | "--overlap-bytes" => {
                overlap_bytes = next_value(&mut args, &arg)?
                    .parse()
                    .context("Overlap bytes must be a valid integer")?;
            }
            value if value.starts_with('-') => bail!("Unknown option: {value}"),
            value => {
                if url.replace(value.to_string()).is_some() {
                    bail!("Only one URL may be provided");
                }
            }
        }
    }

    if retries > MAX_RETRIES {
        bail!("Retries must be <= {MAX_RETRIES}");
    }
    if !(MIN_TIMEOUT..=MAX_TIMEOUT).contains(&timeout_secs) {
        bail!("Timeout must be in range {MIN_TIMEOUT}..={MAX_TIMEOUT} seconds");
    }

    let url = Url::parse(url.as_deref().context("Missing URL")?).context("URL is invalid")?;
    let cfg = DownloadConfig {
        url,
        output,
        connections: parse_connection_strategy(&connections)?,
        retries,
        timeout_secs,
        no_resume,
        no_range_strategy: parse_no_range_strategy(
            &no_range_strategy,
            no_range_workers,
            overlap_bytes,
        )?,
    };

    run_cli_download(cfg).await?;
    Ok(true)
}

async fn run_cli_download(cfg: DownloadConfig) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<DownloadEvent>();
    let handle = tokio::spawn(async move {
        if let Err(err) = run_download(cfg, tx.clone()).await {
            let _ = tx.send(DownloadEvent::Failed(format!("{err:#}")));
        }
    });

    let mut downloaded = 0u64;
    while let Some(evt) = rx.recv().await {
        match evt {
            DownloadEvent::Phase(message) => eprintln!("{message}"),
            DownloadEvent::TargetResolved {
                output,
                total_size,
                supports_ranges,
            } => {
                eprintln!(
                    "Output: {} | Size: {} | Ranges: {}",
                    output.display(),
                    total_size
                        .map(HumanBytes)
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "unknown".to_string()),
                    supports_ranges
                );
            }
            DownloadEvent::ResumeOffset(bytes) => {
                downloaded = bytes;
                eprintln!("Resumed {}", HumanBytes(downloaded));
            }
            DownloadEvent::PlanSelected {
                strategy,
                workers,
                segments,
                segment_size,
            } => {
                eprintln!(
                    "Plan: {strategy} | workers={workers} | segments={segments} | segment~{}",
                    HumanBytes(segment_size)
                );
            }
            DownloadEvent::Advanced(bytes) => {
                downloaded = downloaded.saturating_add(bytes);
            }
            DownloadEvent::Completed(summary) => {
                eprintln!(
                    "Done: {} ({} new, {:.2} MiB/s avg)",
                    summary.output.display(),
                    HumanBytes(summary.newly_transferred),
                    summary.avg_mib_per_sec
                );
                handle.await.context("download task failed")?;
                return Ok(());
            }
            DownloadEvent::Cancelled { saved_bytes } => {
                eprintln!(
                    "Cancelled. Saved {} verified bytes for resume.",
                    HumanBytes(saved_bytes)
                );
                handle.await.context("download task failed")?;
                return Ok(());
            }
            DownloadEvent::Failed(err) => {
                handle.abort();
                bail!("{err}");
            }
        }
    }

    handle.await.context("download task failed")?;
    Ok(())
}

fn next_value(args: &mut impl Iterator<Item = String>, option: &str) -> Result<String> {
    args.next()
        .with_context(|| format!("{option} requires a value"))
}

fn print_usage() {
    println!(
        "Usage: blitzer <URL> [--output PATH] [--connections auto|N] [--retries N] [--timeout SECONDS] [--no-resume] [--no-range-strategy single|overlap] [--no-range-workers N] [--overlap-bytes N]"
    );
}
