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
fn comfy_checks_and_serves_https_with_valid_certificate() {
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
    fs::write(dir.path().join("index.html"), "comfy tls ok").unwrap();
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
    let binary = env!("CARGO_BIN_EXE_rws-comfy");
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
            "Comfy exited before TLS startup"
        );
        assert!(
            Instant::now() < deadline,
            "Comfy TLS listener did not start"
        );
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
    assert_eq!(output.stdout, b"comfy tls ok");
}
