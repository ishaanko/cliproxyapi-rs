reqwest 0.12.28 from crates.io, unchanged except:

- `ClientBuilder::http1_max_buf_size(usize)` (async_impl/client.rs): passes hyper-util's
  `http1_max_buf_size` through, capping the per-connection HTTP/1 read buffer.
- Sources of features this workspace never enables (`blocking`, `cookies`, `multipart`,
  `hickory-dns`, wasm) are removed.

Wired in through `[patch.crates-io]` in the workspace `Cargo.toml`.
