use super::http::RemoteInfo;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::cmp::min;

pub(super) const RESUME_MANIFEST_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Chunk {
    pub(super) index: usize,
    pub(super) start: u64,
    pub(super) end: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ResumeManifest {
    version: u8,
    url: String,
    total_size: u64,
    chunks: Vec<Chunk>,
    etag: Option<String>,
    last_modified: Option<String>,
}

impl ResumeManifest {
    pub(super) fn new(url: &Url, remote: &RemoteInfo, chunks: &[Chunk], total_size: u64) -> Self {
        Self {
            version: RESUME_MANIFEST_VERSION,
            url: url.to_string(),
            total_size,
            chunks: chunks.to_vec(),
            etag: remote.etag.clone(),
            last_modified: remote.last_modified.clone(),
        }
    }

    pub(super) fn is_compatible_with(&self, expected: &Self) -> bool {
        self == expected
    }
}

pub(super) fn build_chunks(total_size: u64, requested_connections: usize) -> Vec<Chunk> {
    let chunk_count = min(requested_connections.max(1) as u64, total_size) as usize;
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

pub(super) fn chunk_len(chunk: Chunk) -> u64 {
    chunk.end - chunk.start + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_rejects_changed_chunk_layout() {
        let remote = RemoteInfo {
            size: Some(100),
            supports_ranges: true,
            suggested_filename: None,
            etag: Some("\"fixture\"".to_string()),
            last_modified: None,
            content_type: None,
            request_headers: Default::default(),
        };
        let url = Url::parse("http://example.test/file.bin").unwrap();
        let four_chunks = build_chunks(100, 4);
        let two_chunks = build_chunks(100, 2);

        let existing = ResumeManifest::new(&url, &remote, &four_chunks, 100);
        let expected = ResumeManifest::new(&url, &remote, &two_chunks, 100);

        assert!(!existing.is_compatible_with(&expected));
    }
}
