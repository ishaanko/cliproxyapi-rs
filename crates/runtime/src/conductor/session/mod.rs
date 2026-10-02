//! Session identity and affinity bookkeeping (Go: sdk/cliproxy/session + auth/session_cache.go).
//!
//! - [`info`]: explicit session id extraction (headers, body, metadata) with parent/fork hints.
//! - [`identity`]: `enrich` (canonical/derived ids into metadata), derived `ctx:v1:` ids and the
//!   `(primary, fallback)` pair affinity binds on.
//! - [`cache`]: the TTL session -> credential cache.
//!
//! Not ported: the longest-common-prefix (Merkle) matcher that Go consults when a request has no
//! explicit id; requests without one use the derived/hash id instead.

pub mod cache;
pub mod identity;
pub mod info;

pub use cache::SessionCache;
pub use identity::{
    CANDIDATE_SESSION_PREFIXES, caller_scope, canonical_session_id, derive_id, enrich, explicit_session_ids,
    is_subagent_session, session_ids,
};
pub use info::{SessionInfo, bound_session_identity, extract_session_info, normalize_explicit_id};
