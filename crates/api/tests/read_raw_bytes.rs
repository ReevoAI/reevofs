//! Verifies read_file returns raw bytes (including non-UTF-8).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use reevofs_api::{ApiError, ReevoClient};

fn spawn_server(response: Vec<u8>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 2048];
        let _ = stream.read(&mut buf);
        stream.write_all(&response).unwrap();
    });
    (url, handle)
}

#[test]
fn read_file_returns_raw_bytes_including_binary() {
    let body: &[u8] = &[0xff, 0xfe, 0xfd, 0xfc];
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\n\r\n",
        body.len()
    );
    let mut response = header.into_bytes();
    response.extend_from_slice(body);

    let (url, handle) = spawn_server(response);
    let client = ReevoClient::new(&url, "");
    let got = client.read_file("ns", "scope", "/any").unwrap();
    handle.join().unwrap();

    assert_eq!(got, body);
}

#[test]
fn read_file_maps_415_to_forbidden() {
    let response = b"HTTP/1.1 415 Unsupported Media Type\r\nContent-Length: 0\r\n\r\n".to_vec();
    let (url, handle) = spawn_server(response);
    let client = ReevoClient::new(&url, "");
    let err = client.read_file("ns", "scope", "/x.exe").unwrap_err();
    handle.join().unwrap();
    assert!(matches!(err, ApiError::Forbidden), "expected Forbidden, got {err:?}");
}
