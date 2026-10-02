# Oracle tooling

Regenerates `conformance/translator_cases.jsonl.gz` from the Go reference.

1. Clone CLIProxyAPI into `tmp/CLIProxyAPI`, copy `oracle.go` to `cmd/oracle/main.go`, build `tmp/oracle`.
2. Copy the repo to `tmp/capture`, add `zzcapture.go` as `internal/zzcapture/capture.go` and
   `capinject.go` as `cmd/capinject/main.go`, then run `capture_inject.py` from `tmp/`.
3. Run the Go translator/executor tests in `tmp/capture` with `CAPTURE_FILE=tmp/capture.jsonl`.
4. Run `build_corpus.py` from `tmp/`, then gzip the output.
