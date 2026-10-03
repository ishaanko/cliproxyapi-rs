//! Docker-backed fixtures. Tests using them skip unless `CPA_STORE_DOCKER=1`, and also skip when
//! the docker CLI cannot start a container.
#![allow(dead_code)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub fn docker_enabled() -> bool {
    std::env::var("CPA_STORE_DOCKER").is_ok_and(|v| v == "1")
}

/// A running container, removed on drop.
pub struct Container {
    pub name: String,
    pub port: u16,
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn unique(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{prefix}-{}-{nanos}", std::process::id())
}

/// `docker run -d` with `args`, publishing `inner_port` on a random localhost port.
fn run(prefix: &str, inner_port: u16, args: &[&str]) -> Option<Container> {
    let name = unique(prefix);
    let publish = format!("127.0.0.1::{inner_port}");
    let out = Command::new("docker")
        .args(["run", "-d", "--name", &name, "-p", &publish])
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!("docker run failed: {}", String::from_utf8_lossy(&out.stderr));
        return None;
    }
    let mut container = Container { name: name.clone(), port: 0 };
    let port_out = Command::new("docker")
        .args(["port", &name, &inner_port.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&port_out.stdout);
    let port: u16 = text.lines().next()?.rsplit(':').next()?.trim().parse().ok()?;
    container.port = port;
    Some(container)
}

fn wait_for(what: &str, timeout: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if ready() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    eprintln!("timed out waiting for {what}");
    false
}

/// A throwaway Postgres; returns the container and a DSN.
pub fn postgres() -> Option<(Container, String)> {
    if !docker_enabled() {
        return None;
    }
    let c = run("cpa-pg", 5432, &["-e", "POSTGRES_PASSWORD=pw", "-e", "POSTGRES_DB=cpa", "postgres:16-alpine"])?;
    let name = c.name.clone();
    let ok = wait_for("postgres", Duration::from_secs(60), || {
        Command::new("docker")
            .args(["exec", &name, "pg_isready", "-U", "postgres", "-d", "cpa", "-h", "127.0.0.1"])
            .output()
            .is_ok_and(|o| o.status.success())
    });
    if !ok {
        return None;
    }
    // pg_isready can answer during the init-time server restart; let it settle.
    std::thread::sleep(Duration::from_secs(3));
    let dsn = format!("postgres://postgres:pw@127.0.0.1:{}/cpa?sslmode=disable", c.port);
    Some((c, dsn))
}

pub const S3_ACCESS_KEY: &str = "testak";
pub const S3_SECRET_KEY: &str = "testsecretkey123";

/// A throwaway SeaweedFS S3 gateway (signature-checked); returns the container and `host:port`.
pub fn s3(dir: &std::path::Path) -> Option<(Container, String)> {
    if !docker_enabled() {
        return None;
    }
    let cfg = dir.join("s3.json");
    std::fs::write(
        &cfg,
        format!(
            r#"{{"identities":[{{"name":"t","credentials":[{{"accessKey":"{S3_ACCESS_KEY}","secretKey":"{S3_SECRET_KEY}"}}],"actions":["Admin","Read","Write","List","Tagging"]}}]}}"#
        ),
    )
    .ok()?;
    let mount = format!("{}:/s3.json", cfg.display());
    let c = run(
        "cpa-s3",
        8333,
        &["-v", &mount, "chrislusf/seaweedfs:latest", "server", "-s3", "-s3.config=/s3.json", "-dir=/data", "-volume.max=5"],
    )?;
    let endpoint = format!("127.0.0.1:{}", c.port);
    let url = format!("http://{endpoint}/");
    let ok = wait_for("s3 gateway", Duration::from_secs(90), || {
        Command::new("curl")
            .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", &url])
            .output()
            .is_ok_and(|o| o.stdout == b"403")
    });
    if !ok {
        return None;
    }
    // Volume servers need a moment to register before buckets accept writes.
    std::thread::sleep(Duration::from_secs(5));
    Some((c, endpoint))
}
