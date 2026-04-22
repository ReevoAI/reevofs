//! Verifies write_file sends raw bytes (application/octet-stream) and maps
//! 415 → Forbidden.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

use reevofs_api::{ApiError, ReevoClient};

type Captured = Arc<Mutex<Vec<u8>>>;

/// Read the full HTTP request (headers + body of declared Content-Length)
/// into `captured`, then write `response`.
fn spawn_server(response: Vec<u8>) -> (String, Captured, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = Arc::clone(&captured);
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        // Read until we see the headers + full body. Simplification: read once;
        // ureq writes the whole small request in one go in practice.
        loop {
            let n = stream.read(&mut tmp).unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(header_end) = find_double_crlf(&buf) {
                // Parse Content-Length to know when the body is complete.
                let headers = &buf[..header_end];
                let cl = parse_content_length(headers).unwrap_or(0);
                if buf.len() >= header_end + 4 + cl {
                    break;
                }
            }
        }
        *captured_clone.lock().unwrap() = buf;
        stream.write_all(&response).unwrap();
    });
    (url, captured, handle)
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_content_length(headers: &[u8]) -> Option<usize> {
    let s = std::str::from_utf8(headers).ok()?;
    for line in s.split("\r\n") {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

#[test]
fn write_file_sends_raw_bytes_with_octet_stream() {
    let json_body = br#"{"success":true,"path":"/foo.bin"}"#;
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        json_body.len()
    )
    .into_bytes()
    .into_iter()
    .chain(json_body.iter().copied())
    .collect::<Vec<u8>>();
    let (url, captured, handle) = spawn_server(response);
    let client = ReevoClient::new(&url, "");
    let body: &[u8] = &[0xff, 0xfe, 0xfd, 0xfc, 0x00, 0x01];
    let _ = client.write_file("ns", "scope", "/foo.bin", body).unwrap();
    handle.join().unwrap();

    let req = captured.lock().unwrap().clone();
    let req_str = String::from_utf8_lossy(&req);
    assert!(
        req_str.to_ascii_lowercase().contains("content-type: application/octet-stream"),
        "expected octet-stream header, got:\n{req_str}"
    );
    // Body bytes appear verbatim after the \r\n\r\n separator.
    let sep = req.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    assert_eq!(&req[sep..sep + body.len()], body);
}

#[test]
fn write_file_maps_415_to_forbidden() {
    let response = b"HTTP/1.1 415 Unsupported Media Type\r\nContent-Length: 0\r\n\r\n".to_vec();
    let (url, _captured, handle) = spawn_server(response);
    let client = ReevoClient::new(&url, "");
    let err = client.write_file("ns", "scope", "/x.exe", b"ignored").unwrap_err();
    handle.join().unwrap();
    assert!(matches!(err, ApiError::Forbidden), "expected Forbidden, got {err:?}");
}
