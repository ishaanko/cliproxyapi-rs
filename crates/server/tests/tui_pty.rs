//! Runs the real `cliproxy` binary with `--tui` inside a pseudo-terminal and drives it with
//! keystrokes, reading the screen through a VT100 emulator. Covers `--standalone` (embedded
//! server, no password gate) and client mode (password gate against a separately started server).

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};

const BIN: &str = env!("CARGO_BIN_EXE_cliproxy");

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

fn write_config(dir: &Path, port: u16) -> std::path::PathBuf {
    std::fs::create_dir_all(dir.join("auths")).expect("auth dir");
    std::fs::write(
        dir.join("auths/claude-test@example.com.json"),
        r#"{"type":"claude","email":"test@example.com","access_token":"x","refresh_token":"y","expired":"2099-01-01T00:00:00Z"}"#,
    )
    .expect("auth file");
    let config = dir.join("config.yaml");
    std::fs::write(
        &config,
        format!(
            "host: \"127.0.0.1\"\nport: {port}\nauth-dir: \"{}\"\napi-keys:\n  - \"sk-test-key-0001-abcdefghij\"\nremote-management:\n  secret-key: \"mgmt-secret\"\nlogging-to-file: false\n",
            dir.join("auths").display()
        ),
    )
    .expect("config");
    config
}

/// A child process on a pty with its screen mirrored into a VT100 parser.
struct PtyApp {
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
}

impl PtyApp {
    fn spawn(args: &[&str], cwd: &Path) -> PtyApp {
        let pair = native_pty_system()
            .openpty(PtySize { rows: 36, cols: 120, pixel_width: 0, pixel_height: 0 })
            .expect("openpty");
        let mut cmd = CommandBuilder::new(BIN);
        cmd.args(args);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(cmd).expect("spawn");
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("reader");
        let writer = pair.master.take_writer().expect("writer");
        let parser = Arc::new(Mutex::new(vt100::Parser::new(36, 120, 0)));
        let sink = parser.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().process(&buf[..n]);
            }
        });
        // Keep the master alive for the child's lifetime.
        std::mem::forget(pair.master);
        PtyApp { child, writer, parser }
    }

    fn screen(&self) -> String {
        self.parser.lock().screen().contents()
    }

    fn send(&mut self, bytes: &str) {
        self.writer.write_all(bytes.as_bytes()).expect("write keys");
        self.writer.flush().expect("flush");
    }

    fn wait_for(&self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if self.screen().contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for {needle:?}; screen:\n{}", self.screen());
    }

    fn wait_exit(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                return status.success();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        false
    }

    fn in_alternate_screen(&self) -> bool {
        self.parser.lock().screen().alternate_screen()
    }
}

impl Drop for PtyApp {
    /// Never leave a server or TUI behind when an assertion fails.
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[test]
fn standalone_tui_starts_navigates_and_exits_cleanly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = write_config(dir.path(), free_port());
    let mut app = PtyApp::spawn(&["--config", config.to_str().expect("utf8"), "--tui", "--standalone"], dir.path());

    // No password gate; the embedded server answers the dashboard fetches.
    app.wait_for("● Connected");
    assert!(app.in_alternate_screen());
    let screen = app.screen();
    assert!(screen.contains("Dashboard") && screen.contains("Auth Files (1 active)"), "{screen}");
    assert!(screen.contains("Retry Count:"), "{screen}");

    app.send("\t");
    app.wait_for("Configuration");
    app.send("\t");
    app.wait_for("claude-test@example.c...");
    app.send("\t");
    app.wait_for("Access API Keys (1)");
    app.send("\t");
    app.wait_for("Select a provider");
    app.send("\t");
    app.wait_for("Lines:");

    // `q` does not quit on the Logs tab; Ctrl+C does.
    app.send("q");
    std::thread::sleep(Duration::from_millis(300));
    assert!(app.screen().contains("Lines:"), "q must not quit on the Logs tab");
    app.send("\x03");
    assert!(app.wait_exit(), "TUI exits with status 0");
    assert!(!app.in_alternate_screen(), "alternate screen is released");
}

#[test]
fn client_mode_gates_on_password_then_shows_dashboard() {
    let dir = tempfile::tempdir().expect("tempdir");
    let port = free_port();
    let config = write_config(dir.path(), port);
    let cfg = config.to_str().expect("utf8");

    // A separate server process for the TUI to manage.
    let mut server = PtyApp::spawn(&["--config", cfg], dir.path());
    server.wait_for("API server started successfully");

    let mut tui = PtyApp::spawn(&["--config", cfg, "--tui"], dir.path());
    tui.wait_for("Connect Management API");
    tui.send("wrong\r");
    tui.wait_for("Connection failed: HTTP 401");
    tui.send("\x15mgmt-secret\r");
    tui.wait_for("● Connected");
    assert!(tui.screen().contains(&format!("http://127.0.0.1:{port}")));

    tui.send("q");
    assert!(tui.wait_exit(), "TUI exits with status 0");
    let _ = server.child.kill();
}
