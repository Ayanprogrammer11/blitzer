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
    let (url, handle) = spawn_no_range_server(TestBody::Alternating(first, second));
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

enum TestBody {
    Stable(Vec<u8>),
    Alternating(Vec<u8>, Vec<u8>),
}

fn spawn_no_range_server(body: TestBody) -> (Url, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let handle = thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if handle_connection(stream, &body, &counter) {
                break;
            }
        }
    });
    let url = Url::parse(&format!("http://{addr}/file.bin")).unwrap();
    (url, handle)
}

fn handle_connection(mut stream: TcpStream, body: &TestBody, counter: &Arc<AtomicUsize>) -> bool {
    let mut request = [0u8; 4096];
    let Ok(read) = stream.read(&mut request) else {
        return false;
    };
    let request = String::from_utf8_lossy(&request[..read]);
    if request.contains("/shutdown") {
        let _ = stream.write_all(
            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return true;
    }

    let selected = match body {
        TestBody::Stable(bytes) => bytes.as_slice(),
        TestBody::Alternating(first, second) => {
            if counter.fetch_add(1, Ordering::SeqCst).is_multiple_of(2) {
                first.as_slice()
            } else {
                second.as_slice()
            }
        }
    };
    let response = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n";
    stream.write_all(response).unwrap();
    stream.write_all(selected).unwrap();
    false
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
