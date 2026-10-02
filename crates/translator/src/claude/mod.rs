pub mod gemini;
pub mod interactions;
pub mod openai;

use crate::registry::Registry;

pub fn register(r: &mut Registry) {
    gemini::register(r);
    interactions::register(r);
    openai::register(r);
}
