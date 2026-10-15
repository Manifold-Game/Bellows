//! Blob uploads stream to disk and downloads stream from it, with the same
//! answers the buffered handlers gave.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Sends one request and returns the status code and body.
fn request(port: u16, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("response has a header block");
    let status = std::str::from_utf8(&response[9..12])
        .unwrap()
        .parse()
        .unwrap();
    (status, response[split + 4..].to_vec())
}

fn temporaries(data: &Path) -> usize {
    let mut count = 0;
    let mut stack = vec![data.join("blobs")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            } else if entry.file_name().to_string_lossy().starts_with(".tmp-") {
                count += 1;
            }
        }
    }
    count
}

#[test]
fn blobs_stream_through_with_their_limits_and_digest_checks() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("store");
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let _daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_bellowsd"))
            .args([
                "--listen",
                &format!("127.0.0.1:{port}"),
                "--max-blob-mb",
                "1",
            ])
            .arg("--data-dir")
            .arg(&data)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "bellowsd never listened");
        std::thread::sleep(Duration::from_millis(50));
    }

    // Larger than one read buffer, so the body arrives in several chunks.
    let blob = (0..700_000u32).map(|i| (i % 251) as u8).collect::<Vec<_>>();
    let digest = blake3::hash(&blob).to_hex().to_string();
    let path = format!("/v1/blobs/{digest}");
    assert_eq!(request(port, "GET", &path, b"").0, 404);
    assert_eq!(request(port, "PUT", &path, &blob).0, 201);
    assert_eq!(
        request(port, "PUT", &path, &blob).0,
        204,
        "a second copy is kept once"
    );
    assert_eq!(request(port, "HEAD", &path, b"").0, 200);
    let (status, body) = request(port, "GET", &path, b"");
    assert_eq!(status, 200);
    assert!(body == blob, "downloaded blob differs from the upload");

    let other = vec![7u8; 1000];
    let wrong = format!("/v1/blobs/{}", blake3::hash(b"something else").to_hex());
    assert_eq!(request(port, "PUT", &wrong, &other).0, 400);

    // One byte over --max-blob-mb: the server has read the whole body before
    // it refuses, so the refusal arrives intact.
    let oversized = vec![1u8; 1024 * 1024 + 1];
    let oversized_path = format!("/v1/blobs/{}", blake3::hash(&oversized).to_hex());
    assert_eq!(request(port, "PUT", &oversized_path, &oversized).0, 413);
    assert_eq!(request(port, "GET", &oversized_path, b"").0, 404);
    assert_eq!(temporaries(&data), 0, "a refused upload left its temporary");
}
