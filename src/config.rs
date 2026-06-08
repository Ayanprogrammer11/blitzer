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
pub const MAX_URL_BYTES: usize = 8192;
pub const MAX_OUTPUT_CHARS: usize = 4096;

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
    if raw.eq_ignore_ascii_case("auto") {
        return Ok(ConnectionStrategy::Auto);
    }
    if raw.is_empty() {
        bail!("Connections is required; enter 'auto' or {MIN_CONNECTIONS}..={MAX_CONNECTIONS}");
    }

    let connections = raw
        .parse::<usize>()
        .context("Connections must be 'auto' or a valid integer")?;
    if !(MIN_CONNECTIONS..=MAX_CONNECTIONS).contains(&connections) {
        bail!("Connections must be 'auto' or in range {MIN_CONNECTIONS}..={MAX_CONNECTIONS}");
    }
    Ok(ConnectionStrategy::Fixed(connections))
}

pub fn parse_http_url(raw: &str) -> Result<Url> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("URL is required");
    }
    if raw.len() > MAX_URL_BYTES {
        bail!("URL is too long (maximum {MAX_URL_BYTES} bytes)");
    }
    if raw.chars().any(char::is_whitespace) {
        bail!("URL cannot contain whitespace");
    }

    let url = Url::parse(raw).context("URL is invalid")?;
    match url.scheme() {
        "http" | "https" => {}
        scheme => bail!("URL scheme must be http or https, got '{scheme}'"),
    }
    if url.host_str().is_none() {
        bail!("URL must include a host");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("URL credentials are not supported; remove the username/password");
    }
    Ok(url)
}

pub fn parse_retries(raw: &str) -> Result<usize> {
    let retries = parse_usize(raw, "Retries")?;
    if retries > MAX_RETRIES {
        bail!("Retries must be in range 0..={MAX_RETRIES}");
    }
    Ok(retries)
}

pub fn parse_timeout_secs(raw: &str) -> Result<u64> {
    let timeout = parse_u64(raw, "Timeout")?;
    if !(MIN_TIMEOUT..=MAX_TIMEOUT).contains(&timeout) {
        bail!("Timeout must be in range {MIN_TIMEOUT}..={MAX_TIMEOUT} seconds");
    }
    Ok(timeout)
}

pub fn parse_output_path(raw: &str) -> Result<Option<PathBuf>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    if raw.chars().count() > MAX_OUTPUT_CHARS {
        bail!("Output path is too long (maximum {MAX_OUTPUT_CHARS} characters)");
    }
    if raw.chars().any(char::is_control) {
        bail!("Output path cannot contain control characters");
    }

    let output = PathBuf::from(raw);
    let Some(file_name) = output.file_name() else {
        bail!("Output path must name a file");
    };
    if file_name == "." || file_name == ".." {
        bail!("Output path must name a file");
    }
    if output.is_dir() {
        bail!("Output path points to a directory");
    }
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
        && parent.exists()
        && !parent.is_dir()
    {
        bail!("Output parent is not a directory: {}", parent.display());
    }

    Ok(Some(output))
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

fn parse_usize(raw: &str, label: &str) -> Result<usize> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("{label} is required");
    }
    raw.parse::<usize>()
        .with_context(|| format!("{label} must be a valid integer"))
}

fn parse_u64(raw: &str, label: &str) -> Result<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("{label} is required");
    }
    raw.parse::<u64>()
        .with_context(|| format!("{label} must be a valid integer"))
}

#[cfg(test)]
mod tests {
    use super::{
        parse_connection_strategy, parse_http_url, parse_output_path, parse_retries,
        parse_timeout_secs,
    };

    #[test]
    fn http_url_parser_rejects_non_http_schemes() {
        assert!(parse_http_url("https://example.com/file").is_ok());
        assert!(parse_http_url("http://example.com/file").is_ok());

        let err = parse_http_url("ftp://example.com/file").unwrap_err();
        assert!(format!("{err:#}").contains("URL scheme must be http or https"));
    }

    #[test]
    fn validation_rejects_ambiguous_or_unsafe_values() {
        assert!(parse_connection_strategy("a").is_err());
        assert!(parse_connection_strategy("").is_err());
        assert!(parse_connection_strategy("auto").is_ok());
        assert!(parse_connection_strategy("64").is_ok());
        assert!(parse_connection_strategy("65").is_err());

        assert!(parse_http_url("https://user:secret@example.com/file").is_err());
        assert!(parse_http_url("https://example.com/a b").is_err());
        assert!(parse_http_url("https://").is_err());
        assert!(parse_retries("20").is_ok());
        assert!(parse_retries("21").is_err());
        assert!(parse_timeout_secs("4").is_err());
        assert!(parse_timeout_secs("300").is_ok());
        assert!(parse_output_path("/").is_err());
        assert!(parse_output_path("bad\nname.bin").is_err());
        assert!(parse_output_path("safe/file.bin").is_ok());
    }
}
