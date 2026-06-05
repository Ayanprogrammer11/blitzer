use anyhow::{Context, Result, bail};
use reqwest::{
    Client, RequestBuilder, StatusCode, Url,
    header::{
        ACCEPT, ACCEPT_ENCODING, ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE,
        CONTENT_TYPE, ETAG, HeaderMap, HeaderValue, LAST_MODIFIED, RANGE, REFERER,
    },
};
use std::path::PathBuf;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct RequestHeaders {
    pub(super) referer: Option<String>,
    pub(super) xhr: bool,
}

#[derive(Debug, Default, Clone)]
pub(super) struct RemoteInfo {
    pub(super) size: Option<u64>,
    pub(super) supports_ranges: bool,
    pub(super) suggested_filename: Option<String>,
    pub(super) etag: Option<String>,
    pub(super) last_modified: Option<String>,
    pub(super) content_type: Option<String>,
    pub(super) request_headers: RequestHeaders,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContentRange {
    start: u64,
    end: u64,
    total: Option<u64>,
}

pub(super) async fn probe_remote(client: &Client, url: &Url) -> Result<RemoteInfo> {
    let mut info = RemoteInfo::default();
    let mut head_succeeded = false;

    if let Ok(resp) = client.head(url.clone()).send().await
        && resp.status().is_success()
    {
        head_succeeded = true;
        capture_common_headers(&mut info, resp.headers());
        info.supports_ranges = header_accepts_ranges(resp.headers().get(ACCEPT_RANGES));
    }

    let probe = client
        .get(url.clone())
        .header(RANGE, "bytes=0-0")
        .header(ACCEPT_ENCODING, "identity")
        .send()
        .await;

    match probe {
        Ok(resp) if resp.status() == StatusCode::PARTIAL_CONTENT => {
            let final_url = resp.url().clone();
            capture_common_headers(&mut info, resp.headers());
            info.supports_ranges = true;
            if let Some(total) = parse_total_from_content_range(resp.headers().get(CONTENT_RANGE)) {
                info.size = Some(total);
            }
            if let Some(gated) =
                probe_referer_gated_download(client, url, &final_url, &info).await?
            {
                return Ok(gated);
            }
        }
        Ok(resp) if resp.status().is_success() => {
            let final_url = resp.url().clone();
            capture_common_headers(&mut info, resp.headers());
            info.supports_ranges = false;
            if let Some(gated) =
                probe_referer_gated_download(client, url, &final_url, &info).await?
            {
                return Ok(gated);
            }
        }
        Ok(resp) if head_succeeded => {
            info.supports_ranges = false;
            let _ = resp;
        }
        Ok(resp) => bail!("failed to probe remote server: status {}", resp.status()),
        Err(err) if head_succeeded => {
            info.supports_ranges = false;
            let _ = err;
        }
        Err(err) => return Err(err).context("failed to probe range support"),
    }

    Ok(info)
}

pub(super) fn apply_request_headers(
    request: RequestBuilder,
    headers: &RequestHeaders,
) -> RequestBuilder {
    let request = request.header(ACCEPT, "*/*");
    let request = if let Some(referer) = &headers.referer {
        request.header(REFERER, referer)
    } else {
        request
    };
    if headers.xhr {
        request.header("X-Requested-With", "XMLHttpRequest")
    } else {
        request
    }
}

pub(super) fn resolve_output_path(
    url: &Url,
    manual_output: Option<PathBuf>,
    suggested: Option<String>,
) -> PathBuf {
    if let Some(output) = manual_output {
        return output;
    }
    if let Some(name) = suggested.and_then(|n| sanitize_filename(&n)) {
        return PathBuf::from(name);
    }

    let inferred = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .and_then(|segment| sanitize_filename(&percent_decode(segment)))
        .unwrap_or_else(|| "download.bin".to_string());
    PathBuf::from(inferred)
}

pub(super) fn validate_content_range(
    value: Option<&HeaderValue>,
    expected_start: u64,
    expected_end: u64,
) -> Result<()> {
    let range = parse_content_range(value).context("range response is missing Content-Range")?;
    if range.start != expected_start || range.end != expected_end {
        bail!(
            "server returned byte range {}-{}, expected {}-{}",
            range.start,
            range.end,
            expected_start,
            expected_end
        );
    }
    Ok(())
}

fn capture_common_headers(info: &mut RemoteInfo, headers: &HeaderMap) {
    if info.size.is_none() {
        info.size = parse_content_length(headers.get(CONTENT_LENGTH));
    }
    if info.suggested_filename.is_none() {
        info.suggested_filename = parse_filename_from_disposition(
            headers
                .get(CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok()),
        );
    }
    if info.etag.is_none() {
        info.etag = header_to_string(headers.get(ETAG));
    }
    if info.last_modified.is_none() {
        info.last_modified = header_to_string(headers.get(LAST_MODIFIED));
    }
    if let Some(content_type) = header_to_string(headers.get(CONTENT_TYPE)) {
        info.content_type = Some(content_type);
    }
}

async fn probe_referer_gated_download(
    client: &Client,
    original_url: &Url,
    final_url: &Url,
    current: &RemoteInfo,
) -> Result<Option<RemoteInfo>> {
    if original_url == final_url || !is_html_content_type(current.content_type.as_deref()) {
        return Ok(None);
    }

    let request_headers = RequestHeaders {
        referer: Some(final_url.to_string()),
        xhr: true,
    };
    let resp = apply_request_headers(client.get(original_url.clone()), &request_headers)
        .header(RANGE, "bytes=0-0")
        .header(ACCEPT_ENCODING, "identity")
        .send()
        .await
        .context("failed to probe referer-gated download")?;

    if resp.status() != StatusCode::PARTIAL_CONTENT
        || is_html_header(resp.headers().get(CONTENT_TYPE))
    {
        return Ok(None);
    }

    let mut info = RemoteInfo {
        supports_ranges: true,
        request_headers,
        ..RemoteInfo::default()
    };
    capture_common_headers(&mut info, resp.headers());
    if let Some(total) = parse_total_from_content_range(resp.headers().get(CONTENT_RANGE)) {
        info.size = Some(total);
    }
    Ok(Some(info))
}

fn header_accepts_ranges(value: Option<&HeaderValue>) -> bool {
    value
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("bytes") || v.to_ascii_lowercase().contains("bytes"))
        .unwrap_or(false)
}

fn header_to_string(value: Option<&HeaderValue>) -> Option<String> {
    value
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
}

fn is_html_header(value: Option<&HeaderValue>) -> bool {
    value
        .and_then(|v| v.to_str().ok())
        .map(|v| is_html_content_type(Some(v)))
        .unwrap_or(false)
}

fn is_html_content_type(value: Option<&str>) -> bool {
    value
        .map(|v| v.to_ascii_lowercase().starts_with("text/html"))
        .unwrap_or(false)
}

fn parse_content_length(value: Option<&HeaderValue>) -> Option<u64> {
    value
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

fn parse_total_from_content_range(value: Option<&HeaderValue>) -> Option<u64> {
    parse_content_range(value).and_then(|range| range.total)
}

fn parse_content_range(value: Option<&HeaderValue>) -> Option<ContentRange> {
    let raw = value.and_then(|v| v.to_str().ok())?.trim();
    let raw = raw.strip_prefix("bytes ")?;
    let (range, total_raw) = raw.split_once('/')?;
    let (start_raw, end_raw) = range.split_once('-')?;
    let total = if total_raw.trim() == "*" {
        None
    } else {
        Some(total_raw.trim().parse::<u64>().ok()?)
    };

    Some(ContentRange {
        start: start_raw.trim().parse::<u64>().ok()?,
        end: end_raw.trim().parse::<u64>().ok()?,
        total,
    })
}

fn parse_filename_from_disposition(content_disposition: Option<&str>) -> Option<String> {
    let raw = content_disposition?;
    let mut plain = None;

    for segment in raw.split(';').skip(1) {
        let segment = segment.trim();
        let Some((key, value)) = segment.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().trim_matches('"');
        if key == "filename*" {
            if let Some(decoded) =
                decode_rfc5987_filename(value).and_then(|v| sanitize_filename(&v))
            {
                return Some(decoded);
            }
        } else if key == "filename" {
            plain = sanitize_filename(value);
        }
    }

    plain
}

fn decode_rfc5987_filename(value: &str) -> Option<String> {
    let (_, encoded) = value.split_once("''")?;
    Some(percent_decode(encoded))
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'%'
            && idx + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_value(bytes[idx + 1]), hex_value(bytes[idx + 2]))
        {
            decoded.push((hi << 4) | lo);
            idx += 3;
            continue;
        }
        decoded.push(bytes[idx]);
        idx += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn sanitize_filename(raw: &str) -> Option<String> {
    let raw = raw.trim().trim_matches('"').trim();
    let leaf = raw.rsplit(['/', '\\']).next().unwrap_or(raw).trim();
    let cleaned: String = leaf
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        None
    } else {
        Some(cleaned.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_filename_is_sanitized() {
        let name = parse_filename_from_disposition(Some(
            "attachment; filename=\"../../evil.bin\"; filename*=UTF-8''safe%20name.bin",
        ));
        assert_eq!(name.as_deref(), Some("safe name.bin"));
    }

    #[test]
    fn parses_content_range_total() {
        let value = HeaderValue::from_static("bytes 10-19/200");
        assert_eq!(parse_total_from_content_range(Some(&value)), Some(200));
        assert_eq!(
            parse_content_range(Some(&value)),
            Some(ContentRange {
                start: 10,
                end: 19,
                total: Some(200),
            })
        );
    }
}
