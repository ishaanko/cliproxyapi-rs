// Go side of the config differential test (see tests/oracle_dump.rs and compare.py).
//
// It must live inside the reference module to import internal/config. Copy it to
// <CLIProxyAPI>/cmd/zz_cfgdump/main.go and run from that checkout:
//
//	go run ./cmd/zz_cfgdump <repo>/crates/config/oracle/corpus /tmp/go-out
//	CPA_CONFIG_ORACLE_OUT=/tmp/rust-out cargo test -p cpa-config --test oracle_dump -- --ignored
//	python3 crates/config/oracle/compare.py /tmp/go-out /tmp/rust-out
//
// Accepted differences (all in the Rust implementation's favour or representational):
//   - block_scalar: Go's NormalizeCommentIndentation drops "# ..." lines inside block scalars on
//     save (silent data loss); the Rust writer keeps them.
//   - the "json" record: Go marshals nil slices/maps as null, Rust writes [] / {}.
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"gopkg.in/yaml.v3"
)

func generic(cfg *config.Config) any {
	data, err := yaml.Marshal(cfg)
	if err != nil {
		return map[string]any{"__error": err.Error()}
	}
	var out any
	if err := yaml.Unmarshal(data, &out); err != nil {
		return map[string]any{"__error": err.Error()}
	}
	return out
}

func fileGeneric(path string) any {
	data, err := os.ReadFile(path)
	if err != nil {
		return map[string]any{"__error": true}
	}
	var out any
	if err := yaml.Unmarshal(data, &out); err != nil {
		return map[string]any{"__error": true}
	}
	if out == nil {
		return map[string]any{}
	}
	return out
}

func main() {
	in, outDir := os.Args[1], os.Args[2]
	_ = os.MkdirAll(outDir, 0o755)
	tmp, _ := os.MkdirTemp("", "cfgsave")
	files, _ := filepath.Glob(filepath.Join(in, "*.yaml"))
	for _, f := range files {
		raw, _ := os.ReadFile(f)
		name := strings.TrimSuffix(filepath.Base(f), ".yaml")
		res := map[string]any{}
		cfg, err := config.ParseConfigBytes(raw)
		if err != nil {
			res["parse"] = map[string]any{"__error": true}
		} else {
			res["parse"] = generic(cfg)
			if raw, errJSON := json.Marshal(cfg); errJSON == nil {
				var asJSON any
				_ = json.Unmarshal(raw, &asJSON)
				res["json"] = asJSON
			}
		}
		res["validate"] = config.ValidateV8Config(raw) == nil
		migrated, _, errM := config.NormalizeConfigLayout(raw, true)
		if errM != nil {
			res["migrated"] = map[string]any{"__error": true}
		} else {
			res["migrated_validate"] = config.ValidateV8Config(migrated) == nil
			cfg2, err2 := config.ParseConfigBytes(migrated)
			if err2 != nil {
				res["migrated"] = map[string]any{"__error": true}
			} else {
				res["migrated"] = generic(cfg2)
			}
		}
		// Load from disk (side effects), then save without and with v8 migration.
		for _, mode := range []struct {
			key     string
			migrate bool
		}{{"saved", false}, {"saved_migrated", true}} {
			path := filepath.Join(tmp, name+"-"+mode.key+".yaml")
			_ = os.WriteFile(path, raw, 0o600)
			loaded, errLoad := config.LoadConfig(path)
			if errLoad != nil {
				res[mode.key] = map[string]any{"__error": "load"}
				continue
			}
			if mode.key == "saved" {
				res["loaded_file"] = fileGeneric(path)
				res["loaded"] = generic(loaded)
			}
			if errSave := config.SaveConfigPreserveComments(path, loaded, mode.migrate); errSave != nil {
				res[mode.key] = map[string]any{"__error": "save"}
				continue
			}
			res[mode.key] = fileGeneric(path)
			res[mode.key+"_reload"] = func() any {
				cfg3, err3 := config.LoadConfig(path)
				if err3 != nil {
					return map[string]any{"__error": true}
				}
				return generic(cfg3)
			}()
		}
		data, _ := json.Marshal(res)
		_ = os.WriteFile(filepath.Join(outDir, name+".json"), data, 0o644)
	}
	fmt.Println("done", len(files))
}
