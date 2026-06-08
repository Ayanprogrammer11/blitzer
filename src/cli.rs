use crate::{
    config::{
        DownloadConfig, default_connection_input, default_no_range_workers, default_overlap_bytes,
        parse_connection_strategy, parse_http_url, parse_no_range_strategy, parse_output_path,
        parse_retries, parse_timeout_secs,
    },
    download::{CancelToken, DownloadEvent, run_download_with_cancel},
    format::{format_bytes, format_rate},
};
use anyhow::{Context, Result, bail};
use std::env;
use tokio::sync::mpsc;

pub async fn run_from_args() -> Result<bool> {
    let mut args = env::args().skip(1).peekable();
    if args.peek().is_none() {
        return Ok(false);
    }

    let mut url = None;
    let mut output = None::<String>;
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
                output = Some(next_value(&mut args, &arg)?);
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

    let url = parse_http_url(url.as_deref().context("Missing URL")?)?;
    let cfg = DownloadConfig {
        url,
        output: parse_output_path(output.as_deref().unwrap_or_default())?,
        connections: parse_connection_strategy(&connections)?,
        retries: parse_retries(&retries.to_string())?,
        timeout_secs: parse_timeout_secs(&timeout_secs.to_string())?,
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
    let cancel = CancelToken::default();
    let task_cancel = cancel.clone();
    let handle = tokio::spawn(async move {
        if let Err(err) = run_download_with_cancel(cfg, tx.clone(), task_cancel).await {
            let _ = tx.send(DownloadEvent::Failed(format!("{err:#}")));
        }
    });

    let mut downloaded = 0u64;
    let mut cancelling = false;
    loop {
        let evt = tokio::select! {
            evt = rx.recv() => evt,
            signal = tokio::signal::ctrl_c(), if !cancelling => {
                signal.context("failed listening for Ctrl+C")?;
                cancelling = true;
                cancel.cancel();
                eprintln!("Interrupt received; cancelling and saving verified resume data...");
                continue;
            }
        };
        let Some(evt) = evt else {
            break;
        };

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
                        .map(format_bytes)
                        .unwrap_or_else(|| "unknown".to_string()),
                    supports_ranges
                );
            }
            DownloadEvent::ResumeOffset(bytes) => {
                downloaded = bytes;
                eprintln!("Resumed {}", format_bytes(downloaded));
            }
            DownloadEvent::PlanSelected {
                strategy,
                workers,
                segments,
                segment_size,
            } => {
                let segments = if segments == 0 {
                    "unknown".to_string()
                } else {
                    segments.to_string()
                };
                eprintln!(
                    "Plan: {strategy} | workers={workers} | segments={segments} | segment~{}",
                    format_bytes(segment_size)
                );
            }
            DownloadEvent::ProgressReset => {
                downloaded = 0;
            }
            DownloadEvent::Advanced(bytes) => {
                downloaded = downloaded.saturating_add(bytes);
            }
            DownloadEvent::Completed(summary) => {
                eprintln!(
                    "Done: {} ({} new, {} avg)",
                    summary.output.display(),
                    format_bytes(summary.newly_transferred),
                    format_rate(
                        summary.newly_transferred as f64 / summary.elapsed.as_secs_f64().max(0.001)
                    )
                );
                handle.await.context("download task failed")?;
                return Ok(());
            }
            DownloadEvent::Cancelled { saved_bytes } => {
                eprintln!(
                    "Cancelled. Saved {} verified bytes for resume.",
                    format_bytes(saved_bytes)
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
