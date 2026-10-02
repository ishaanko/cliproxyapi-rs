Go oracle sources for the antigravity fixtures (oracle.json.gz, replay.json.gz, misc.json.gz).

The three zz_*_test.go files are throwaway tests for package `executor` of the reference repo
(internal/runtime/executor). Copy them next to the Go sources and run:

  python3 gen_cases.py > cases.json
  ORACLE_IN=cases.json ORACLE_OUT=oracle.json go test ./internal/runtime/executor -run TestZZOracle -count=1
  REPLAY_OUT=replay.json go test ./internal/runtime/executor -run TestZZReplayDump -count=1
  MISC_OUT=misc.json   go test ./internal/runtime/executor -run TestZZMiscDump -count=1

then gzip -9 the outputs into ../ . The Rust tests (tests.rs, replay_tests.rs, misc_tests.rs)
replay the recorded Go behavior.
