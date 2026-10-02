pub mod chat_completions;
pub mod responses;

use crate::registry::Registry;

pub fn register(r: &mut Registry) {
    chat_completions::register(r);
    responses::register(r);
}
