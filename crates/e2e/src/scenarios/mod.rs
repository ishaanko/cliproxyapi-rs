//! Scenario tables. Each module contributes a list of declarative scenarios.

mod access;
mod bodies;
mod chunking;
mod codex_prepare;
mod history;
mod interactions;
mod management;
mod errors;
mod matrix;
mod plugins;
mod media;
mod profiles;
mod reqlog;
mod realtime;
mod redisqueue;
mod rich;
mod routing;
mod rust_only;
mod websocket;

use crate::scenario::Scenario;

/// Every scenario; `mock_port` is needed where scenarios embed credential ids derived from
/// the mock's base URL.
pub fn all(mock_port: u16) -> Vec<Scenario> {
    let mut v = vec![];
    v.extend(matrix::scenarios());
    v.extend(errors::scenarios());
    v.extend(routing::scenarios());
    v.extend(rich::scenarios());
    v.extend(history::scenarios());
    v.extend(chunking::scenarios());
    v.extend(interactions::scenarios());
    v.extend(access::scenarios());
    v.extend(websocket::scenarios());
    v.extend(codex_prepare::scenarios());
    v.extend(reqlog::scenarios());
    v.extend(realtime::scenarios());
    v.extend(management::scenarios(mock_port));
    v.extend(plugins::scenarios(mock_port));
    v.extend(redisqueue::scenarios());
    v.extend(media::scenarios());
    v.extend(rust_only::scenarios());
    v
}
