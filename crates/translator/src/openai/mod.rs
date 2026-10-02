pub mod claude;
pub mod gemini;
pub mod interactions;
// Mirrors Go's internal/translator/openai/openai package.
#[allow(clippy::module_inception)]
pub mod openai;

use crate::registry::Registry;

pub fn register(r: &mut Registry) {
    claude::register(r);
    gemini::register(r);
    interactions::register(r);
    openai::register(r);
}
