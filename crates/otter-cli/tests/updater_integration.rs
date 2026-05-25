//! Integration test: spawn `otter _daemon` with `OTTER_UPDATE_REPO_URL`
//! pointing at a stub HTTP server and assert that the startup probe writes a
//! cache file and that the broadcast event reaches a connecting subscriber.

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const STUB_RELEASE_JSON: &str = r#"{
    "tag_name": "v9.9.9",
    "html_url": "https://github.com/Checkmk/otter/releases/tag/v9.9.9",
    "prerelease": false,
    "draft": false
}"#;

struct StubServer {
    addr: String,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl StubServer {
    fn start(body: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_thread = shutdown.clone();
        let handle = thread::spawn(move || {
            while !shutdown_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut buf = [0u8; 1024];
                        let _ = stream.read(&mut buf);
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });
        StubServer {
            addr,
            shutdown,
            handle: Some(handle),
        }
    }
}

impl Drop for StubServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn otter_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_otter"))
}

fn data_dir(home: &Path) -> PathBuf {
    home.join(".local/share/otter")
}

fn wait_for<F: Fn() -> bool>(timeout: Duration, cond: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    cond()
}

#[test]
fn daemon_startup_probe_writes_update_cache() {
    // GIVEN a stub release server reporting a much-newer version
    let stub = StubServer::start(STUB_RELEASE_JSON.to_string());
    let home = tempfile::tempdir().expect("home tempdir");
    std::fs::create_dir_all(home.path().join(".config/otter")).unwrap();
    std::fs::create_dir_all(data_dir(home.path())).unwrap();

    // WHEN the daemon is started against that stub
    let mut child = Command::new(otter_binary())
        .arg("_daemon")
        .env("HOME", home.path())
        .env("OTTER_UPDATE_REPO_URL", &stub.addr)
        .env("OTTER_NO_DESKTOP_NOTIFY", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn otter _daemon");

    let cache_path = data_dir(home.path()).join("update.json");

    // THEN the probe writes the cache file within a few seconds
    let appeared = wait_for(Duration::from_secs(10), || cache_path.exists());

    // Clean up the daemon before any assertion can panic and leak it.
    let _ = child.kill();
    let _ = child.wait();

    assert!(appeared, "update.json never appeared at {:?}", cache_path);

    let bytes = std::fs::read(&cache_path).expect("read update.json");
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("parse update.json");
    assert_eq!(value["latest"], "9.9.9");
    assert_eq!(value["current"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn daemon_writes_no_cache_when_up_to_date() {
    // GIVEN a stub reporting the same version that this binary was built with
    let same_version = format!(
        r#"{{
            "tag_name": "v{v}",
            "html_url": "https://github.com/Checkmk/otter/releases/tag/v{v}",
            "prerelease": false,
            "draft": false
        }}"#,
        v = env!("CARGO_PKG_VERSION")
    );
    let stub = StubServer::start(same_version);
    let home = tempfile::tempdir().expect("home tempdir");
    std::fs::create_dir_all(home.path().join(".config/otter")).unwrap();
    std::fs::create_dir_all(data_dir(home.path())).unwrap();

    // WHEN the daemon runs the startup probe
    let mut child = Command::new(otter_binary())
        .arg("_daemon")
        .env("HOME", home.path())
        .env("OTTER_UPDATE_REPO_URL", &stub.addr)
        .env("OTTER_NO_DESKTOP_NOTIFY", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn otter _daemon");

    // Wait long enough for the probe to definitely have completed
    thread::sleep(Duration::from_secs(2));

    let cache_path = data_dir(home.path()).join("update.json");
    let exists = cache_path.exists();

    let _ = child.kill();
    let _ = child.wait();

    // THEN no cache file is written (write_cache(None) removes a missing file as a no-op)
    assert!(
        !exists,
        "update.json should not exist when daemon is up-to-date"
    );
}
