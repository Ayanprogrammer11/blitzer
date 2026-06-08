use super::*;
use crate::{config::NoRangeStrategy, download::CancelToken};
use reqwest::{Client, Url};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};
use tempfile::TempDir;
use tokio::{fs, sync::mpsc};

#[tokio::test]
async fn overlap_downloads_unknown_length_stable_stream() {
    let content = patterned_bytes((PAYLOAD_BYTES * 3 + 12_345) as usize);
    let (url, handle) = spawn_no_range_server(TestBody::Stable(content.clone()));
    let tmp = TempDir::new().unwrap();
    let output = tmp.path().join("stable.bin");
    let (tx, mut rx) = mpsc::unbounded_channel();

    let client = Client::new();
    let request_headers = RequestHeaders::default();
    let bytes = download_no_range_overlap(NoRangeDownload {
        client: &client,
        url: &url,
        output: &output,
        total_size: None,
        request_headers: &request_headers,
        strategy: NoRangeStrategy::Overlap {
            workers: 3,
            overlap_bytes: 32 * 1024,
        },
        retries: 1,
        cancel: CancelToken::default(),
        tx,
    })
    .await
    .unwrap();
    while rx.try_recv().is_ok() {}

    assert_eq!(bytes, content.len() as u64);
    assert_eq!(fs::read(&output).await.unwrap(), content);

    stop_server(&url);
    handle.join().unwrap();
}

#[tokio::test]
async fn overlap_rejects_unstable_unknown_length_stream() {
    let first = patterned_bytes((PAYLOAD_BYTES * 2 + 1024) as usize);
    let mut second = first.clone();
    second[PAYLOAD_BYTES as usize + 10] ^= 0xff;
    let (url, handle) = spawn_no_range_server(TestBody::ChangesAfterFirst(first, second));
    let tmp = TempDir::new().unwrap();
    let output = tmp.path().join("unstable.bin");
    let (tx, mut rx) = mpsc::unbounded_channel();

    let client = Client::new();
    let request_headers = RequestHeaders::default();
    let err = download_no_range_overlap(NoRangeDownload {
        client: &client,
        url: &url,
        output: &output,
        total_size: None,
        request_headers: &request_headers,
        strategy: NoRangeStrategy::Overlap {
            workers: 2,
            overlap_bytes: 32 * 1024,
        },
        retries: 0,
        cancel: CancelToken::default(),
        tx,
    })
    .await
    .unwrap_err();
    while rx.try_recv().is_ok() {}

    assert!(format!("{err:#}").contains("overlap mismatch"));

    stop_server(&url);
    handle.join().unwrap();
}

#[tokio::test]
async fn unknown_length_small_stream_uses_single_probe() {
    let content = patterned_bytes(256 * 1024);
    let (url, counter, handle) =
        spawn_no_range_server_with_counter(TestBody::Stable(content.clone()));
    let tmp = TempDir::new().unwrap();
    let output = tmp.path().join("small.bin");
    let (tx, mut rx) = mpsc::unbounded_channel();

    let client = Client::new();
    let request_headers = RequestHeaders::default();
    let bytes = download_no_range_overlap(NoRangeDownload {
        client: &client,
        url: &url,
        output: &output,
        total_size: None,
        request_headers: &request_headers,
        strategy: NoRangeStrategy::Overlap {
            workers: 4,
            overlap_bytes: 32 * 1024,
        },
        retries: 0,
        cancel: CancelToken::default(),
        tx,
    })
    .await
    .unwrap();
    while rx.try_recv().is_ok() {}

    assert_eq!(bytes, content.len() as u64);
    assert_eq!(fs::read(&output).await.unwrap(), content);
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    stop_server(&url);
    handle.join().unwrap();
}

enum TestBody {
    Stable(Vec<u8>),
    ChangesAfterFirst(Vec<u8>, Vec<u8>),
}

fn spawn_no_range_server(body: TestBody) -> (Url, thread::JoinHandle<()>) {
    let (url, _counter, handle) = spawn_no_range_server_with_counter(body);
    (url, handle)
}

fn spawn_no_range_server_with_counter(
    body: TestBody,
) -> (Url, Arc<AtomicUsize>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let server_counter = counter.clone();
    let handle = thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if handle_connection(stream, &body, &server_counter) {
                break;
            }
        }
    });
    let url = Url::parse(&format!("http://{addr}/file.bin")).unwrap();
    (url, counter, handle)
}

fn handle_connection(mut stream: TcpStream, body: &TestBody, counter: &Arc<AtomicUsize>) -> bool {
    let Some(request) = read_request_headers(&mut stream) else {
        return false;
    };
    if request.contains("/shutdown") {
        let _ = stream.write_all(
            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return true;
    }

    let request_index = counter.fetch_add(1, Ordering::SeqCst);
    let selected = match body {
        TestBody::Stable(bytes) => bytes.as_slice(),
        TestBody::ChangesAfterFirst(first, second) => {
            if request_index == 0 {
                first.as_slice()
            } else {
                second.as_slice()
            }
        }
    };
    let response = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n";
    let _ = stream.write_all(response);
    let _ = stream.write_all(selected);
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

fn stop_server(url: &Url) {
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

fn patterned_bytes(len: usize) -> Vec<u8> {
    (0..len).map(|idx| (idx % 251) as u8).collect()
}
