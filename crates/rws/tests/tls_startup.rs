#![cfg(target_os = "linux")]

use std::{
    fs,
    net::{SocketAddr, TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn rws_checks_and_serves_https_with_valid_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let cert = dir.path().join("cert.pem");
    let key = dir.path().join("key.pem");
    let generated = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=localhost",
            "-addext",
            "subjectAltName=DNS:localhost",
            "-keyout",
        ])
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("openssl is required for the Linux TLS integration test");
    assert!(
        generated.status.success(),
        "openssl failed: {}",
        String::from_utf8_lossy(&generated.stderr)
    );
    fs::write(dir.path().join("index.html"), "rws tls ok").unwrap();
    let challenges = dir.path().join(".well-known/acme-challenge");
    fs::create_dir_all(&challenges).unwrap();
    fs::write(challenges.join("Test_token-01"), "test-key-authorization").unwrap();
    // Keep all sockets reserved until the addresses have been selected.
    let sockets: Vec<_> = (0..3)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    let addresses: Vec<SocketAddr> = sockets
        .iter()
        .map(|socket| socket.local_addr().unwrap())
        .collect();
    let config = dir.path().join("config.toml");
    fs::write(&config, format!(
        "[server]\nlisten='{}'\nhttps_listen='{}'\n[metrics]\nlisten='{}'\n[tls]\ncert_file='{}'\nkey_file='{}'\n[[sites]]\nhost='localhost'\nroot='{}'\n",
        addresses[0], addresses[1], addresses[2], cert.display(), key.display(), dir.path().display()
    )).unwrap();
    let binary = env!("CARGO_BIN_EXE_rws");
    let check = Command::new(binary)
        .arg("--config")
        .arg(&config)
        .arg("--check")
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "TLS --check failed: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    drop(sockets);
    let mut server = Server(
        Command::new(binary)
            .arg("--config")
            .arg(&config)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect_timeout(&addresses[1], Duration::from_millis(100)).is_err() {
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "RWS exited before TLS startup"
        );
        assert!(Instant::now() < deadline, "RWS TLS listener did not start");
        thread::sleep(Duration::from_millis(25));
    }
    let output = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail",
            "--max-time",
            "5",
            "--noproxy",
            "*",
            "--cacert",
        ])
        .arg(&cert)
        .arg("--resolve")
        .arg(format!("localhost:{}:127.0.0.1", addresses[1].port()))
        .arg(format!("https://localhost:{}/", addresses[1].port()))
        .output()
        .expect("curl is required for the Linux TLS integration test");
    assert!(
        output.status.success(),
        "HTTPS request failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"rws tls ok");

    // Verify that only the plaintext listener redirects, including POST and the
    // native WebSocket route, and that the nonstandard HTTPS port is retained.
    for path in ["/a%20b?x=1&next=%2F", "/_rws/ws"] {
        let output = Command::new("curl")
            .args([
                "--silent",
                "--show-error",
                "--max-time",
                "5",
                "--noproxy",
                "*",
                "--request",
                "POST",
                "--dump-header",
                "-",
                "--output",
                "/dev/null",
            ])
            .arg(format!("http://localhost:{}{path}", addresses[0].port()))
            .output()
            .unwrap();
        assert!(output.status.success());
        let headers = String::from_utf8(output.stdout)
            .unwrap()
            .to_ascii_lowercase();
        assert!(
            headers.starts_with("HTTP/1.1 308".to_ascii_lowercase().as_str()),
            "{headers}"
        );
        assert!(
            headers.contains(
                &format!(
                    "location: https://localhost:{}{path}\r\n",
                    addresses[1].port()
                )
                .to_ascii_lowercase()
            ),
            "{headers}"
        );
    }
    let output = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail",
            "--max-time",
            "5",
            "--noproxy",
            "*",
        ])
        .arg(format!(
            "http://localhost:{}/.well-known/acme-challenge/Test_token-01",
            addresses[0].port()
        ))
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"test-key-authorization");
}
