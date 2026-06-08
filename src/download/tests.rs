use crate::config::{ConnectionStrategy, NoRangeStrategy};

use super::{
    http::{RemoteInfo, RequestHeaders},
    manifest::{ResumeManifest, build_chunks, chunk_len},
    parts::{
        compute_resume_offset, part_dir_for, part_path_for, path_exists, write_resume_manifest,
    },
    *,
};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};
use tempfile::TempDir;
use tokio::{fs, sync::mpsc};
use url::Url;

#[tokio::test]
async fn stale_parts_from_different_chunk_layout_are_not_merged() {
    let content: Vec<u8> = (0..100u8).collect();
    let (url, handle) = spawn_range_server(content.clone());
    let tmp = TempDir::new().unwrap();
    let output = tmp.path().join("fixture.bin");
    let part_dir = part_dir_for(&output).unwrap();
    fs::create_dir_all(&part_dir).await.unwrap();

    let remote = RemoteInfo {
        size: Some(content.len() as u64),
        supports_ranges: true,
        suggested_filename: None,
        etag: Some("\"fixture\"".to_string()),
        last_modified: None,
        content_type: None,
        request_headers: RequestHeaders::default(),
    };
    let old_chunks = build_chunks(content.len() as u64, 4);
    let old_manifest = ResumeManifest::new(&url, &remote, &old_chunks, content.len() as u64);
    write_resume_manifest(&part_dir, &old_manifest)
        .await
        .unwrap();
    fs::write(part_path_for(&part_dir, 0), &content[0..25])
        .await
        .unwrap();
    fs::write(part_path_for(&part_dir, 1), &content[25..50])
        .await
        .unwrap();

    let (tx, mut rx) = mpsc::unbounded_channel();
    let cfg = DownloadConfig {
        url: url.clone(),
        output: Some(output.clone()),
        connections: ConnectionStrategy::Fixed(2),
        retries: 1,
        timeout_secs: 30,
        no_resume: false,
        no_range_strategy: NoRangeStrategy::Single,
    };

    run_download(cfg, tx).await.unwrap();
    while rx.try_recv().is_ok() {}
    let downloaded = fs::read(&output).await.unwrap();
    assert_eq!(downloaded, content);
    assert!(!path_exists(&part_dir).await.unwrap());

    stop_range_server(&url);
    handle.join().unwrap();
}

#[tokio::test]
async fn lying_range_server_falls_back_to_no_range() {
    let content = patterned_bytes(2 * 1024 * 1024 + 12_345);
    let (url, handle) = spawn_lying_range_server(content.clone());
    let tmp = TempDir::new().unwrap();
    let output = tmp.path().join("lying.bin");
    let part_dir = part_dir_for(&output).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let cfg = DownloadConfig {
        url: url.clone(),
        output: Some(output.clone()),
        connections: ConnectionStrategy::Fixed(1),
        retries: 2,
        timeout_secs: 30,
        no_resume: true,
        no_range_strategy: NoRangeStrategy::Overlap {
            workers: 2,
            overlap_bytes: 32 * 1024,
        },
    };

    run_download(cfg, tx).await.unwrap();
    while rx.try_recv().is_ok() {}

    let downloaded = fs::read(&output).await.unwrap();
    assert_eq!(downloaded, content);
    assert!(!path_exists(&part_dir).await.unwrap());

    stop_range_server(&url);
    handle.join().unwrap();
}

#[tokio::test]
async fn referer_gated_download_uses_real_file_ranges() {
    let content = patterned_bytes(3 * 1024 * 1024 + 777);
    let (url, handle) = spawn_referer_gated_server(content.clone());
    let tmp = TempDir::new().unwrap();
    let output = tmp.path().join("gated.pdf");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let cfg = DownloadConfig {
        url: url.clone(),
        output: Some(output.clone()),
        connections: ConnectionStrategy::Fixed(4),
        retries: 1,
        timeout_secs: 30,
        no_resume: true,
        no_range_strategy: NoRangeStrategy::Overlap {
            workers: 2,
            overlap_bytes: 32 * 1024,
        },
    };

    run_download(cfg, tx).await.unwrap();
    while rx.try_recv().is_ok() {}

    let downloaded = fs::read(&output).await.unwrap();
    assert_eq!(downloaded.len(), content.len());
    assert_same_bytes(&downloaded, &content);

    stop_range_server(&url);
    handle.join().unwrap();
}

#[tokio::test]
async fn cancelled_range_download_reports_exact_resume_offset() {
    let content = patterned_bytes(24 * 1024 * 1024 + 111);
    let (url, handle) = spawn_slow_range_server(content.clone());
    let tmp = TempDir::new().unwrap();
    let output = tmp.path().join("cancelled.bin");
    let cfg = DownloadConfig {
        url: url.clone(),
        output: Some(output.clone()),
        connections: ConnectionStrategy::Fixed(4),
        retries: 1,
        timeout_secs: 30,
        no_resume: false,
        no_range_strategy: NoRangeStrategy::Single,
    };

    let cancel = CancelToken::default();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let task_cancel = cancel.clone();
    let task_cfg = cfg.clone();
    let first_run =
        tokio::spawn(async move { run_download_with_cancel(task_cfg, tx, task_cancel).await });

    let mut advanced = 0u64;
    let mut plan_segments = None;
    let mut cancelled_saved = None;
    while let Some(evt) = rx.recv().await {
        match evt {
            DownloadEvent::PlanSelected { segments, .. } => {
                plan_segments = Some(segments);
            }
            DownloadEvent::Advanced(bytes) => {
                advanced = advanced.saturating_add(bytes);
                if advanced >= 512 * 1024 {
                    cancel.cancel();
                }
            }
            DownloadEvent::Cancelled { saved_bytes } => {
                cancelled_saved = Some(saved_bytes);
                break;
            }
            DownloadEvent::Completed(_) => panic!("download completed before cancellation"),
            DownloadEvent::Failed(err) => panic!("download failed: {err}"),
            _ => {}
        }
    }
    first_run.await.unwrap().unwrap();

    let saved_bytes = cancelled_saved.expect("cancelled event was not sent");
    assert!(saved_bytes > 0);
    assert!(saved_bytes < content.len() as u64);

    let segments = plan_segments.expect("parallel plan was not selected");
    let chunks = build_chunks(content.len() as u64, segments);
    let part_dir = part_dir_for(&output).unwrap();
    let disk_saved = compute_resume_offset(&part_dir, &chunks).await.unwrap();
    assert_eq!(
        saved_bytes, disk_saved,
        "cancel event must report the same verified bytes that resume will use"
    );

    let (tx, mut rx) = mpsc::unbounded_channel();
    run_download(cfg, tx).await.unwrap();
    let mut resumed_from = None;
    while let Ok(evt) = rx.try_recv() {
        if let DownloadEvent::ResumeOffset(bytes) = evt {
            resumed_from = Some(bytes);
        }
    }

    assert_eq!(resumed_from, Some(saved_bytes));
    assert_eq!(fs::read(&output).await.unwrap(), content);
    assert!(!path_exists(&part_dir).await.unwrap());

    stop_range_server(&url);
    handle.join().unwrap();
}

#[tokio::test]
async fn rate_limited_ranges_retry_as_single_range_not_no_range() {
    let content = patterned_bytes(4 * 1024 * 1024 + 333);
    let (url, state, handle) = spawn_rate_limited_range_server(content.clone());
    let tmp = TempDir::new().unwrap();
    let output = tmp.path().join("limited.bin");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let cfg = DownloadConfig {
        url: url.clone(),
        output: Some(output.clone()),
        connections: ConnectionStrategy::Fixed(4),
        retries: 10,
        timeout_secs: 30,
        no_resume: true,
        no_range_strategy: NoRangeStrategy::Overlap {
            workers: 4,
            overlap_bytes: 32 * 1024,
        },
    };

    run_download(cfg, tx).await.unwrap();
    let mut plans = Vec::new();
    while let Ok(evt) = rx.try_recv() {
        if let DownloadEvent::PlanSelected {
            workers, segments, ..
        } = evt
        {
            plans.push((workers, segments));
        }
    }

    assert_eq!(fs::read(&output).await.unwrap(), content);
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].1, plans[1].1);
    assert_eq!(plans[1].0, 1);
    let served_ranges = state.served_ranges.lock().unwrap().clone();
    let mut served_starts = served_ranges
        .iter()
        .map(|(start, _)| *start)
        .collect::<Vec<_>>();
    served_starts.sort_unstable();
    served_starts.dedup();
    assert!(
        served_ranges.len() <= plans[0].1 + 1,
        "rate-limit retry may resume an interrupted chunk tail, but should not restart chunks wholesale"
    );
    assert!(
        served_ranges.len().saturating_sub(served_starts.len()) <= 1,
        "rate-limit recovery may replay one interrupted range, but must not restart ranges wholesale"
    );
    let chunks = build_chunks(content.len() as u64, plans[0].1);
    let first_chunk_len = chunk_len(chunks[0]) as usize;
    assert!(
        served_ranges
            .iter()
            .any(|(start, end)| end - start + 1 > first_chunk_len),
        "rate-limit retry should coalesce adjacent manifest chunks into larger range requests"
    );
    stop_range_server(&url);
    handle.join().unwrap();
}

#[tokio::test]
async fn forbidden_parallel_ranges_retry_with_conservative_workers() {
    let content = patterned_bytes(8 * 1024 * 1024 + 777);
    let (url, state, handle) = spawn_policy_limited_range_server(content.clone());
    let tmp = TempDir::new().unwrap();
    let output = tmp.path().join("policy-limited.bin");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let cfg = DownloadConfig {
        url: url.clone(),
        output: Some(output.clone()),
        connections: ConnectionStrategy::Fixed(8),
        retries: 2,
        timeout_secs: 30,
        no_resume: true,
        no_range_strategy: NoRangeStrategy::Single,
    };

    run_download(cfg, tx).await.unwrap();
    let mut workers = Vec::new();
    while let Ok(evt) = rx.try_recv() {
        if let DownloadEvent::PlanSelected {
            workers: selected, ..
        } = evt
        {
            workers.push(selected);
        }
    }

    assert_eq!(fs::read(&output).await.unwrap(), content);
    assert_eq!(workers.first(), Some(&8));
    assert!(
        workers.contains(&CONSERVATIVE_RANGE_WORKERS),
        "403 recovery should retry with a conservative parallel worker count"
    );
    assert_eq!(state.full_body_requests.load(Ordering::SeqCst), 0);
    assert!(state.rejected_ranges.load(Ordering::SeqCst) > 0);

    stop_range_server(&url);
    handle.join().unwrap();
}

fn spawn_range_server(content: Vec<u8>) -> (Url, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(false).unwrap();
    let handle = thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if handle_connection(stream, &content) {
                break;
            }
        }
    });
    let url = Url::parse(&format!("http://{addr}/fixture.bin")).unwrap();
    (url, handle)
}

fn spawn_slow_range_server(content: Vec<u8>) -> (Url, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(false).unwrap();
    let handle = thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if handle_slow_connection(stream, &content) {
                break;
            }
        }
    });
    let url = Url::parse(&format!("http://{addr}/slow.bin")).unwrap();
    (url, handle)
}

fn spawn_lying_range_server(content: Vec<u8>) -> (Url, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(false).unwrap();
    let handle = thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if handle_lying_connection(stream, &content) {
                break;
            }
        }
    });
    let url = Url::parse(&format!("http://{addr}/lying.bin")).unwrap();
    (url, handle)
}

fn spawn_referer_gated_server(content: Vec<u8>) -> (Url, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(false).unwrap();
    let page = format!("http://{addr}/document/pdf/50-mb-pdf");
    let handle = thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if handle_referer_gated_connection(stream, &content, &page) {
                break;
            }
        }
    });
    let url = Url::parse(&format!("http://{addr}/file-download/325")).unwrap();
    (url, handle)
}

#[derive(Default)]
struct RateLimitState {
    rejected_ranges: AtomicUsize,
    served_ranges: Mutex<Vec<(usize, usize)>>,
}

#[derive(Default)]
struct PolicyLimitState {
    rejected_ranges: AtomicUsize,
    full_body_requests: AtomicUsize,
}

fn spawn_rate_limited_range_server(
    content: Vec<u8>,
) -> (Url, Arc<RateLimitState>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(false).unwrap();
    let state = Arc::new(RateLimitState::default());
    let thread_state = state.clone();
    let handle = thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if handle_rate_limited_connection(stream, &content, &thread_state) {
                break;
            }
        }
    });
    let url = Url::parse(&format!("http://{addr}/limited.bin")).unwrap();
    (url, state, handle)
}

fn spawn_policy_limited_range_server(
    content: Vec<u8>,
) -> (Url, Arc<PolicyLimitState>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let state = Arc::new(PolicyLimitState::default());
    let thread_state = state.clone();
    let handle = thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if handle_policy_limited_connection(stream, &content, &thread_state) {
                break;
            }
        }
    });
    let url = Url::parse(&format!("http://{addr}/policy-limited.bin")).unwrap();
    (url, state, handle)
}

fn stop_range_server(url: &Url) {
    let addr = format!(
        "{}:{}",
        url.host_str().unwrap(),
        url.port_or_known_default().unwrap()
    );
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .write_all(b"GET /shutdown HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
}

fn handle_connection(mut stream: TcpStream, content: &[u8]) -> bool {
    let Some(request) = read_request_headers(&mut stream) else {
        return false;
    };
    let first_line = request.lines().next().unwrap_or_default();
    let shutdown = request.contains("/shutdown");

    if shutdown {
        let _ = stream.write_all(
            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return true;
    }

    if first_line.starts_with("HEAD ") {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"fixture\"\r\nConnection: close\r\n\r\n",
            content.len()
        );
        let _ = stream.write_all(response.as_bytes());
        return shutdown;
    }

    if let Some((start, end)) = parse_test_range(&request) {
        let end = end.min(content.len().saturating_sub(1));
        let body = &content[start..=end];
        let response = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nETag: \"fixture\"\r\nConnection: close\r\n\r\n",
            body.len(),
            start,
            end,
            content.len()
        );
        let _ = stream.write_all(response.as_bytes());
        write_body(&mut stream, body);
        return shutdown;
    }

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"fixture\"\r\nConnection: close\r\n\r\n",
        content.len()
    );
    let _ = stream.write_all(response.as_bytes());
    write_body(&mut stream, content);
    shutdown
}

fn handle_slow_connection(mut stream: TcpStream, content: &[u8]) -> bool {
    let Some(request) = read_request_headers(&mut stream) else {
        return false;
    };
    let first_line = request.lines().next().unwrap_or_default();
    let shutdown = request.contains("/shutdown");

    if shutdown {
        let _ = stream.write_all(
            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return true;
    }

    if first_line.starts_with("HEAD ") {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"fixture\"\r\nConnection: close\r\n\r\n",
            content.len()
        );
        let _ = stream.write_all(response.as_bytes());
        return false;
    }

    if let Some((start, end)) = parse_test_range(&request) {
        write_slow_range_response(&mut stream, content, start, end);
        return false;
    }

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"fixture\"\r\nConnection: close\r\n\r\n",
        content.len()
    );
    let _ = stream.write_all(response.as_bytes());
    write_body_slow(&mut stream, content);
    false
}

fn handle_lying_connection(mut stream: TcpStream, content: &[u8]) -> bool {
    let Some(request) = read_request_headers(&mut stream) else {
        return false;
    };
    let first_line = request.lines().next().unwrap_or_default();
    let shutdown = request.contains("/shutdown");

    if shutdown {
        let _ = stream.write_all(
            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return true;
    }

    if first_line.starts_with("HEAD ") {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
            content.len()
        );
        let _ = stream.write_all(response.as_bytes());
        return false;
    }

    if let Some((start, end)) = parse_test_range(&request) {
        let claimed_end = end.min(content.len().saturating_sub(1));
        let response = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
            start,
            claimed_end,
            content.len()
        );
        let _ = stream.write_all(response.as_bytes());

        let extra_end = if start == 0 && claimed_end == 0 {
            claimed_end
        } else {
            (claimed_end + 4096).min(content.len().saturating_sub(1))
        };
        write_body(&mut stream, &content[start..=extra_end]);
        return false;
    }

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
        content.len()
    );
    let _ = stream.write_all(response.as_bytes());
    write_body(&mut stream, content);
    false
}

fn handle_referer_gated_connection(mut stream: TcpStream, content: &[u8], page: &str) -> bool {
    let Some(request) = read_request_headers(&mut stream) else {
        return false;
    };
    let first_line = request.lines().next().unwrap_or_default();
    let shutdown = request.contains("/shutdown");

    if shutdown {
        let _ = stream.write_all(
            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return true;
    }

    if first_line.starts_with("HEAD ") {
        write_html_page(&mut stream, content.len());
        return false;
    }

    if first_line.contains("/document/pdf/50-mb-pdf") {
        write_html_page(&mut stream, content.len());
        return false;
    }

    let request_lower = request.to_ascii_lowercase();
    if !request_lower.contains(&format!("referer: {}", page.to_ascii_lowercase()))
        || !request_lower.contains("x-requested-with: xmlhttprequest")
    {
        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: {page}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let _ = stream.write_all(response.as_bytes());
        return false;
    }

    if let Some((start, end)) = parse_test_range(&request) {
        write_range_response(
            &mut stream,
            content,
            start,
            end,
            "application/pdf",
            "50mb.pdf",
        );
    } else {
        write_attachment_response(&mut stream, content, "application/pdf", "50mb.pdf");
    }
    false
}

fn handle_rate_limited_connection(
    mut stream: TcpStream,
    content: &[u8],
    state: &Arc<RateLimitState>,
) -> bool {
    let Some(request) = read_request_headers(&mut stream) else {
        return false;
    };
    let first_line = request.lines().next().unwrap_or_default();
    let shutdown = request.contains("/shutdown");

    if shutdown {
        let _ = stream.write_all(
            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return true;
    }

    if first_line.starts_with("HEAD ") {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
            content.len()
        );
        let _ = stream.write_all(response.as_bytes());
        return false;
    }

    let Some((start, end)) = parse_test_range(&request) else {
        write_attachment_response(
            &mut stream,
            content,
            "application/octet-stream",
            "limited.bin",
        );
        return false;
    };

    if start == 0 && end == 0 {
        write_range_response(
            &mut stream,
            content,
            start,
            end,
            "application/octet-stream",
            "limited.bin",
        );
        return false;
    }

    let served_before_limit = {
        let mut served_ranges = state.served_ranges.lock().unwrap();
        if served_ranges.is_empty() {
            served_ranges.push((start, end));
            true
        } else {
            false
        }
    };
    if served_before_limit {
        write_range_response(
            &mut stream,
            content,
            start,
            end,
            "application/octet-stream",
            "limited.bin",
        );
        return false;
    }

    if state.rejected_ranges.fetch_add(1, Ordering::SeqCst) < 8 {
        let response = b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(response);
        return false;
    }

    state.served_ranges.lock().unwrap().push((start, end));
    write_range_response(
        &mut stream,
        content,
        start,
        end,
        "application/octet-stream",
        "limited.bin",
    );
    false
}

fn handle_policy_limited_connection(
    mut stream: TcpStream,
    content: &[u8],
    state: &Arc<PolicyLimitState>,
) -> bool {
    let Some(request) = read_request_headers(&mut stream) else {
        return false;
    };
    let first_line = request.lines().next().unwrap_or_default();
    if request.contains("/shutdown") {
        let _ = stream.write_all(
            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return true;
    }
    if first_line.starts_with("HEAD ") {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
            content.len()
        );
        let _ = stream.write_all(response.as_bytes());
        return false;
    }

    let Some((start, end)) = parse_test_range(&request) else {
        state.full_body_requests.fetch_add(1, Ordering::SeqCst);
        write_attachment_response(
            &mut stream,
            content,
            "application/octet-stream",
            "policy-limited.bin",
        );
        return false;
    };
    if start == 0 && end == 0 {
        write_range_response(
            &mut stream,
            content,
            start,
            end,
            "application/octet-stream",
            "policy-limited.bin",
        );
        return false;
    }
    if state.rejected_ranges.fetch_add(1, Ordering::SeqCst) < 4 {
        let _ = stream
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return false;
    }

    write_range_response(
        &mut stream,
        content,
        start,
        end,
        "application/octet-stream",
        "policy-limited.bin",
    );
    false
}

fn read_request_headers(stream: &mut TcpStream) -> Option<String> {
    let mut request = Vec::with_capacity(1024);
    let mut buffer = [0u8; 1024];
    while request.len() < 16 * 1024 {
        let read = stream.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(request).ok()
}

fn write_html_page(stream: &mut TcpStream, advertised_size: usize) {
    let body = format!(
        "<html><title>50 MB PDF</title><body><a href=\"/file-download/325\">Download</a><p>Size {advertised_size}</p></body></html>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=UTF-8\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
}

fn write_attachment_response(
    stream: &mut TcpStream,
    body: &[u8],
    content_type: &str,
    filename: &str,
) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nContent-Disposition: attachment; filename={filename}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    write_body(stream, body);
}

fn write_range_response(
    stream: &mut TcpStream,
    content: &[u8],
    start: usize,
    end: usize,
    content_type: &str,
    filename: &str,
) {
    let end = end.min(content.len().saturating_sub(1));
    let body = &content[start..=end];
    let response = format!(
        "HTTP/1.1 206 Partial Content\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nContent-Disposition: attachment; filename={filename}\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
        body.len(),
        start,
        end,
        content.len()
    );
    let _ = stream.write_all(response.as_bytes());
    write_body(stream, body);
}

fn write_slow_range_response(stream: &mut TcpStream, content: &[u8], start: usize, end: usize) {
    let end = end.min(content.len().saturating_sub(1));
    let body = &content[start..=end];
    let response = format!(
        "HTTP/1.1 206 Partial Content\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
        body.len(),
        start,
        end,
        content.len()
    );
    let _ = stream.write_all(response.as_bytes());
    write_body_slow(stream, body);
}

fn write_body(stream: &mut TcpStream, body: &[u8]) {
    for chunk in body.chunks(64 * 1024) {
        if stream.write_all(chunk).is_err() {
            break;
        }
    }
}

fn write_body_slow(stream: &mut TcpStream, body: &[u8]) {
    for chunk in body.chunks(64 * 1024) {
        if stream.write_all(chunk).is_err() {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }
}

fn parse_test_range(request: &str) -> Option<(usize, usize)> {
    let header = request
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("range: bytes="))?;
    let range = header.split_once("bytes=")?.1.trim();
    let (start, end) = range.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?))
}

fn patterned_bytes(len: usize) -> Vec<u8> {
    (0..len)
        .map(|idx| ((idx.wrapping_mul(31) + idx / 7) % 251) as u8)
        .collect()
}

fn assert_same_bytes(actual: &[u8], expected: &[u8]) {
    if actual == expected {
        return;
    }
    let mismatch = actual
        .iter()
        .zip(expected)
        .position(|(actual, expected)| actual != expected)
        .unwrap_or_else(|| actual.len().min(expected.len()));
    panic!(
        "byte mismatch at offset {mismatch}: actual={:?}, expected={:?}",
        actual.get(mismatch),
        expected.get(mismatch)
    );
}
