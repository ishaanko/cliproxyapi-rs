# Vendored reqwest 0.12.28

Pristine crates.io `reqwest 0.12.28` (`Cargo.toml`, licenses, README, `src/`; no tests or CI files)
plus one patch, `../reqwest.patch` (also kept in this repo as the source of truth):

- `ClientBuilder::http1_max_buf_size(usize)` in `src/async_impl/client.rs`: a config field, its
  default, a builder method and one `builder.http1_max_buf_size(max)` call that passes hyper-util's
  option through. It caps the per-connection HTTP/1 read buffer (hyper's default grows to ~400 KB
  on a stream that keeps filling reads) and also bounds the size of a response head. HTTP/2
  connections, negotiated by ALPN, are not affected.

The tree is wired in with `[patch.crates-io]` in the workspace `Cargo.toml`, so every workspace
crate uses it; all upstream feature sources are kept so enabling any reqwest feature still builds.

## Maintaining

    tools/revendor-reqwest.sh --check   # tree == upstream + patch
    tools/revendor-reqwest.sh           # rebuild the tree from upstream + patch
    tools/revendor-reqwest.sh --diff    # regenerate ../reqwest.patch after editing the tree

To upgrade, bump `VERSION` in the script, run it, resolve any patch rejects by hand, run `--diff`
and refresh this file.
