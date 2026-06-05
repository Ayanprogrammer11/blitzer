use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use url::Url;

pub const MIN_CONNECTIONS: usize = 1;
pub const MAX_CONNECTIONS: usize = 64;
pub const MAX_RETRIES: usize = 20;
pub const MIN_TIMEOUT: u64 = 5;
pub const MAX_TIMEOUT: u64 = 300;
pub const MIN_NO_RANGE_WORKERS: usize = 1;
pub const MAX_NO_RANGE_WORKERS: usize = 16;
pub const MIN_OVERLAP_BYTES: u64 = 4096;
pub const MAX_OVERLAP_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStrategy {
    Auto,
    Fixed(usize),
}

impl ConnectionStrategy {
    pub fn label(self) -> String {
        match self {
            Self::Auto => "auto".to_string(),
            Self::Fixed(connections) => connections.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoRangeStrategy {
    Single,
    Overlap { workers: usize, overlap_bytes: u64 },
}

impl NoRangeStrategy {
    pub fn label(self) -> String {
        match self {
            Self::Single => "single".to_string(),
            Self::Overlap { workers, .. } => format!("overlap/{workers}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DownloadConfig {
    pub url: Url,
    pub output: Option<PathBuf>,
    pub connections: ConnectionStrategy,
    pub retries: usize,
    pub timeout_secs: u64,
    pub no_resume: bool,
    pub no_range_strategy: NoRangeStrategy,
}

pub fn default_connection_input() -> String {
    "auto".to_string()
}

pub fn default_no_range_workers() -> usize {
    4
}

pub fn default_overlap_bytes() -> u64 {
    64 * 1024
}

pub fn parse_connection_strategy(raw: &str) -> Result<ConnectionStrategy> {
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("auto") || raw.eq_ignore_ascii_case("a") {
        return Ok(ConnectionStrategy::Auto);
    }

    let connections = raw
        .parse::<usize>()
        .context("Connections must be 'auto' or a valid integer")?;
    if !(MIN_CONNECTIONS..=MAX_CONNECTIONS).contains(&connections) {
        bail!("Connections must be 'auto' or in range {MIN_CONNECTIONS}..={MAX_CONNECTIONS}");
    }
    Ok(ConnectionStrategy::Fixed(connections))
}

pub fn parse_no_range_strategy(
    raw: &str,
    workers: usize,
    overlap_bytes: u64,
) -> Result<NoRangeStrategy> {
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("single") {
        return Ok(NoRangeStrategy::Single);
    }
    if raw.eq_ignore_ascii_case("overlap") || raw.eq_ignore_ascii_case("speculative") {
        if !(MIN_NO_RANGE_WORKERS..=MAX_NO_RANGE_WORKERS).contains(&workers) {
            bail!(
                "No-range workers must be in range {MIN_NO_RANGE_WORKERS}..={MAX_NO_RANGE_WORKERS}"
            );
        }
        if !(MIN_OVERLAP_BYTES..=MAX_OVERLAP_BYTES).contains(&overlap_bytes) {
            bail!("Overlap bytes must be in range {MIN_OVERLAP_BYTES}..={MAX_OVERLAP_BYTES}");
        }
        return Ok(NoRangeStrategy::Overlap {
            workers,
            overlap_bytes,
        });
    }

    bail!("No-range strategy must be 'single' or 'overlap'")
}

pub fn default_connections() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (cpus * 2).clamp(4, 16)
}
