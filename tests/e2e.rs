use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};
use tempfile::TempDir;
use tokio::{net::TcpStream, time::sleep};

/// Get the project root directory (where Cargo.toml is)
fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Find an available port by binding to port 0
fn find_available_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Wait for a TCP server to be ready
async fn wait_for_server(port: u16, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return true;
        }
        sleep(Duration::from_millis(50)).await;
    }
    false
}

struct TestProxy {
    process: Child,
    port: u16,
    trigger_file: PathBuf,
}

impl TestProxy {
    async fn start() -> Self {
        let port = find_available_port();
        let project_root = project_root();
        let trigger_file = project_root.join(".e2e-trigger.txt");

        // Clean up any leftover trigger file
        let _ = std::fs::remove_file(&trigger_file);

        // Build the test-server first to avoid build time during test
        let status = Command::new("cargo")
            .args(["build", "--bin", "test-server"])
            .current_dir(&project_root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("failed to build test-server");
        assert!(status.success(), "failed to build test-server");

        // Start the proxy
        let process = Command::new(env!("CARGO_BIN_EXE_build-proxy"))
            .args(["--port", &port.to_string()])
            .args(["--pwd", project_root.to_str().unwrap()])
            .args(["--", "cargo", "run", "--bin", "test-server"])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to start build-proxy");

        // Wait for the proxy to be ready by polling the port
        assert!(
            wait_for_server(port, Duration::from_secs(30)).await,
            "proxy did not become ready in time"
        );

        TestProxy {
            process,
            port,
            trigger_file,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.port, path)
    }

    fn touch_trigger_file(&self) {
        std::fs::write(&self.trigger_file, "trigger rebuild").expect("failed to write trigger file");
    }
}

impl Drop for TestProxy {
    fn drop(&mut self) {
        // Kill the proxy process
        let _ = self.process.kill();
        let _ = self.process.wait();

        // Clean up trigger file
        let _ = std::fs::remove_file(&self.trigger_file);
    }
}

#[tokio::test]
async fn test_proxy_forwards_requests() {
    let proxy = TestProxy::start().await;

    let response = reqwest::get(proxy.url("/"))
        .await
        .expect("request failed");

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "hello");
}

#[tokio::test]
async fn test_rebuild_on_file_change() {
    let proxy = TestProxy::start().await;

    // Get initial instance ID
    let id1 = reqwest::get(proxy.url("/instance-id"))
        .await
        .expect("first request failed")
        .text()
        .await
        .unwrap();

    // Touch a file to trigger rebuild
    proxy.touch_trigger_file();

    // Wait a bit for the file watcher to detect the change and kill the child
    sleep(Duration::from_millis(200)).await;

    // Make another request - this should trigger a rebuild
    let id2 = reqwest::get(proxy.url("/instance-id"))
        .await
        .expect("second request failed")
        .text()
        .await
        .unwrap();

    // The instance IDs should be different (server was rebuilt)
    assert_ne!(
        id1, id2,
        "expected server to be rebuilt after file change, but instance ID remained the same"
    );
}

#[tokio::test]
async fn test_rebuild_after_initial_failure() {
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    // Create a script that will fail initially (missing success marker)
    let script_path = temp_path.join("server.sh");
    let success_marker = temp_path.join("success_marker");
    let trigger_file = temp_path.join("trigger.txt");

    // Script that checks for success marker, fails if not present
    // Uses Python's http.server which is more reliable than netcat
    let script_content = format!(
        r#"#!/bin/bash
if [ ! -f "{}" ]; then
    echo "Build failed: success marker not found" >&2
    exit 1
fi

PORT="${{PORT}}"
python3 -c "
import http.server
import socketserver

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header('Content-Type', 'text/plain')
        self.end_headers()
        self.wfile.write(b'hello')
    def log_message(self, format, *args):
        pass  # Suppress logging

with socketserver.TCPServer(('', $PORT), Handler) as httpd:
    httpd.serve_forever()
"
"#,
        success_marker.display()
    );
    std::fs::write(&script_path, script_content).unwrap();

    // Make script executable
    Command::new("chmod")
        .args(["+x", script_path.to_str().unwrap()])
        .status()
        .unwrap();

    let port = find_available_port();

    // Start the proxy - this will fail initially
    let mut process = Command::new(env!("CARGO_BIN_EXE_build-proxy"))
        .args(["--port", &port.to_string()])
        .args(["--pwd", temp_path.to_str().unwrap()])
        .args(["--", script_path.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to start build-proxy");

    // Give it time to attempt and fail the initial build
    sleep(Duration::from_millis(500)).await;

    // Confirm proxy is up
    let initial_server_ready = wait_for_server(port, Duration::from_millis(500)).await;
    assert!(
        initial_server_ready,
        "proxy should be listening even if initial build failed"
    );

    // Now create the success marker so the build will succeed
    std::fs::write(&success_marker, "ok").unwrap();

    // Touch the trigger file to notify the watcher
    std::fs::write(&trigger_file, "trigger").unwrap();

    // Wait for file watcher to detect the change
    sleep(Duration::from_millis(500)).await;

    // Now try to make a request - should trigger a rebuild that succeeds
    let response = reqwest::get(format!("http://127.0.0.1:{}/", port))
        .await
        .expect("request should succeed after rebuild");

    // Clean up
    let _ = process.kill();
    let _ = process.wait();

    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "server should return 200 after successful rebuild"
    );
}

/// Test that a rebuild is attempted when making a request after initial failure,
/// even if no files have changed (the user might have fixed external dependencies)
#[tokio::test]
async fn test_rebuild_on_request_after_initial_failure() {
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    let script_path = temp_path.join("server.sh");
    let success_marker = temp_path.join("success_marker");

    // Script that checks for success marker, fails if not present
    let script_content = format!(
        r#"#!/bin/bash
if [ ! -f "{}" ]; then
    echo "Build failed: success marker not found" >&2
    exit 1
fi

PORT="${{PORT}}"
python3 -c "
import http.server
import socketserver

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header('Content-Type', 'text/plain')
        self.end_headers()
        self.wfile.write(b'hello')
    def log_message(self, format, *args):
        pass

with socketserver.TCPServer(('', $PORT), Handler) as httpd:
    httpd.serve_forever()
"
"#,
        success_marker.display()
    );
    std::fs::write(&script_path, script_content).unwrap();

    Command::new("chmod")
        .args(["+x", script_path.to_str().unwrap()])
        .status()
        .unwrap();

    let port = find_available_port();

    let mut process = Command::new(env!("CARGO_BIN_EXE_build-proxy"))
        .args(["--port", &port.to_string()])
        .args(["--pwd", temp_path.to_str().unwrap()])
        .args(["--", script_path.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to start build-proxy");

    // Wait for proxy to be ready (initial build will fail)
    sleep(Duration::from_millis(500)).await;
    assert!(
        wait_for_server(port, Duration::from_millis(500)).await,
        "proxy should be listening"
    );

    // Fix the build (create success marker) BEFORE making any request
    // This simulates: user fixes external dependency, then refreshes browser
    std::fs::write(&success_marker, "ok").unwrap();

    // Make a request - should trigger a rebuild that succeeds
    let response = reqwest::get(format!("http://127.0.0.1:{}/", port))
        .await
        .expect("request should succeed");

    let _ = process.kill();
    let _ = process.wait();

    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "server should rebuild and return 200 when request comes in after initial failure, even without file changes"
    );
}
