pub mod claude;

use crate::registry::Registry;

pub fn register(r: &mut Registry) {
    claude::register(r);
}
