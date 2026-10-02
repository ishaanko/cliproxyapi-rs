pub mod claude;
pub mod common;
// Mirrors Go's internal/translator/gemini/gemini package.
#[allow(clippy::module_inception)]
pub mod gemini;
pub mod interactions;
pub mod openai;

use crate::registry::Registry;

pub fn register(r: &mut Registry) {
    claude::register(r);
    gemini::register(r);
    interactions::register(r);
    openai::register(r);
}
