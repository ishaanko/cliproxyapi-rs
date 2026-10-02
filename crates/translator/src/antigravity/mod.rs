mod b64;
mod function_names;
mod function_response;
pub mod claude;
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
