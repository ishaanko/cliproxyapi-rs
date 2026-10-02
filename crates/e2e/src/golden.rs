//! Golden file IO, structural diff and the markdown report.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::scenario::Capture;

pub fn golden_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

pub fn save(dir: &Path, c: &Capture) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut text = serde_json::to_string_pretty(c)?;
    text.push('\n');
    std::fs::write(golden_path(dir, &c.id), text).with_context(|| format!("write golden {}", c.id))
}

pub fn load(dir: &Path, id: &str) -> Result<Option<Capture>> {
    let path = golden_path(dir, id);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(Some(serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?))
}

/// Golden ids present on disk (excluding the report).
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

/// Returns the first difference between the golden and actual capture, if any. Object key order
/// is ignored unless `strict_order` is set.
pub fn diff(golden: &Capture, actual: &Capture, strict_order: bool) -> Option<String> {
    let g = serde_json::to_value(golden).ok()?;
    let a = serde_json::to_value(actual).ok()?;
    diff_value("", &g, &a, strict_order)
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 160 { format!("{}...", s.chars().take(160).collect::<String>()) } else { s }
}

fn diff_value(path: &str, g: &Value, a: &Value, strict: bool) -> Option<String> {
    match (g, a) {
        (Value::Object(go), Value::Object(ao)) => {
            for (k, gv) in go {
                match ao.get(k) {
                    None => return Some(format!("{}: missing in actual (golden {})", join(path, k), short(gv))),
                    Some(av) => {
                        if let Some(d) = diff_value(&join(path, k), gv, av, strict) {
                            return Some(d);
                        }
                    }
                }
            }
            if let Some(k) = ao.keys().find(|k| !go.contains_key(*k)) {
                return Some(format!("{}: unexpected in actual ({})", join(path, k), short(&ao[k])));
            }
            if strict && go.keys().ne(ao.keys()) {
                return Some(format!("{}: key order differs (golden {:?}, actual {:?})", if path.is_empty() { "$" } else { path }, go.keys().collect::<Vec<_>>(), ao.keys().collect::<Vec<_>>()));
            }
            None
        }
        (Value::Array(ga), Value::Array(aa)) => {
            for (i, (gv, av)) in ga.iter().zip(aa).enumerate() {
                if let Some(d) = diff_value(&format!("{path}[{i}]"), gv, av, strict) {
                    return Some(d);
                }
            }
            if ga.len() != aa.len() {
                return Some(format!("{path}: array length golden {} != actual {}", ga.len(), aa.len()));
            }
            None
        }
        _ if g == a => None,
        _ => Some(format!("{}: golden {} != actual {}", if path.is_empty() { "$" } else { path }, short(g), short(a))),
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() { key.to_string() } else { format!("{path}.{key}") }
}

pub struct Outcome {
    pub id: String,
    pub desc: String,
    /// `None` when the scenario passed, otherwise the first difference or error.
    pub failure: Option<String>,
}

/// Markdown report written by `check`: the verifiable artifact.
pub fn report(outcomes: &[Outcome], server: &str, golden_dir: &Path, layout: &str) -> String {
    let pass = outcomes.iter().filter(|o| o.failure.is_none()).count();
    let mut out = String::new();
    out.push_str("# E2E differential report\n\n");
    out.push_str(&format!("- server: `{server}`\n- config layout: `{layout}`\n- goldens digest: `{}`\n", digest(golden_dir)));
    out.push_str(&format!("- scenarios: {} total, {} passed, {} failed\n\n", outcomes.len(), pass, outcomes.len() - pass));
    out.push_str("| scenario | result | what | first difference |\n|---|---|---|---|\n");
    for o in outcomes {
        let (result, detail) = match &o.failure {
            None => ("PASS", String::new()),
            Some(d) => ("FAIL", d.replace('|', "\\|").replace('\n', " ")),
        };
        out.push_str(&format!("| `{}` | {result} | {} | {detail} |\n", o.id, o.desc.replace('|', "\\|")));
    }
    out
}
