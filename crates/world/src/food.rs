//! Food values shared by the authoritative hunger simulation and item UI.

use crate::{ItemId, items};

pub const MAX_HUNGER: u16 = 20;

/// Hunger restored by eating one item. Cooking improves the same food chain.
pub fn nutrition(item: ItemId) -> Option<u16> {
    match item {
        items::BREAD => Some(5),
        items::TOAST => Some(8),
        _ => None,
    }
}
