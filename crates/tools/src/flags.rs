//! A small stand-in for Go's `flag` package (ExitOnError semantics): `-name` and `--name`,
//! `-name=value`, `-name value` for strings and bare or `=bool` for booleans, parsing stops at the
//! first non-flag argument or `--`.

use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Str,
    Bool,
}

pub struct FlagDef {
    pub name: &'static str,
    pub kind: Kind,
    pub default: &'static str,
    pub usage: &'static str,
}

/// Parsed flag values with Go's `flag.Visit` (explicitly set) information.
pub struct Flags {
    values: HashMap<&'static str, String>,
    visited: HashSet<&'static str>,
}

/// How a failed parse ends the program.
#[derive(Debug, PartialEq, Eq)]
pub struct Exit(pub i32);

impl Flags {
    pub fn string(&self, name: &str) -> String {
        self.values.get(name).cloned().unwrap_or_default()
    }

    pub fn boolean(&self, name: &str) -> bool {
        self.values.get(name).is_some_and(|v| v == "true")
    }

    /// True when the flag appeared on the command line.
    pub fn was_set(&self, name: &str) -> bool {
        self.visited.contains(name)
    }
}

/// `strconv.ParseBool`.
fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

fn usage(prog: &str, defs: &[FlagDef]) {
    eprintln!("Usage of {prog}:");
    let sorted: BTreeMap<&str, &FlagDef> = defs.iter().map(|d| (d.name, d)).collect();
    for def in sorted.values() {
        let type_name = if def.kind == Kind::Str { " string" } else { "" };
        eprintln!("  -{}{}", def.name, type_name);
        let zero = match def.kind {
            Kind::Str => def.default.is_empty(),
            Kind::Bool => def.default == "false",
        };
        if zero {
            eprintln!("    \t{}", def.usage);
        } else if def.kind == Kind::Str {
            eprintln!("    \t{} (default {:?})", def.usage, def.default);
        } else {
            eprintln!("    \t{} (default {})", def.usage, def.default);
        }
    }
}

/// Parses `args` (without the program name). On a bad flag the Go error and usage text go to
/// stderr and `Err(Exit(2))` is returned (`-h`/`-help` gives `Exit(0)`).
pub fn parse(prog: &str, defs: &[FlagDef], args: &[String]) -> Result<Flags, Exit> {
    let mut flags = Flags {
        values: defs.iter().map(|d| (d.name, d.default.to_string())).collect(),
        visited: HashSet::new(),
    };
    let fail = |msg: String| -> Exit {
        eprintln!("{msg}");
        usage(prog, defs);
        Exit(2)
    };
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg.len() < 2 || !arg.starts_with('-') {
            break;
        }
        let mut name = &arg[1..];
        if let Some(stripped) = name.strip_prefix('-') {
            if stripped.is_empty() {
                break;
            }
            name = stripped;
        }
        if name.starts_with('-') || name.starts_with('=') {
            return Err(fail(format!("bad flag syntax: {arg}")));
        }
        let (name, inline) = match name.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (name, None),
        };
        let Some(def) = defs.iter().find(|d| d.name == name) else {
            if name == "help" || name == "h" {
                usage(prog, defs);
                return Err(Exit(0));
            }
            return Err(fail(format!("flag provided but not defined: -{name}")));
        };
        let value = match def.kind {
            Kind::Bool => match inline {
                None => "true".to_string(),
                Some(v) => match parse_bool(&v) {
                    Some(b) => b.to_string(),
                    None => return Err(fail(format!("invalid boolean value {v:?} for -{name}: parse error"))),
                },
            },
            Kind::Str => match inline.or_else(|| rest.next().cloned()) {
                Some(v) => v,
                None => return Err(fail(format!("flag needs an argument: -{name}"))),
            },
        };
        flags.values.insert(def.name, value);
        flags.visited.insert(def.name);
    }
    Ok(flags)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFS: &[FlagDef] = &[
        FlagDef { name: "output", kind: Kind::Str, default: "out.json", usage: "o" },
        FlagDef { name: "auths-dir", kind: Kind::Str, default: "", usage: "a" },
        FlagDef { name: "pretty", kind: Kind::Bool, default: "true", usage: "p" },
    ];

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_go_style_flags() {
        let f = parse("t", DEFS, &args(&["-output", "x.json", "--auths-dir=/a", "-pretty=false"])).unwrap();
        assert_eq!(f.string("output"), "x.json");
        assert_eq!(f.string("auths-dir"), "/a");
        assert!(!f.boolean("pretty"));
        assert!(f.was_set("auths-dir"));

        let f = parse("t", DEFS, &args(&[])).unwrap();
        assert_eq!(f.string("output"), "out.json");
        assert!(f.boolean("pretty"));
        assert!(!f.was_set("output"));
        assert!(parse("t", DEFS, &args(&["-pretty"])).unwrap().boolean("pretty"));
    }

    #[test]
    fn rejects_unknown_and_incomplete_flags() {
        assert_eq!(parse("t", DEFS, &args(&["-nope"])).err(), Some(Exit(2)));
        assert_eq!(parse("t", DEFS, &args(&["-output"])).err(), Some(Exit(2)));
        assert_eq!(parse("t", DEFS, &args(&["-pretty=maybe"])).err(), Some(Exit(2)));
        assert_eq!(parse("t", DEFS, &args(&["-h"])).err(), Some(Exit(0)));
        // Parsing stops at the first positional argument.
        let f = parse("t", DEFS, &args(&["pos", "-output", "x"])).unwrap();
        assert_eq!(f.string("output"), "out.json");
    }
}
