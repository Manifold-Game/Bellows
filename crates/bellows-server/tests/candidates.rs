//! A server stores a candidate without the checkouts a client pinned it to:
//! only the machine that holds a checkout can tell whether it still exists.
use bellows_core::{
    ActionCandidate, Artifact, CandidateIndex, EnvInput, PIN_PREFIX, PROTOCOL_VERSION,
    PinnedCheckout, StreamArtifact, compiler_action_key, digest_bytes,
};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
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
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
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

#[test]
fn a_published_candidate_is_stored_without_its_checkouts() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("store");
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let _daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_bellowsd"))
            .args(["--listen", &format!("127.0.0.1:{port}")])
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

    let output = b"library bytes".to_vec();
    let blob = |bytes: &[u8]| {
        let digest = digest_bytes(bytes);
        let (status, _) = request(port, "PUT", &format!("/v1/blobs/{digest}"), bytes);
        assert!(
            status == 201 || status == 204,
            "blob upload answered {status}"
        );
        digest
    };
    let empty = blob(b"");
    let static_key = digest_bytes(b"static");
    let env = vec![EnvInput::capture(
        format!("{PIN_PREFIX}$WORKSPACE"),
        Some("/work/checkout/crate"),
    )];
    let candidate = ActionCandidate {
        protocol: PROTOCOL_VERSION,
        action_key: compiler_action_key(&static_key, &[], &[], &env),
        static_key: static_key.clone(),
        crate_name: "fixture".into(),
        created_ms: 1,
        files: vec![],
        host_files: vec![],
        env,
        artifacts: vec![Artifact {
            file_name: "libfixture-abc.rlib".into(),
            digest: blob(&output),
            executable: false,
        }],
        stdout: StreamArtifact {
            digest: empty.clone(),
            len: 0,
        },
        stderr: StreamArtifact {
            digest: empty,
            len: 0,
        },
        proc_macros: vec![],
        // A client that sends them anyway (none should).
        pinned_checkouts: vec![PinnedCheckout {
            path: "/work/checkout".into(),
            volume: "/".into(),
        }],
    };
    let path = format!("/v1/actions/{static_key}/{}", candidate.action_key);
    let (status, body) = request(port, "PUT", &path, &serde_json::to_vec(&candidate).unwrap());
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));

    let (status, body) = request(port, "GET", &format!("/v1/actions/{static_key}"), b"");
    assert_eq!(status, 200);
    let text = String::from_utf8(body).unwrap();
    assert!(!text.contains("pinned_checkouts"), "{text}");
    assert!(!text.contains("/work/checkout\""), "{text}");
    let index: CandidateIndex = serde_json::from_str(&text).unwrap();
    assert_eq!(index.candidates.len(), 1);
    assert_eq!(index.candidates[0].action_key, candidate.action_key);
    assert!(index.candidates[0].pinned_checkouts.is_empty());
    let stored = std::fs::read_to_string(
        data.join("actions")
            .join(&static_key[..2])
            .join(format!("{static_key}.json")),
    )
    .unwrap();
    assert!(!stored.contains("pinned_checkouts"), "{stored}");
}
