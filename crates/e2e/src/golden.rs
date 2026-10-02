//! Golden file IO, volatile-path learning, structural diff and the markdown report.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::scenario::Capture;

/// Replacement for leaves that `record` saw differ between two runs of the same scenario.
const VOLATILE: &str = "<volatile>";
/// Differences listed per failing scenario.
const MAX_DIFFS: usize = 6;

pub fn golden_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn write_pretty(path: &Path, c: &Capture) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = serde_json::to_string_pretty(c)?;
    text.push('\n');
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

pub fn save(dir: &Path, c: &Capture) -> Result<()> {
    write_pretty(&golden_path(dir, &c.id), c)
}

/// Failing captures are kept under `<golden dir>/actual/` for inspection (gitignored).
pub fn save_actual(dir: &Path, c: &Capture) -> Result<()> {
    write_pretty(&dir.join("actual").join(format!("{}.json", c.id)), c)
}

pub fn clear_actual(dir: &Path, id: &str) {
    let _ = std::fs::remove_file(dir.join("actual").join(format!("{id}.json")));
}

pub fn load(dir: &Path, id: &str) -> Result<Option<Capture>> {
    let path = golden_path(dir, id);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(Some(serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?))
}

/// Golden ids present on disk (excluding the report and `actual/`).
pub fn list_ids(dir: &Path) -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".json")).map(str::to_string))
        .collect();
    ids.sort();
    ids
}

/// Digest over all golden files, so a report identifies exactly what it was checked against.
pub fn digest(dir: &Path) -> String {
    let mut h = Sha256::new();
    for id in list_ids(dir) {
        h.update(id.as_bytes());
        if let Ok(bytes) = std::fs::read(golden_path(dir, &id)) {
            h.update(&bytes);
        }
    }
    h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect()
}

// ------------------------------------------------------------ volatile learning

fn pointer_escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Compares two runs of one scenario. Leaves whose scalar values differ are recorded (as JSON
/// pointers) and masked; any structural difference (keys, array lengths) is an error because
/// it cannot be masked.
fn learn_walk(path: &str, a: &Value, b: &Value, out: &mut Vec<String>) -> Result<(), String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            if x.keys().ne(y.keys()) {
                return Err(format!("{}: keys differ between runs", if path.is_empty() { "/" } else { path }));
            }
            for (k, xv) in x {
                learn_walk(&format!("{path}/{}", pointer_escape(k)), xv, &y[k], out)?;
            }
            Ok(())
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                return Err(format!("{path}: array length {} vs {} between runs", x.len(), y.len()));
            }
            for (i, (xv, yv)) in x.iter().zip(y).enumerate() {
                learn_walk(&format!("{path}/{i}"), xv, yv, out)?;
            }
            Ok(())
        }
        (Value::Object(_) | Value::Array(_), _) | (_, Value::Object(_) | Value::Array(_)) => Err(format!("{path}: type differs between runs")),
        _ => {
            if a != b {
                out.push(path.to_string());
            }
            Ok(())
        }
    }
}

fn mask(v: &mut Value, pointers: &[String]) {
    for p in pointers {
        if let Some(leaf) = v.pointer_mut(p) {
            *leaf = Value::String(VOLATILE.into());
        }
    }
}

/// Merges two runs into the golden: the first run with differing leaves masked. The mask set is
/// stored in the capture so `check` applies the same masks to the actual output.
pub fn learn_volatile(first: &Capture, second: &Capture) -> Result<Capture, String> {
    let a = serde_json::to_value(first).map_err(|e| e.to_string())?;
    let mut b = serde_json::to_value(second).map_err(|e| e.to_string())?;
    // Paths already learned from earlier runs stay masked.
    mask(&mut b, &first.volatile);
    let mut paths = first.volatile.clone();
    learn_walk("", &a, &b, &mut paths)?;
    let mut merged = a;
    mask(&mut merged, &paths);
    mask(&mut b, &paths);
    paths.sort();
    paths.dedup();
    let mut out: Capture = serde_json::from_value(merged).map_err(|e| e.to_string())?;
    out.volatile = paths;
    Ok(out)
}

// ----------------------------------------------------------------------- diff

/// Differences between golden and actual (empty when equal). The golden's volatile paths are
/// masked in the actual capture first. Object key order matters when `strict_order` is set.
pub fn diff(golden: &Capture, actual: &Capture, strict_order: bool) -> Vec<String> {
    let (g, mut a) = match (serde_json::to_value(golden), serde_json::to_value(actual)) {
        (Ok(g), Ok(a)) => (g, a),
        (g, a) => return vec![format!("serialization error: {:?} {:?}", g.err(), a.err())],
    };
    mask(&mut a, &golden.volatile);
    // The mask list is metadata of the golden, not part of the behavior.
    let mut g = g;
    for v in [&mut g, &mut a] {
        if let Value::Object(m) = v {
            m.shift_remove("volatile");
        }
    }
    let mut out = vec![];
    diff_value("", &g, &a, strict_order, &mut out);
    out
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 160 { format!("{}...", s.chars().take(160).collect::<String>()) } else { s }
}

fn join(path: &str, key: &str) -> String {
    format!("{path}/{}", pointer_escape(key))
}

fn diff_value(path: &str, g: &Value, a: &Value, strict: bool, out: &mut Vec<String>) {
    if out.len() >= MAX_DIFFS {
        return;
    }
    let here = if path.is_empty() { "/" } else { path };
    match (g, a) {
        (Value::Object(go), Value::Object(ao)) => {
            for (k, gv) in go {
                match ao.get(k) {
                    None => out.push(format!("{}: missing in actual (golden {})", join(path, k), short(gv))),
                    Some(av) => diff_value(&join(path, k), gv, av, strict, out),
                }
            }
            for k in ao.keys().filter(|k| !go.contains_key(*k)) {
                out.push(format!("{}: unexpected in actual ({})", join(path, k), short(&ao[k])));
            }
            let same_keys = go.len() == ao.len() && go.keys().all(|k| ao.contains_key(k));
            if strict && same_keys && go.keys().ne(ao.keys()) {
                out.push(format!("{here}: key order differs (golden {:?}, actual {:?})", go.keys().collect::<Vec<_>>(), ao.keys().collect::<Vec<_>>()));
            }
        }
        (Value::Array(ga), Value::Array(aa)) => {
            for (i, (gv, av)) in ga.iter().zip(aa).enumerate() {
                diff_value(&format!("{path}/{i}"), gv, av, strict, out);
            }
            if ga.len() != aa.len() {
                out.push(format!("{here}: array length golden {} != actual {}", ga.len(), aa.len()));
            }
        }
        _ if g == a => {}
        _ => out.push(format!("{here}: golden {} != actual {}", short(g), short(a))),
    }
    out.truncate(MAX_DIFFS);
}

// --------------------------------------------------------------------- report

pub struct Outcome {
    pub id: String,
    pub desc: String,
    /// Empty when the scenario passed; otherwise the differences or the error.
    pub failures: Vec<String>,
}

impl Outcome {
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Markdown report written by `check`: the verifiable artifact.
pub fn report(outcomes: &[Outcome], server: &str, golden_dir: &Path, layout: &str) -> String {
    let pass = outcomes.iter().filter(|o| o.passed()).count();
    let mut out = String::new();
    out.push_str("# E2E differential report\n\n");
    out.push_str(&format!("- server: `{server}`\n- config layout: `{layout}`\n- goldens digest: `{}`\n", digest(golden_dir)));
    out.push_str(&format!("- scenarios: {} total, {} passed, {} failed\n\n", outcomes.len(), pass, outcomes.len() - pass));
    out.push_str("| scenario | result | what | first difference |\n|---|---|---|---|\n");
    for o in outcomes {
        let (result, first) = match o.failures.first() {
            None => ("PASS", String::new()),
            Some(d) => ("FAIL", d.replace('|', "\\|").replace('\n', " ")),
        };
        out.push_str(&format!("| `{}` | {result} | {} | {first} |\n", o.id, o.desc.replace('|', "\\|")));
    }
    let failing: Vec<&Outcome> = outcomes.iter().filter(|o| !o.passed()).collect();
    if !failing.is_empty() {
        out.push_str("\n## Failures\n");
        for o in failing {
            out.push_str(&format!("\n### `{}`\n\n", o.id));
            for d in &o.failures {
                out.push_str(&format!("- {}\n", d.replace('\n', " ")));
            }
        }
    }
    out
}
