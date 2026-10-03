//! `-discover` flags and the `discover` subcommand (Go: cmd/server/main.go discover branches).

use std::time::Duration;

use cpa_discovery::scan::{
    DiscoverOptions, do_discover_with_options, load_discovery_scan_filters, parse_interface_list,
    resolve_discovery_interface_filters,
};

use crate::BuildInfo;
use crate::cli::Cli;

/// `-discover` / `-discover-json`: scans the LAN before any config is loaded. Returns the exit code.
pub async fn run_flags(cli: &Cli) -> i32 {
    let (cfg_include, cfg_exclude) = load_discovery_scan_filters(&cli.config);
    let (include, exclude) = resolve_discovery_interface_filters(&cli.discover_include, &cli.discover_exclude, &cfg_include, &cfg_exclude);
    do_discover_with_options(DiscoverOptions {
        timeout: seconds(cli.discover_timeout),
        json_output: cli.discover_json,
        service_type: cli.discover_service_type.clone(),
        include,
        exclude,
    })
    .await
}

/// Non-positive values become zero, which the scan turns into its 3s default.
fn seconds(n: i64) -> Duration {
    Duration::from_secs(u64::try_from(n).unwrap_or(0))
}

const SUBCOMMAND_USAGE: &str = "Usage of discover:
  -config string
    \tConfigure File Path
  -exclude value
    \tComma-separated interface names to skip
  -include value
    \tComma-separated interface names to scan (overrides default physical LAN filter)
  -json
    \tOutput in JSON format
  -service-type string
    \tDNS-SD service type (default _ai-gateway._tcp)
  -timeout int
    \tDiscovery timeout in seconds (default 3)
";

#[derive(Default)]
struct SubcommandArgs {
    timeout: i64,
    json: bool,
    service_type: String,
    config: String,
    include: Vec<String>,
    exclude: Vec<String>,
}

enum Parsed {
    Run(SubcommandArgs),
    Help,
    Error(String),
}

/// Go `flag.NewFlagSet("discover", flag.ExitOnError).Parse(args)`.
fn parse_subcommand(args: &[String]) -> Parsed {
    let mut out = SubcommandArgs { timeout: 3, ..Default::default() };
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" || arg == "-" || !arg.starts_with('-') {
            break;
        }
        let trimmed = arg.trim_start_matches('-');
        if arg.starts_with("---") || trimmed.is_empty() || trimmed.starts_with('=') {
            return Parsed::Error(format!("bad flag syntax: {arg}"));
        }
        let (name, inline) = match trimmed.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (trimmed, None),
        };
        i += 1;
        if name == "h" || name == "help" {
            return Parsed::Help;
        }
        if name == "json" {
            out.json = match inline.as_deref() {
                None => true,
                Some("1" | "t" | "T" | "true" | "TRUE" | "True") => true,
                Some("0" | "f" | "F" | "false" | "FALSE" | "False") => false,
                Some(v) => return Parsed::Error(format!("invalid boolean value {v:?} for -json: parse error")),
            };
            continue;
        }
        if !matches!(name, "timeout" | "service-type" | "config" | "include" | "exclude") {
            return Parsed::Error(format!("flag provided but not defined: -{name}"));
        }
        let value = match inline {
            Some(v) => v,
            None => {
                if i >= args.len() {
                    return Parsed::Error(format!("flag needs an argument: -{name}"));
                }
                i += 1;
                args[i - 1].clone()
            }
        };
        match name {
            "timeout" => match value.parse::<i64>() {
                Ok(n) => out.timeout = n,
                Err(e) => return Parsed::Error(format!("invalid value {value:?} for flag -timeout: parse error ({e})")),
            },
            "service-type" => out.service_type = value,
            "config" => out.config = value,
            "include" => out.include.extend(parse_interface_list(&[value])),
            _ => out.exclude.extend(parse_interface_list(&[value])),
        }
    }
    Parsed::Run(out)
}

/// `cliproxy discover [flags]`: the banner goes to stderr so `-json` keeps stdout clean.
pub async fn run_subcommand(args: &[String], build: &BuildInfo) -> i32 {
    let parsed = match parse_subcommand(args) {
        Parsed::Run(p) => p,
        Parsed::Help => {
            eprint!("{SUBCOMMAND_USAGE}");
            return 0;
        }
        Parsed::Error(msg) => {
            eprintln!("{msg}");
            eprint!("{SUBCOMMAND_USAGE}");
            return 2;
        }
    };
    if !parsed.json {
        eprintln!("CLIProxyAPI Version: {}, Commit: {}, BuiltAt: {}", build.version, build.commit, build.build_date);
    }
    let (cfg_include, cfg_exclude) = load_discovery_scan_filters(&parsed.config);
    let (include, exclude) = resolve_discovery_interface_filters(&parsed.include, &parsed.exclude, &cfg_include, &cfg_exclude);
    do_discover_with_options(DiscoverOptions {
        timeout: seconds(parsed.timeout),
        json_output: parsed.json,
        service_type: parsed.service_type,
        include,
        exclude,
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn subcommand_flags_parse() {
        let Parsed::Run(p) = parse_subcommand(&args("-timeout 5 --json -service-type=_x._tcp -include en0,eth0 -include wlan0 -exclude=docker0")) else {
            panic!("expected run");
        };
        assert_eq!((p.timeout, p.json, p.service_type.as_str()), (5, true, "_x._tcp"));
        assert_eq!(p.include, ["en0", "eth0", "wlan0"]);
        assert_eq!(p.exclude, ["docker0"]);
        assert!(matches!(parse_subcommand(&args("-bogus")), Parsed::Error(m) if m == "flag provided but not defined: -bogus"));
        assert!(matches!(parse_subcommand(&args("-h")), Parsed::Help));
    }
}
