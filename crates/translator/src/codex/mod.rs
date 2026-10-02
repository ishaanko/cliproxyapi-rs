//! Port of internal/translator/codex (client dialects -> Codex Responses upstream).

pub mod claude;
pub mod gemini;
pub mod interactions;
pub mod openai;
mod raw;
mod util;

use crate::registry::Registry;

pub fn register(r: &mut Registry) {
    claude::register(r);
    gemini::register(r);
    interactions::register(r);
    openai::register(r);
}
