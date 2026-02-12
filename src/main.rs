use anyhow::{Context, Result, bail};
use clap::Parser;
use futures_util::StreamExt;
use indicatif::{HumanBytes, ProgressBar, ProgressStyle};
use reqwest::{
    Client, StatusCode, Url,
    header::{
        ACCEPT_ENCODING, ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, RANGE,
    },
};
use std::{
    cmp::min,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    fs::{self, OpenOptions},
    io::{self, AsyncWriteExt},
    task::JoinSet,
    time::sleep,
};

#[derive(Parser, Debug)]
#[command(
    name = "blitzer",
    about = "Fast multi-connection file downloader with resume support"
)]
struct Cli {
    /// Target URL to download
    url: String,
    /// Output file path (defaults to filename from URL or headers)
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Number of parallel connections
    #[arg(
        short = 'n',
        long,
        default_value_t = default_connections()
    )]
    connections: usize,
    /// Retry attempts per chunk
    #[arg(long, default_value_t = 4)]
    retries: usize,
    /// Request timeout in seconds
    #[arg(long, default_value_t = 30)]
    timeout_secs: u64,
    /// Disable resume behavior
    #[arg(long)]
    no_resume: bool,
}

#[derive(Debug, Clone, Copy)]
struct Chunk {
    index: usize,
    start: u64,
    end: u64,
}

#[derive(Debug, Default)]
struct RemoteInfo {
    size: Option<u64>,
    supports_ranges: bool,
    suggested_filename: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.connections == 0 || cli.connections > 64 {
        bail!("--connections must be in the range 1..=64");
    }
    if cli.retries > 20 {
        bail!("--retries must be <= 20");
    }
    if !(5..=300).contains(&cli.timeout_secs) {
        bail!("--timeout-secs must be in the range 5..=300");
    }

    let url = Url::parse(&cli.url).context("invalid URL")?;
    let client = Client::builder()
        .user_agent("blitzer/0.1.0")
        .timeout(Duration::from_secs(cli.timeout_secs))
        .build()
        .context("failed to build HTTP client")?;

    let remote = probe_remote(&client, &url).await?;
    let output = resolve_output_path(&url, cli.output, remote.suggested_filename);
    ensure_parent_dir(&output).await?;

    let start_time = Instant::now();
    let transferred = if let (Some(total), true) = (remote.size, remote.supports_ranges) {
        download_parallel(
            &client,
            &url,
            &output,
            total,
            cli.connections,
            cli.retries,
            cli.no_resume,
        )
        .await?
    } else {
        download_single(&client, &url, &output).await?
    };

    let elapsed = start_time.elapsed().as_secs_f64().max(0.001);
    let rate = transferred as f64 / elapsed;
    println!(
        "Saved to {} ({} transferred, {:.2} MiB/s)",
        output.display(),
        HumanBytes(transferred),
        rate / (1024.0 * 1024.0)
    );
    Ok(())
}

async fn probe_remote(client: &Client, url: &Url) -> Result<RemoteInfo> {
    let mut info = RemoteInfo::default();

    if let Ok(resp) = client.head(url.clone()).send().await {
        if resp.status().is_success() {
            info.size = parse_content_length(resp.headers().get(CONTENT_LENGTH));
            info.supports_ranges = resp
                .headers()
                .get(ACCEPT_RANGES)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.to_ascii_lowercase().contains("bytes"))
                .unwrap_or(false);
            info.suggested_filename = parse_filename_from_disposition(
                resp.headers()
                    .get(CONTENT_DISPOSITION)
                    .and_then(|v| v.to_str().ok()),
            );
        }
    }

    if info.size.is_none() || !info.supports_ranges {
        let resp = client
            .get(url.clone())
            .header(RANGE, "bytes=0-0")
            .header(ACCEPT_ENCODING, "identity")
            .send()
            .await
            .context("failed to probe range support")?;

        if resp.status() == StatusCode::PARTIAL_CONTENT {
            info.supports_ranges = true;
            if info.size.is_none() {
                info.size = parse_total_from_content_range(resp.headers().get(CONTENT_RANGE));
            }
        }

        if info.size.is_none() {
            info.size = parse_content_length(resp.headers().get(CONTENT_LENGTH));
        }

        if info.suggested_filename.is_none() {
            info.suggested_filename = parse_filename_from_disposition(
                resp.headers()
                    .get(CONTENT_DISPOSITION)
                    .and_then(|v| v.to_str().ok()),
            );
        }
    }

    Ok(info)
}

async fn download_single(client: &Client, url: &Url, output: &Path) -> Result<u64> {
    let resp = client
        .get(url.clone())
        .header(ACCEPT_ENCODING, "identity")
        .send()
        .await
        .context("failed to start download")?;
    if !resp.status().is_success() {
        bail!("download request failed with status {}", resp.status());
    }

    let total = parse_content_length(resp.headers().get(CONTENT_LENGTH)).unwrap_or(0);
    let pb = make_progress_bar(if total > 0 { Some(total) } else { None });
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(output)
        .await
        .with_context(|| format!("failed to create {}", output.display()))?;

    let mut transferred = 0u64;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("failed while reading response body")?;
        file.write_all(&chunk)
            .await
            .context("failed writing file")?;
        let len = chunk.len() as u64;
        transferred += len;
        pb.inc(len);
    }
    file.flush().await?;
    pb.finish_and_clear();
    Ok(transferred)
}

async fn download_parallel(
    client: &Client,
    url: &Url,
    output: &Path,
    total_size: u64,
    connections: usize,
    retries: usize,
    no_resume: bool,
) -> Result<u64> {
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

    let chunks = build_chunks(total_size, connections);
    let part_dir = part_dir_for(output)?;
    if no_resume && part_dir.exists() {
        fs::remove_dir_all(&part_dir)
            .await
            .with_context(|| format!("failed to remove {}", part_dir.display()))?;
    }
    fs::create_dir_all(&part_dir)
        .await
        .with_context(|| format!("failed to create {}", part_dir.display()))?;

    let mut already_downloaded = 0u64;
    if !no_resume {
        for chunk in &chunks {
            let part_path = part_path_for(&part_dir, chunk.index);
            if let Ok(meta) = fs::metadata(&part_path).await {
                let expected = chunk_len(*chunk);
                already_downloaded += min(meta.len(), expected);
            }
        }
    }

    let pb = make_progress_bar(Some(total_size));
    pb.set_position(already_downloaded);
    let transferred = Arc::new(AtomicU64::new(0));

    let mut set = JoinSet::new();
    for chunk in chunks.iter().copied() {
        let client = client.clone();
        let url = url.clone();
        let part_path = part_path_for(&part_dir, chunk.index);
        let transferred = transferred.clone();
        let pb = pb.clone();
        set.spawn(async move {
            download_chunk(
                client,
                url,
                chunk,
                part_path,
                retries,
                no_resume,
                transferred,
                pb,
            )
            .await
        });
    }

    while let Some(joined) = set.join_next().await {
        joined.context("a worker task panicked")??;
    }
    pb.finish_and_clear();

    merge_parts(&part_dir, output, &chunks, total_size).await?;
    fs::remove_dir_all(&part_dir)
        .await
        .with_context(|| format!("failed to cleanup {}", part_dir.display()))?;
    Ok(transferred.load(Ordering::Relaxed))
}

async fn download_chunk(
    client: Client,
    url: Url,
    chunk: Chunk,
    part_path: PathBuf,
    retries: usize,
    no_resume: bool,
    transferred: Arc<AtomicU64>,
    pb: ProgressBar,
) -> Result<()> {
    let expected_len = chunk_len(chunk);
    if no_resume && part_path.exists() {
        fs::remove_file(&part_path)
            .await
            .with_context(|| format!("failed removing {}", part_path.display()))?;
    }

    let mut local_len = match fs::metadata(&part_path).await {
        Ok(meta) => meta.len(),
        Err(_) => 0,
    };
    if local_len > expected_len {
        local_len = 0;
        fs::remove_file(&part_path)
            .await
            .with_context(|| format!("failed truncating {}", part_path.display()))?;
    }
    if local_len == expected_len {
        return Ok(());
    }

    for attempt in 0..=retries {
        let range_start = chunk.start + local_len;
        let range_header = format!("bytes={}-{}", range_start, chunk.end);
        let resp = client
            .get(url.clone())
            .header(RANGE, range_header)
            .header(ACCEPT_ENCODING, "identity")
            .send()
            .await;

        match resp {
            Ok(resp) => {
                if resp.status() != StatusCode::PARTIAL_CONTENT {
                    bail!(
                        "server does not honor range requests (status {})",
                        resp.status()
                    );
                }

                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&part_path)
                    .await
                    .with_context(|| format!("failed to open {}", part_path.display()))?;

                let mut stream = resp.bytes_stream();
                let mut stream_error = None;
                while let Some(next) = stream.next().await {
                    match next {
                        Ok(buf) => {
                            file.write_all(&buf).await.with_context(|| {
                                format!("failed writing chunk {}", part_path.display())
                            })?;
                            let bytes = buf.len() as u64;
                            local_len += bytes;
                            transferred.fetch_add(bytes, Ordering::Relaxed);
                            pb.inc(bytes);
                            if local_len > expected_len {
                                bail!("chunk {} exceeded expected size", chunk.index);
                            }
                        }
                        Err(e) => {
                            stream_error = Some(e);
                            break;
                        }
                    }
                }

                if let Some(e) = stream_error {
                    if attempt >= retries {
                        return Err(e).context("stream failed after retries");
                    }
                    sleep(backoff(attempt)).await;
                    continue;
                }

                file.flush().await?;
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
        sleep(backoff(attempt)).await;
    }

    bail!("chunk {} failed unexpectedly", chunk.index)
}

async fn merge_parts(
    part_dir: &Path,
    output: &Path,
    chunks: &[Chunk],
    total_size: u64,
) -> Result<()> {
    let tmp_name = format!(
        ".{}.blitzer.tmp",
        output
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("download")
    );
    let tmp_output = output
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(tmp_name);
    if tmp_output.exists() {
        fs::remove_file(&tmp_output).await?;
    }

    let mut out = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp_output)
        .await
        .with_context(|| format!("failed to create {}", tmp_output.display()))?;

    for chunk in chunks {
        let path = part_path_for(part_dir, chunk.index);
        let mut in_file = OpenOptions::new()
            .read(true)
            .open(&path)
            .await
            .with_context(|| format!("missing part {}", path.display()))?;
        io::copy(&mut in_file, &mut out)
            .await
            .with_context(|| format!("failed merging {}", path.display()))?;
    }
    out.flush().await?;

    let final_meta = fs::metadata(&tmp_output).await?;
    if final_meta.len() != total_size {
        bail!(
            "merged file size mismatch: got {}, expected {}",
            final_meta.len(),
            total_size
        );
    }

    if output.exists() {
        fs::remove_file(output).await?;
    }
    fs::rename(&tmp_output, output).await?;
    Ok(())
}

fn resolve_output_path(
    url: &Url,
    cli_output: Option<PathBuf>,
    suggested: Option<String>,
) -> PathBuf {
    if let Some(path) = cli_output {
        return path;
    }
    if let Some(name) = suggested.filter(|n| !n.trim().is_empty()) {
        return PathBuf::from(name);
    }

    let inferred = url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .filter(|s| !s.is_empty())
        .unwrap_or("download.bin");
    PathBuf::from(inferred)
}

async fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
    }
    Ok(())
}

fn part_dir_for(output: &Path) -> Result<PathBuf> {
    let file = output
        .file_name()
        .and_then(|v| v.to_str())
        .context("invalid output filename")?;
    Ok(output
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{}.parts", file)))
}

fn part_path_for(dir: &Path, index: usize) -> PathBuf {
    dir.join(format!("part-{index:04}.bin"))
}

fn build_chunks(total_size: u64, requested_connections: usize) -> Vec<Chunk> {
    let chunk_count = min(requested_connections.max(1), total_size as usize);
    let base = total_size / chunk_count as u64;
    let remainder = total_size % chunk_count as u64;

    let mut chunks = Vec::with_capacity(chunk_count);
    let mut start = 0u64;
    for idx in 0..chunk_count {
        let extra = if idx < remainder as usize { 1 } else { 0 };
        let len = base + extra;
        let end = start + len - 1;
        chunks.push(Chunk {
            index: idx,
            start,
            end,
        });
        start = end + 1;
    }
    chunks
}

fn chunk_len(chunk: Chunk) -> u64 {
    chunk.end - chunk.start + 1
}

fn parse_content_length(value: Option<&reqwest::header::HeaderValue>) -> Option<u64> {
    value
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

fn parse_total_from_content_range(value: Option<&reqwest::header::HeaderValue>) -> Option<u64> {
    let raw = value.and_then(|v| v.to_str().ok())?;
    let (_, total) = raw.split_once('/')?;
    total.trim().parse::<u64>().ok()
}

fn parse_filename_from_disposition(content_disposition: Option<&str>) -> Option<String> {
    let raw = content_disposition?;
    for segment in raw.split(';') {
        let segment = segment.trim();
        if let Some(name) = segment.strip_prefix("filename=") {
            return Some(name.trim_matches('"').to_string());
        }
    }
    None
}

fn make_progress_bar(total: Option<u64>) -> ProgressBar {
    match total {
        Some(t) if t > 0 => {
            let pb = ProgressBar::new(t);
            pb.set_style(
                ProgressStyle::with_template(
                    "{spinner:.green} {bytes}/{total_bytes} [{bar:40.cyan/blue}] {eta} @ {bytes_per_sec}",
                )
                .unwrap_or_else(|_| ProgressStyle::default_bar()),
            );
            pb
        }
        _ => {
            let pb = ProgressBar::new_spinner();
            pb.set_style(
                ProgressStyle::with_template("{spinner:.green} {bytes} @ {bytes_per_sec}")
                    .unwrap_or_else(|_| ProgressStyle::default_spinner()),
            );
            pb.enable_steady_tick(Duration::from_millis(100));
            pb
        }
    }
}

fn backoff(attempt: usize) -> Duration {
    let capped = min(attempt as u32, 6);
    Duration::from_millis(250 * (1u64 << capped))
}

fn default_connections() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (cpus * 2).clamp(4, 16)
}
