# Base-layer differential oracle

Re-runnable evidence that `cpa-core`'s `util`, `registry`, `misc` and `applypatch` match the Go
reference (`internal/util`, `internal/registry`, `internal/misc`, `internal/client/codex/apply-patch`).
Nothing here is built by the main workspace.

```sh
crates/core/oracle/run.sh          # needs python3, the Go toolchain and tmp/CLIProxyAPI
```

Environment overrides: `GO_SRC` (reference checkout), `GO_TOOLCHAIN_BIN`, `SCRATCH` (default
`<repo>/tmp/scratch/base`), `FUZZ_SEEDS`, `FUZZ_COUNT`.

## Pieces

- `go/base_oracle.go.txt`: Go program (a `.txt` so Go tooling ignores it) copied to
  `cmd/zz_base_oracle/main.go` inside a scratch copy of the reference repo. JSONL ops in, one JSON
  result per line out.
- `rust/`: standalone cargo package with the same protocol over `cpa-core`.
- `python/`: corpus builders and comparators.
  - `schemas.py`: ~900 tool schemas from `conformance/translator_cases.jsonl.gz` and Go test
    strings, plus `mk_extra.py` edge schemas, through all schema cleaners; ~4,800 request bodies
    through the tool name maps, Responses tool builder, attribution stripping and dedupe.
    Expected: 8 mismatches, all whitespace inside `key: <raw json>` description hints when the
    quoted node was itself edited by an earlier phase (Go keeps the input's whitespace).
  - `scalars.py`: sanitizers, FixJSON, Gemini tool-use ids, masks, tool choice/result,
    apply-patch, OAuth callback, user agents, mime table, static catalogs and lookups. Expected:
    2 mismatches (OAuth error message text for unparsable inputs).
  - `registry_fuzz.py SEED COUNT`: random register/unregister/suspend/quota/projection/capability
    scenarios against the global registry in both implementations, comparing a full snapshot after
    each query step. Expected: 0 mismatches. `by_provider` is compared by model ids only since Go
    picks an arbitrary client's info there (map order).

Outputs are compared structurally. String results holding JSON keep key order; Go map results
ignore it; nil and empty collections are treated alike where Go cannot distinguish them.
