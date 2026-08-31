//! Inventories and slot manipulation (design.md §7, roadmap M5).
//!
//! Everything here is pure data + pure functions: no Bevy, no networking, no
//! I/O. The server owns every inventory and is the only thing allowed to
//! mutate one; the client renders snapshots. Keeping the rules here means
//! they are unit-testable without a running game.
//!
//! The slot-click model is Minecraft's, because it is the one players
//! already know and it expresses every drag-and-drop gesture with a single
//! message: a *cursor* holds at most one stack, a left click takes or
//! deposits the whole stack, a right click takes half or deposits one.

use serde::{Deserialize, Serialize};

use crate::item::{ItemId, ItemRegistry, ItemStack};

/// Hotbar slots, which are also the first slots of the main inventory: a
/// player's slot 0..9 is their hotbar, and slot 9..36 is the backpack. This
/// aliasing is deliberate -- it is why picking something up can make it
/// immediately usable.
pub const HOTBAR_SIZE: usize = 9;

/// Total player inventory slots (hotbar included).
pub const MAIN_INVENTORY_SIZE: usize = 36;

/// Slots in a chest.
pub const CHEST_SIZE: usize = 27;

/// A fixed-size array of slots, each either empty or holding one stack.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inventory {
    slots: Vec<Option<ItemStack>>,
}

impl Inventory {
    /// An inventory of `size` empty slots.
    pub fn new(size: usize) -> Self {
        Self {
            slots: vec![None; size],
        }
    }

    /// Wraps an existing slot array (loading a save, or a network snapshot).
    pub fn from_slots(slots: Vec<Option<ItemStack>>) -> Self {
        Self { slots }
    }

    pub fn slots(&self) -> &[Option<ItemStack>] {
        &self.slots
    }

    pub fn slots_mut(&mut self) -> &mut [Option<ItemStack>] {
        &mut self.slots
    }

    pub fn to_vec(&self) -> Vec<Option<ItemStack>> {
        self.slots.clone()
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// `None` for an out-of-range index as well as an empty slot; callers
    /// are handling untrusted indices from the network.
    pub fn slot(&self, index: usize) -> Option<ItemStack> {
        self.slots.get(index).copied().flatten()
    }

    /// Out-of-range writes are ignored (see [`Self::slot`]).
    pub fn set_slot(&mut self, index: usize, stack: Option<ItemStack>) {
        if let Some(slot) = self.slots.get_mut(index) {
            *slot = stack;
        }
    }

    /// Total count of `item` across every slot.
    pub fn count_of(&self, item: ItemId) -> u32 {
        self.slots
            .iter()
            .flatten()
            .filter(|stack| stack.item == item)
            .map(|stack| stack.count)
            .sum()
    }

    /// Inserts `stack`, merging into existing partial stacks of the same item
    /// first (in slot order, respecting `max_stack`), then filling empty
    /// slots. Returns whatever did not fit, or `None` if everything did.
    ///
    /// Merging before filling matters: picking up one dirt when you already
    /// hold 3 must grow that stack rather than eat a fresh slot.
    pub fn insert(&mut self, stack: ItemStack, reg: &ItemRegistry) -> Option<ItemStack> {
        let mut left = stack;
        if left.count == 0 {
            return None;
        }
        let max = reg.max_stack(left.item).max(1);

        for slot in self.slots.iter_mut() {
            if left.count == 0 {
                break;
            }
            if let Some(existing) = slot
                && existing.mergeable_with(left)
                && existing.count < max
            {
                let moved = (max - existing.count).min(left.count);
                existing.count += moved;
                left.count -= moved;
            }
        }

        for slot in self.slots.iter_mut() {
            if left.count == 0 {
                break;
            }
            if slot.is_none() {
                let moved = max.min(left.count);
                *slot = Some(ItemStack {
                    count: moved,
                    ..left
                });
                left.count -= moved;
            }
        }

        (left.count > 0).then_some(left)
    }

    /// Removes up to `count` of `item` from anywhere in the inventory.
    /// All-or-nothing: if fewer than `count` are present, nothing is removed
    /// and this returns `false`.
    pub fn remove(&mut self, item: ItemId, count: u32) -> bool {
        if count == 0 {
            return true;
        }
        if self.count_of(item) < count {
            return false;
        }
        let mut left = count;
        for slot in self.slots.iter_mut() {
            if left == 0 {
                break;
            }
            let Some(existing) = slot else { continue };
            if existing.item != item {
                continue;
            }
            let taken = existing.count.min(left);
            existing.count -= taken;
            left -= taken;
            if existing.count == 0 {
                *slot = None;
            }
        }
        true
    }

    /// Takes up to `count` from one slot, leaving the remainder. `None` if
    /// the slot is empty or out of range.
    pub fn take_from(&mut self, index: usize, count: u32) -> Option<ItemStack> {
        if count == 0 {
            return None;
        }
        let slot = self.slots.get_mut(index)?;
        let existing = slot.as_mut()?;
        let taken_from = *existing;
        let taken = existing.count.min(count);
        existing.count -= taken;
        if existing.count == 0 {
            *slot = None;
        }
        Some(ItemStack {
            count: taken,
            ..taken_from
        })
    }

    /// Every non-empty slot, emptied out. Used for death drops.
    pub fn drain(&mut self) -> Vec<ItemStack> {
        self.slots
            .iter_mut()
            .filter_map(|slot| slot.take())
            .collect()
    }
}

/// Applies one slot click to `slots[index]` against `cursor`, Minecraft
/// style. Both are mutated in place.
///
/// - Left click (`right == false`):
///   - cursor empty: pick up the whole slot.
///   - cursor holds the same item: deposit as much as `max_stack` allows,
///     keeping the remainder on the cursor.
///   - cursor holds a different item: swap slot and cursor.
/// - Right click (`right == true`):
///   - cursor empty: pick up half the slot, rounded up (so a single item is
///     still taken).
///   - cursor holds the same item, or the slot is empty: deposit one.
///   - cursor holds a different item: swap (same as left click).
///
/// An out-of-range `index` is a no-op: indices arrive from the network.
pub fn click_slot(
    slots: &mut [Option<ItemStack>],
    index: usize,
    cursor: &mut Option<ItemStack>,
    right: bool,
    reg: &ItemRegistry,
) {
    let Some(slot) = slots.get_mut(index) else {
        return;
    };
    match (*slot, *cursor) {
        (None, None) => {}
        // Pick up: the whole stack, or half (rounded up, so a lone item is
        // still picked up rather than doing nothing).
        (Some(in_slot), None) => {
            if right {
                let taken = in_slot.count.div_ceil(2);
                let rest = in_slot.count - taken;
                *cursor = Some(ItemStack {
                    count: taken,
                    ..in_slot
                });
                *slot = (rest > 0).then_some(ItemStack {
                    count: rest,
                    ..in_slot
                });
            } else {
                *cursor = Some(in_slot);
                *slot = None;
            }
        }
        // Deposit into an empty slot.
        (None, Some(held)) => {
            let max = reg.max_stack(held.item).max(1);
            let moved = if right { 1 } else { held.count.min(max) };
            let rest = held.count - moved;
            *slot = Some(ItemStack {
                count: moved,
                ..held
            });
            *cursor = (rest > 0).then_some(ItemStack {
                count: rest,
                ..held
            });
        }
        // Same item: merge as far as the stack limit allows. A full slot
        // leaves everything untouched (rather than swapping, which would
        // surprise the player mid-drag).
        (Some(in_slot), Some(held)) if in_slot.mergeable_with(held) => {
            let max = reg.max_stack(held.item).max(1);
            let space = max.saturating_sub(in_slot.count);
            let moved = if right {
                space.min(1)
            } else {
                space.min(held.count)
            };
            if moved > 0 {
                *slot = Some(ItemStack {
                    count: in_slot.count + moved,
                    ..in_slot
                });
                let rest = held.count - moved;
                *cursor = (rest > 0).then_some(ItemStack {
                    count: rest,
                    ..held
                });
            }
        }
        // Different items: swap.
        (Some(_), Some(_)) => std::mem::swap(slot, cursor),
    }
}

/// Shift-click: moves the whole stack at `from_index` of `from` into `to`,
/// merging as [`Inventory::insert`] does. Anything that does not fit stays
/// put. Returns `true` if at least one item moved.
pub fn quick_move(
    from: &mut Inventory,
    from_index: usize,
    to: &mut Inventory,
    reg: &ItemRegistry,
) -> bool {
    let Some(stack) = from.slot(from_index) else {
        return false;
    };
    let leftover = to.insert(stack, reg);
    let moved = stack.count - leftover.map_or(0, |left| left.count);
    if moved == 0 {
        return false;
    }
    from.set_slot(from_index, leftover);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{DEFAULT_MAX_STACK, items};

    fn reg() -> ItemRegistry {
        ItemRegistry::prototype()
    }

    #[test]
    fn insert_merges_before_filling_empty_slots() {
        let reg = reg();
        let mut inv = Inventory::new(4);
        inv.set_slot(2, Some(ItemStack::new(items::DIRT, 3)));

        assert!(inv.insert(ItemStack::new(items::DIRT, 5), &reg).is_none());

        assert_eq!(inv.slot(2), Some(ItemStack::new(items::DIRT, 8)));
        assert_eq!(inv.slot(0), None, "an empty slot was used before merging");
    }

    #[test]
    fn insert_spills_across_slots_and_reports_leftovers() {
        let reg = reg();
        let mut inv = Inventory::new(2);

        let left = inv.insert(ItemStack::new(items::STONE, DEFAULT_MAX_STACK * 3), &reg);

        assert_eq!(inv.slot(0), Some(ItemStack::new(items::STONE, 64)));
        assert_eq!(inv.slot(1), Some(ItemStack::new(items::STONE, 64)));
        assert_eq!(left, Some(ItemStack::new(items::STONE, 64)));
    }

    #[test]
    fn remove_is_all_or_nothing() {
        let reg = reg();
        let mut inv = Inventory::new(3);
        inv.insert(ItemStack::new(items::PLANKS, 5), &reg);

        assert!(!inv.remove(items::PLANKS, 6));
        assert_eq!(inv.count_of(items::PLANKS), 5, "a failed remove took items");

        assert!(inv.remove(items::PLANKS, 5));
        assert_eq!(inv.count_of(items::PLANKS), 0);
        assert_eq!(inv.slot(0), None, "an emptied slot kept a zero-count stack");
    }

    #[test]
    fn left_click_picks_up_then_deposits() {
        let reg = reg();
        let mut slots = vec![Some(ItemStack::new(items::LOG, 7)), None];
        let mut cursor = None;

        click_slot(&mut slots, 0, &mut cursor, false, &reg);
        assert_eq!(cursor, Some(ItemStack::new(items::LOG, 7)));
        assert_eq!(slots[0], None);

        click_slot(&mut slots, 1, &mut cursor, false, &reg);
        assert_eq!(slots[1], Some(ItemStack::new(items::LOG, 7)));
        assert_eq!(cursor, None);
    }

    #[test]
    fn right_click_takes_half_rounded_up() {
        let reg = reg();
        let mut slots = vec![Some(ItemStack::new(items::SAND, 5))];
        let mut cursor = None;

        click_slot(&mut slots, 0, &mut cursor, true, &reg);

        assert_eq!(cursor, Some(ItemStack::new(items::SAND, 3)));
        assert_eq!(slots[0], Some(ItemStack::new(items::SAND, 2)));
    }

    #[test]
    fn right_click_on_lone_item_still_takes_it() {
        let reg = reg();
        let mut slots = vec![Some(ItemStack::one(items::STICK))];
        let mut cursor = None;

        click_slot(&mut slots, 0, &mut cursor, true, &reg);

        assert_eq!(cursor, Some(ItemStack::one(items::STICK)));
        assert_eq!(slots[0], None);
    }

    #[test]
    fn right_click_deposits_one_at_a_time() {
        let reg = reg();
        let mut slots = vec![None];
        let mut cursor = Some(ItemStack::new(items::CHEST, 3));

        click_slot(&mut slots, 0, &mut cursor, true, &reg);
        assert_eq!(slots[0], Some(ItemStack::one(items::CHEST)));
        assert_eq!(cursor, Some(ItemStack::new(items::CHEST, 2)));

        click_slot(&mut slots, 0, &mut cursor, true, &reg);
        assert_eq!(slots[0], Some(ItemStack::new(items::CHEST, 2)));
        assert_eq!(cursor, Some(ItemStack::one(items::CHEST)));
    }

    #[test]
    fn clicking_a_different_item_swaps() {
        let reg = reg();
        let mut slots = vec![Some(ItemStack::new(items::DIRT, 2))];
        let mut cursor = Some(ItemStack::new(items::STONE, 9));

        click_slot(&mut slots, 0, &mut cursor, false, &reg);

        assert_eq!(slots[0], Some(ItemStack::new(items::STONE, 9)));
        assert_eq!(cursor, Some(ItemStack::new(items::DIRT, 2)));
    }

    #[test]
    fn depositing_onto_a_full_stack_keeps_the_cursor() {
        let reg = reg();
        let mut slots = vec![Some(ItemStack::new(items::DIRT, DEFAULT_MAX_STACK))];
        let mut cursor = Some(ItemStack::new(items::DIRT, 5));

        click_slot(&mut slots, 0, &mut cursor, false, &reg);

        assert_eq!(
            slots[0],
            Some(ItemStack::new(items::DIRT, DEFAULT_MAX_STACK))
        );
        assert_eq!(
            cursor,
            Some(ItemStack::new(items::DIRT, 5)),
            "items vanished into a full stack"
        );
    }

    #[test]
    fn deposit_stops_at_the_stack_limit() {
        let reg = reg();
        let mut slots = vec![Some(ItemStack::new(items::DIRT, 60))];
        let mut cursor = Some(ItemStack::new(items::DIRT, 10));

        click_slot(&mut slots, 0, &mut cursor, false, &reg);

        assert_eq!(slots[0], Some(ItemStack::new(items::DIRT, 64)));
        assert_eq!(cursor, Some(ItemStack::new(items::DIRT, 6)));
    }

    #[test]
    fn out_of_range_clicks_are_ignored() {
        let reg = reg();
        let mut slots = vec![None];
        let mut cursor = Some(ItemStack::one(items::STONE));

        click_slot(&mut slots, 99, &mut cursor, false, &reg);

        assert_eq!(cursor, Some(ItemStack::one(items::STONE)));
    }

    #[test]
    fn quick_move_transfers_what_fits_and_leaves_the_rest() {
        let reg = reg();
        let mut from = Inventory::new(1);
        from.set_slot(0, Some(ItemStack::new(items::STONE, 40)));
        let mut to = Inventory::new(1);
        to.set_slot(0, Some(ItemStack::new(items::STONE, 40)));

        assert!(quick_move(&mut from, 0, &mut to, &reg));

        assert_eq!(to.slot(0), Some(ItemStack::new(items::STONE, 64)));
        assert_eq!(from.slot(0), Some(ItemStack::new(items::STONE, 16)));
    }

    #[test]
    fn quick_move_into_a_full_inventory_does_nothing() {
        let reg = reg();
        let mut from = Inventory::new(1);
        from.set_slot(0, Some(ItemStack::new(items::STONE, 8)));
        let mut to = Inventory::new(1);
        to.set_slot(0, Some(ItemStack::new(items::DIRT, DEFAULT_MAX_STACK)));

        assert!(!quick_move(&mut from, 0, &mut to, &reg));

        assert_eq!(from.slot(0), Some(ItemStack::new(items::STONE, 8)));
    }

    #[test]
    fn drain_empties_the_inventory() {
        let reg = reg();
        let mut inv = Inventory::new(3);
        inv.insert(ItemStack::new(items::LOG, 2), &reg);
        inv.insert(ItemStack::new(items::SAND, 1), &reg);

        let dropped = inv.drain();

        assert_eq!(dropped.len(), 2);
        assert!(inv.slots().iter().all(Option::is_none));
    }
}
