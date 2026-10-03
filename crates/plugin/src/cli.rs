//! Plugin-owned command-line flags (Go: `command_line.go`).
//!
//! The embedding CLI asks [`Host::register_command_line_flags`] which flags plugins declare,
//! reports parsed values with [`Host::set_command_line_flag`], and finally lets
//! [`Host::execute_command_line`] run every plugin whose flag was given.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;

use cpa_auth::{FileTokenStore, SaveOptions, Store};
use cpa_pluginapi::abi;
use cpa_pluginapi::api::{
    AuthData, CommandLineExecutionRequest, CommandLineExecutionResponse, CommandLineFlag, CommandLineFlagValue,
    CommandLineRegistrationRequest, CommandLineRegistrationResponse,
};

use crate::convert::{host_config_summary, parse_go_bool};
use crate::ctx::CallCtx;
use crate::host::Host;

#[derive(Debug, Clone, Default)]
pub struct FlagRecord {
    pub plugin_id: String,
    pub flag: CommandLineFlag,
    pub value: String,
    pub set: bool,
}

/// A declared flag the CLI registers.
#[derive(Debug, Clone)]
pub struct DeclaredFlag {
    pub name: String,
    pub usage: String,
    /// `bool`, `string`, `int`, `int64`, `float64` or `duration`.
    pub kind: String,
}

fn valid_flag_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('-') && name != "help" && name != "h" && !name.chars().any(|c| matches!(c, ' ' | '\t' | '\r' | '\n' | '='))
}

fn normalize_flag_type(kind: &str) -> Option<&'static str> {
    match kind.trim().to_lowercase().as_str() {
        "" | "bool" => Some("bool"),
        "string" => Some("string"),
        "int" => Some("int"),
        "int64" => Some("int64"),
        "float64" => Some("float64"),
        "duration" => Some("duration"),
        _ => None,
    }
}

/// Go `time.ParseDuration` rendered with `Duration.String()`.
fn parse_go_duration(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let (neg, mut rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    if rest == "0" {
        return Some("0s".into());
    }
    let mut total_ns: f64 = 0.0;
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let num_end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
        if num_end == 0 {
            return None;
        }
        let value: f64 = rest[..num_end].parse().ok()?;
        rest = &rest[num_end..];
        let unit_end = rest.find(|c: char| c.is_ascii_digit() || c == '.').unwrap_or(rest.len());
        let unit = &rest[..unit_end];
        rest = &rest[unit_end..];
        let mult = match unit {
            "ns" => 1.0,
            "us" | "\u{b5}s" | "\u{3bc}s" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60.0 * 1e9,
            "h" => 3600.0 * 1e9,
            _ => return None,
        };
        total_ns += value * mult;
    }
    let ns = total_ns as i128;
    Some(format_go_duration(if neg { -ns } else { ns }))
}

/// Go `Duration.String()`.
fn format_go_duration(ns: i128) -> String {
    if ns == 0 {
        return "0s".into();
    }
    let neg = ns < 0;
    let u = ns.unsigned_abs();
    let body = if u < 1_000_000_000 {
        if u < 1000 {
            format!("{u}ns")
        } else if u < 1_000_000 {
            format!("{}\u{b5}s", trim_frac(u as f64 / 1e3))
        } else {
            format!("{}ms", trim_frac(u as f64 / 1e6))
        }
    } else {
        let secs_total = u / 1_000_000_000;
        let frac_ns = u % 1_000_000_000;
        let h = secs_total / 3600;
        let m = (secs_total % 3600) / 60;
        let s = secs_total % 60;
        let sec_str = if frac_ns == 0 { format!("{s}s") } else { format!("{}s", trim_frac(s as f64 + frac_ns as f64 / 1e9)) };
        if h > 0 {
            format!("{h}h{m}m{sec_str}")
        } else if m > 0 {
            format!("{m}m{sec_str}")
        } else {
            sec_str
        }
    };
    if neg { format!("-{body}") } else { body }
}

fn trim_frac(v: f64) -> String {
    let s = format!("{v:.9}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Go `normalizeCommandLineFlagValue`: `None` when `value` is invalid for `kind`.
pub fn normalize_flag_value(kind: &str, value: &str) -> Option<String> {
    let blank = value.trim().is_empty();
    match kind {
        "bool" => {
            if blank {
                return Some("false".into());
            }
            parse_go_bool(value).map(|b| b.to_string())
        }
        "string" => Some(value.to_string()),
        "int" | "int64" => {
            if blank {
                return Some("0".into());
            }
            value.parse::<i64>().ok().map(|n| n.to_string())
        }
        "float64" => {
            if blank {
                return Some("0".into());
            }
            value.parse::<f64>().ok().map(format_go_float)
        }
        "duration" => {
            if blank {
                return Some("0s".into());
            }
            parse_go_duration(value)
        }
        _ => None,
    }
}

/// Go `strconv.FormatFloat(v, 'g', -1, 64)`.
fn format_go_float(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e21 {
        return format!("{}", v as i64);
    }
    let s = format!("{v}");
    s
}

impl Host {
    /// Asks every active command-line plugin for its flags and records them (Go:
    /// `RegisterCommandLineFlags`). `existing` holds the names of flags the CLI already owns;
    /// conflicting plugin flags are skipped. Returns the flags the CLI must accept.
    pub async fn register_command_line_flags(self: &Arc<Self>, ctx: &CallCtx, existing: &HashSet<String>) -> Vec<DeclaredFlag> {
        let mut declared = Vec::new();
        for rec in self.active_records() {
            if !rec.caps().command_line_plugin || self.is_plugin_fused(&rec.id) || !self.usable(&rec) {
                continue;
            }
            let req = CommandLineRegistrationRequest { plugin: rec.meta.clone() };
            let resp: CommandLineRegistrationResponse = match self.rpc(&rec, ctx, abi::METHOD_COMMAND_LINE_REGISTER, &req).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("pluginhost: command-line registrar {} failed: {e}", rec.id);
                    continue;
                }
            };
            for item in resp.flags {
                if let Some(d) = self.register_command_line_flag(existing, &rec.id, item) {
                    declared.push(d);
                }
            }
        }
        declared
    }

    fn register_command_line_flag(&self, existing: &HashSet<String>, plugin_id: &str, item: CommandLineFlag) -> Option<DeclaredFlag> {
        let name = item.name.trim().to_string();
        if !valid_flag_name(&name) {
            tracing::warn!("pluginhost: plugin {plugin_id} declared invalid command-line flag {:?}", item.name);
            return None;
        }
        let Some(kind) = normalize_flag_type(&item.kind) else {
            tracing::warn!("pluginhost: plugin {plugin_id} declared unsupported command-line flag type {:?} for {name}", item.kind);
            return None;
        };
        let Some(value) = normalize_flag_value(kind, &item.default_value) else {
            tracing::warn!("pluginhost: plugin {plugin_id} declared invalid default value {:?} for {name}", item.default_value);
            return None;
        };
        if existing.contains(&name) {
            tracing::warn!("pluginhost: plugin {plugin_id} command-line flag {name} conflicts with an existing flag and was skipped");
            return None;
        }
        let mut st = self.state.lock();
        if st.command_line_flags.contains_key(&name) {
            tracing::warn!("pluginhost: plugin {plugin_id} command-line flag {name} conflicts with a higher-priority plugin and was skipped");
            return None;
        }
        st.command_line_flags.insert(
            name.clone(),
            FlagRecord {
                plugin_id: plugin_id.to_string(),
                flag: CommandLineFlag { name: name.clone(), usage: item.usage.clone(), kind: kind.to_string(), default_value: value.clone() },
                value,
                set: false,
            },
        );
        Some(DeclaredFlag { name, usage: item.usage, kind: kind.to_string() })
    }

    /// Current value of a plugin flag (Go: `commandLineFlagValue.String`).
    pub fn command_line_flag_default(&self, name: &str) -> String {
        self.state.lock().command_line_flags.get(name).map(|r| r.value.clone()).unwrap_or_default()
    }

    /// Records a parsed plugin flag value (Go: `commandLineFlagValue.Set`); `Err` carries the Go
    /// flag error text.
    pub fn set_command_line_flag(&self, name: &str, raw: &str) -> Result<(), String> {
        let mut st = self.state.lock();
        let Some(record) = st.command_line_flags.get(name).cloned() else { return Ok(()) };
        let Some(normalized) = normalize_flag_value(&record.flag.kind, raw) else {
            return Err(format!("invalid {} value {raw:?}", record.flag.kind));
        };
        let mut record = record;
        record.value = normalized;
        record.set = true;
        st.command_line_flags.insert(name.to_string(), record);
        st.command_line_hits.insert(name.to_string());
        Ok(())
    }

    /// Whether any plugin-owned flag was provided (Go: `HasTriggeredCommandLineFlags`).
    pub fn has_triggered_command_line_flags(&self) -> bool {
        !self.state.lock().command_line_hits.is_empty()
    }

    /// Runs every plugin whose flags were provided (Go: `ExecuteCommandLine`). `builtin_flags`
    /// are the CLI's own flags as `(name, value, set)`. Returns `(exit_code, handled)`.
    pub async fn execute_command_line(
        self: &Arc<Self>,
        ctx: &CallCtx,
        program: &str,
        args: &[String],
        config_path: &str,
        builtin_flags: &[(String, String, bool)],
    ) -> (i32, bool) {
        let (triggered_by_plugin, all_flags) = self.command_line_execution_state(builtin_flags);
        if triggered_by_plugin.is_empty() {
            return (0, false);
        }
        let mut exit_code = 0;
        let mut handled = false;
        for rec in self.active_records() {
            if !rec.caps().command_line_plugin || self.is_plugin_fused(&rec.id) {
                continue;
            }
            let Some(triggered) = triggered_by_plugin.get(&rec.id).filter(|t| !t.is_empty()) else { continue };
            handled = true;
            if !self.usable(&rec) {
                // A stale plugin answers with an empty response.
                continue;
            }
            let req = CommandLineExecutionRequest {
                plugin: rec.meta.clone(),
                program: program.to_string(),
                args: args.to_vec(),
                config_path: config_path.to_string(),
                host: host_config_summary(self.runtime_config().as_deref()),
                flags: all_flags.clone(),
                triggered_flags: triggered.clone(),
            };
            let mut resp: CommandLineExecutionResponse = match self.rpc_cb(&rec, ctx, abi::METHOD_COMMAND_LINE_EXECUTE, &req).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("pluginhost: command-line plugin {} failed: {e}", rec.id);
                    if exit_code == 0 {
                        exit_code = 1;
                    }
                    continue;
                }
            };
            if resp.exit_code == 0 && !resp.auths.is_empty() {
                match self.persist_command_line_auths(&resp.auths) {
                    Ok(paths) => resp.stdout = append_saved_paths(resp.stdout, &paths),
                    Err(e) => {
                        write_output(&mut std::io::stdout(), &resp.stdout);
                        write_output(&mut std::io::stderr(), &resp.stderr);
                        write_output(&mut std::io::stderr(), format!("{e}\n").as_bytes());
                        if exit_code == 0 {
                            exit_code = 1;
                        }
                        continue;
                    }
                }
            }
            write_output(&mut std::io::stdout(), &resp.stdout);
            write_output(&mut std::io::stderr(), &resp.stderr);
            if resp.exit_code != 0 && exit_code == 0 {
                exit_code = i32::try_from(resp.exit_code).unwrap_or(1);
            }
        }
        (exit_code, handled)
    }

    #[allow(clippy::type_complexity)]
    fn command_line_execution_state(
        &self,
        builtin_flags: &[(String, String, bool)],
    ) -> (HashMap<String, BTreeMap<String, CommandLineFlagValue>>, BTreeMap<String, CommandLineFlagValue>) {
        let mut triggered: HashMap<String, BTreeMap<String, CommandLineFlagValue>> = HashMap::new();
        let mut all: BTreeMap<String, CommandLineFlagValue> = BTreeMap::new();
        for (name, value, _set) in builtin_flags {
            all.insert(name.clone(), CommandLineFlagValue { name: name.clone(), kind: String::new(), value: value.clone(), set: false });
        }
        let st = self.state.lock();
        for (name, record) in &st.command_line_flags {
            let value = CommandLineFlagValue {
                name: name.clone(),
                kind: record.flag.kind.clone(),
                value: record.value.clone(),
                set: record.set,
            };
            all.insert(name.clone(), value.clone());
            if st.command_line_hits.contains(name) {
                triggered.entry(record.plugin_id.clone()).or_default().insert(name.clone(), value);
            }
        }
        (triggered, all)
    }

    /// Go `persistCommandLineAuths`: saves the auths a command created into the auth dir.
    fn persist_command_line_auths(&self, auths: &[AuthData]) -> Result<Vec<String>, String> {
        let store = FileTokenStore::new();
        let summary = host_config_summary(self.runtime_config().as_deref());
        if !summary.auth_dir.is_empty() {
            store.set_base_dir(&summary.auth_dir);
        }
        let mut saved = Vec::new();
        for (i, data) in auths.iter().enumerate() {
            let Some(mut record) = self.auth_data_to_core_auth(data, "", "") else {
                return Err(format!("pluginhost: command-line auth {} is invalid", i + 1));
            };
            match store.save(&mut record, SaveOptions::default()) {
                Ok(Some(path)) => {
                    let p = path.to_string_lossy().trim().to_string();
                    if !p.is_empty() {
                        saved.push(p);
                    }
                }
                Ok(None) => {}
                Err(e) => return Err(format!("pluginhost: save command-line auth {}: {e}", record.id)),
            }
        }
        Ok(saved)
    }
}

fn append_saved_paths(stdout: Vec<u8>, paths: &[String]) -> Vec<u8> {
    if paths.is_empty() {
        return stdout;
    }
    let mut out = stdout;
    if out.last().is_some_and(|b| *b != b'\n') {
        out.push(b'\n');
    }
    for p in paths {
        if p.trim().is_empty() {
            continue;
        }
        out.extend_from_slice(format!("Authentication saved to {p}\n").as_bytes());
    }
    out
}

fn write_output(w: &mut dyn Write, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    if let Err(e) = w.write_all(data) {
        tracing::warn!("pluginhost: failed to write command-line plugin output: {e}");
    }
}
