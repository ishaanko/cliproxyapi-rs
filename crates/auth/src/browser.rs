//! Opening the system browser and the SSH-tunnel hint shown on headless hosts
//! (internal/browser, internal/util/ssh_helper.go).

use std::path::PathBuf;
use std::process::{Command, Stdio};

const LINUX_BROWSERS: [&str; 6] = ["xdg-open", "x-www-browser", "www-browser", "firefox", "chromium", "google-chrome"];

fn on_path(cmd: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).map(|p| p.join(cmd)).find(|p| p.is_file())
}

/// The command used to open URLs on this platform, when one exists.
fn opener() -> Option<(PathBuf, Vec<&'static str>)> {
    match std::env::consts::OS {
        "macos" => on_path("open").map(|p| (p, vec![])),
        "windows" => on_path("rundll32").map(|p| (p, vec!["url.dll,FileProtocolHandler"])),
        "linux" => LINUX_BROWSERS.iter().find_map(|b| on_path(b)).map(|p| (p, vec![])),
        _ => None,
    }
}

/// True when a browser opener is installed. A headless SSH session usually has none.
pub fn is_available() -> bool {
    opener().is_some()
}

/// Opens `url` in the default browser. The browser process is detached.
pub fn open_url(url: &str) -> Result<(), String> {
    let (program, args) = opener().ok_or_else(|| "no suitable browser found".to_string())?;
    Command::new(program)
        .args(args)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("failed to start browser command: {e}"))
}

/// Text telling a remote user how to tunnel the callback port to their machine.
pub fn ssh_tunnel_instructions(port: u16, host_ip: &str) -> String {
    let border = "=".repeat(80);
    format!(
        "To authenticate from a remote machine, an SSH tunnel may be required.\n{border}\n  Run one of the following commands on your local machine (NOT the server):\n\n  # Standard SSH command (assumes SSH port 22):\n  ssh -L {port}:127.0.0.1:{port} root@{host_ip} -p 22\n\n  # If using an SSH key (assumes SSH port 22):\n  ssh -i <path_to_your_key> -L {port}:127.0.0.1:{port} root@{host_ip} -p 22\n\n  NOTE: If your server's SSH port is not 22, please modify the '-p 22' part accordingly.\n{border}\n"
    )
}

/// Address to show in the tunnel hint: the outbound interface address, else loopback. (The Go
/// helper first asks public IP services; this port avoids phoning home for a hint.)
pub fn outbound_ip() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("8.8.8.8:80").map(|_| s))
        .and_then(|s| s.local_addr())
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_hint_mentions_port_and_host() {
        let t = ssh_tunnel_instructions(1455, "203.0.113.9");
        assert!(t.contains("ssh -L 1455:127.0.0.1:1455 root@203.0.113.9 -p 22"));
        assert!(t.contains("ssh -i <path_to_your_key> -L 1455:127.0.0.1:1455"));
    }
}
