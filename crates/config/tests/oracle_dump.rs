//! Differential testing against the Go implementation (see `oracle/dump.go`).
//!
//! For every file in `oracle/corpus` this writes a JSON record of: the parsed config, v8
//! validation, the migrated config, and the config after load/save round trips (with and
//! without migration). `oracle/compare.py` diffs it against the Go dumper's records.
//!
//! Run: `CPA_CONFIG_ORACLE_OUT=/tmp/rust-out cargo test -p cpa-config --test oracle_dump -- --ignored`

use std::path::{Path, PathBuf};

use cpa_config::*;
use serde::Deserialize;
use serde_json::{Value as Json, json};

fn snapshot(cfg: &Config) -> Json {
    serde_json::to_value(cfg.to_yaml_value().expect("serialisable config")).expect("json value")
}

/// A file's first YAML document as JSON (merge keys applied, like yaml.v3 decoding into `any`).
fn file_json(path: &Path) -> Json {
    let text = std::fs::read_to_string(path).expect("readable file");
    let Some(doc) = serde_yaml_ng::Deserializer::from_str(&text).next() else {
        return json!({});
    };
    match serde_yaml_ng::Value::deserialize(doc) {
        Ok(mut value) => {
            let _ = value.apply_merge();
            if value.is_null() {
                json!({})
            } else {
                serde_json::to_value(value).expect("json value")
            }
        }
        Err(_) => json!({"__error": true}),
    }
}

#[test]
#[ignore = "writes differential-test output; set CPA_CONFIG_ORACLE_OUT"]
fn dump_oracle_records() {
    let out = PathBuf::from(std::env::var("CPA_CONFIG_ORACLE_OUT").expect("CPA_CONFIG_ORACLE_OUT"));
    std::fs::create_dir_all(&out).unwrap();
    let work = tempfile::tempdir().unwrap();
    let error = json!({"__error": true});
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("oracle/corpus");
    for entry in std::fs::read_dir(corpus).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let raw = std::fs::read(&path).unwrap();
        let mut record = serde_json::Map::new();
        let parsed = parse_config_bytes(&raw);
        if let Ok(cfg) = &parsed {
            record.insert("json".into(), cfg.to_json_value().expect("json view"));
        }
        record.insert(
            "parse".into(),
            parsed
                .map(|c| snapshot(&c))
                .unwrap_or_else(|_| error.clone()),
        );
        record.insert("validate".into(), validate_v8_config(&raw).is_ok().into());
        match normalize_config_layout(&raw, true) {
            Ok((migrated, _)) => {
                record.insert(
                    "migrated_validate".into(),
                    validate_v8_config(&migrated).is_ok().into(),
                );
                let parsed = parse_config_bytes(&migrated).map(|c| snapshot(&c));
                record.insert("migrated".into(), parsed.unwrap_or_else(|_| error.clone()));
            }
            Err(_) => {
                record.insert("migrated".into(), error.clone());
            }
        }
        for (key, migrate) in [("saved", false), ("saved_migrated", true)] {
            let file = work.path().join(format!("{name}-{key}.yaml"));
            std::fs::write(&file, &raw).unwrap();
            let Ok(mut loaded) = load_config(&file) else {
                record.insert(key.into(), json!({"__error": "load"}));
                continue;
            };
            if key == "saved" {
                record.insert("loaded_file".into(), file_json(&file));
                record.insert("loaded".into(), snapshot(&loaded));
            }
            if save_config_preserve_comments(&file, &mut loaded, migrate).is_err() {
                record.insert(key.into(), json!({"__error": "save"}));
                continue;
            }
            record.insert(key.into(), file_json(&file));
            let reloaded = load_config(&file)
                .map(|c| snapshot(&c))
                .unwrap_or_else(|_| error.clone());
            record.insert(format!("{key}_reload"), reloaded);
        }
        std::fs::write(
            out.join(format!("{name}.json")),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
    }
}
