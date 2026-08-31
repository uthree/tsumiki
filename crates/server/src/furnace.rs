//! Furnace state: three slots (input/fuel/output) plus the cook/fuel timers
//! that make smelting take time (roadmap M6).
//!
//! A furnace lives in a position-keyed map, not on a chunk, exactly like a
//! chest -- so it keeps ticking while nobody is looking at it (design.md
//! §4's "factories run while you sleep", pre-echoed here by the very first
//! machine). It does NOT run while the server itself is stopped: progress is
//! driven by the server's fixed tick interval (`SimRes::tick_interval_secs`),
//! the same clock every other passive system in this crate uses instead of
//! wall-clock time, so there is no "how long was the server off" catch-up to
//! compute and therefore no way for a long absence to produce an absurd
//! burst of smelted items.
//!
//! Kept as a separate module from `lib.rs`/`slots.rs` for the same reason as
//! `sim.rs`: this is a self-contained state machine, unit-testable without
//! the rest of the server.

use std::collections::HashMap;

use bevy_math::IVec3;
use serde::{Deserialize, Serialize};

use tsumiki_world::Inventory;
use tsumiki_world::inventory::click_slot;
use tsumiki_world::item::{ItemId, ItemRegistry, ItemStack};
use tsumiki_world::smelting::{
    FURNACE_FUEL, FURNACE_INPUT, FURNACE_OUTPUT, FURNACE_SIZE, SmeltingRegistry, fuel_secs, is_fuel,
};

/// How often [`crate::tick_server`] sends `FurnaceProgress` to whoever has a
/// furnace open. Twice a second is smooth enough for a progress bar without
/// meaningfully adding to per-tick traffic (contrast the day/night clock's
/// 5-second `TimeUpdate`, which nobody is watching a bar move on).
pub const BROADCAST_INTERVAL_SECS: f32 = 0.5;

/// One furnace's live state: its three slots plus in-progress timers.
#[derive(Clone, Debug)]
pub struct FurnaceState {
    pub inv: Inventory,
    /// Seconds left in the currently-burning fuel unit; `0.0` means unlit.
    pub fuel_secs_left: f32,
    /// Total seconds the currently-burning fuel unit provides, so
    /// `fuel_secs_left / fuel_secs_total` is a `[0, 1]` progress fraction.
    /// `0.0` while unlit.
    pub fuel_secs_total: f32,
    /// Seconds accumulated smelting the current input item.
    pub cook_secs: f32,
    /// Which input item `cook_secs` belongs to. A mismatch against the
    /// input slot's current item means "reset progress" -- covers both an
    /// item swap mid-cook and the input running out.
    pub cooking_item: Option<ItemId>,
}

impl FurnaceState {
    pub fn new() -> Self {
        Self {
            inv: Inventory::new(FURNACE_SIZE),
            fuel_secs_left: 0.0,
            fuel_secs_total: 0.0,
            cook_secs: 0.0,
            cooking_item: None,
        }
    }

    /// Converts to the persisted shape (see [`FurnaceRecord`]).
    pub fn to_record(&self) -> FurnaceRecord {
        FurnaceRecord {
            slots: self.inv.to_vec(),
            fuel_secs_left: self.fuel_secs_left,
            fuel_secs_total: self.fuel_secs_total,
            cook_secs: self.cook_secs,
            cooking_item: self.cooking_item,
        }
    }

    /// Restores state saved by [`Self::to_record`].
    pub fn from_record(record: FurnaceRecord) -> Self {
        Self {
            inv: Inventory::from_slots(record.slots),
            fuel_secs_left: record.fuel_secs_left,
            fuel_secs_total: record.fuel_secs_total,
            cook_secs: record.cook_secs,
            cooking_item: record.cooking_item,
        }
    }

    /// `(cook, fuel)` progress fractions in `[0, 1]`, for
    /// `ServerToClient::FurnaceProgress`. `cook` is 0 when nothing is
    /// smelting; `fuel` is 0 when unlit.
    pub fn progress(&self, smelting: &SmeltingRegistry) -> (f32, f32) {
        let cook = self
            .cooking_item
            .and_then(|item| smelting.find(item))
            .map(|r| (self.cook_secs / r.secs_per_item as f32).clamp(0.0, 1.0))
            .unwrap_or(0.0);
        let fuel = if self.fuel_secs_total > 0.0 {
            (self.fuel_secs_left / self.fuel_secs_total).clamp(0.0, 1.0)
        } else {
            0.0
        };
        (cook, fuel)
    }
}

impl Default for FurnaceState {
    fn default() -> Self {
        Self::new()
    }
}

/// A furnace's on-disk shape (roadmap M6): the same three slots the live
/// state exposes, plus enough of the timer state to resume mid-smelt after a
/// restart instead of silently losing partial progress.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FurnaceRecord {
    pub slots: Vec<Option<ItemStack>>,
    pub fuel_secs_left: f32,
    pub fuel_secs_total: f32,
    pub cook_secs: f32,
    pub cooking_item: Option<ItemId>,
}

/// Live furnace state for every furnace block that has ever been opened or
/// loaded, keyed by block position -- not chunk-bound (see module docs).
#[derive(Default)]
pub struct FurnacesRes {
    pub states: HashMap<IVec3, FurnaceState>,
    /// Seconds accumulated toward the next `FurnaceProgress` broadcast.
    pub broadcast_accum: f32,
}

/// Whether `output`'s worth of smelted item would fit into the furnace's
/// output slot right now: an empty slot, or one already holding the same
/// item under its stack cap.
fn output_fits(inv: &Inventory, output: ItemStack, item_reg: &ItemRegistry) -> bool {
    let max = item_reg.max_stack(output.item).max(1);
    match inv.slot(FURNACE_OUTPUT) {
        None => output.count <= max,
        Some(existing) => existing.mergeable_with(output) && existing.count + output.count <= max,
    }
}

/// Merges `output` into the furnace's output slot. Callers must have already
/// checked [`output_fits`] -- this does not re-check capacity.
fn deposit_output(inv: &mut Inventory, output: ItemStack) {
    let merged = match inv.slot(FURNACE_OUTPUT) {
        None => output,
        Some(existing) => ItemStack {
            count: existing.count + output.count,
            ..existing
        },
    };
    inv.set_slot(FURNACE_OUTPUT, Some(merged));
}

/// Advances one furnace by `dt` seconds: burns fuel, and while lit and a
/// valid recipe has room in the output, cooks toward the next completed
/// item. Returns `true` if anything about the furnace's persisted state
/// changed (used to mark persistence dirty).
///
/// Ignition only ever starts when there is both something to smelt *and*
/// room to receive it -- ignition never happens just because the fuel slot
/// holds something (the spec rule "do not light fuel with nothing to
/// smelt"). Once lit, though, fuel burns to completion regardless of what
/// happens to the input afterward (Minecraft's own rule): pulling the input
/// mid-burn does not un-light the furnace, it just wastes the rest of that
/// fuel unit.
pub fn tick_furnace(
    state: &mut FurnaceState,
    smelting: &SmeltingRegistry,
    item_reg: &ItemRegistry,
    dt: f32,
) -> bool {
    let mut changed = false;

    let input_item = state
        .inv
        .slot(FURNACE_INPUT)
        .and_then(|s| smelting.find(s.item))
        .map(|r| r.input);
    if input_item != state.cooking_item {
        state.cooking_item = input_item;
        state.cook_secs = 0.0;
        changed = true;
    }

    let recipe = state.cooking_item.and_then(|item| smelting.find(item));
    let can_progress = recipe.is_some_and(|r| output_fits(&state.inv, r.output, item_reg));

    if state.fuel_secs_left <= 0.0
        && can_progress
        && let Some(fuel_stack) = state.inv.slot(FURNACE_FUEL)
        && let Some(secs) = fuel_secs(fuel_stack.item)
    {
        state.inv.take_from(FURNACE_FUEL, 1);
        state.fuel_secs_left = secs as f32;
        state.fuel_secs_total = secs as f32;
        changed = true;
    }

    if state.fuel_secs_left > 0.0 {
        state.fuel_secs_left = (state.fuel_secs_left - dt).max(0.0);
        changed = true;

        if can_progress && let Some(r) = recipe {
            state.cook_secs += dt;
            if state.cook_secs >= r.secs_per_item as f32 {
                state.cook_secs -= r.secs_per_item as f32;
                state.inv.take_from(FURNACE_INPUT, 1);
                deposit_output(&mut state.inv, r.output);

                let input_item = state
                    .inv
                    .slot(FURNACE_INPUT)
                    .and_then(|s| smelting.find(s.item))
                    .map(|r| r.input);
                state.cooking_item = input_item;
                if input_item.is_none() {
                    state.cook_secs = 0.0;
                }
            }
        }
    } else if state.fuel_secs_total > 0.0 {
        // Fully burned out; clear the total too so `progress`'s `fuel`
        // fraction reports 0 rather than a stale `0 / total`.
        state.fuel_secs_total = 0.0;
        changed = true;
    }

    changed
}

/// Ticks every furnace in `states` by `dt`, returning the positions of the
/// ones whose state changed (for persistence dirtying).
pub fn tick_furnaces(
    states: &mut HashMap<IVec3, FurnaceState>,
    smelting: &SmeltingRegistry,
    item_reg: &ItemRegistry,
    dt: f32,
) -> Vec<IVec3> {
    states
        .iter_mut()
        .filter_map(|(&pos, state)| tick_furnace(state, smelting, item_reg, dt).then_some(pos))
        .collect()
}

/// Whether `item` may be *deposited* into furnace slot `index`. Taking items
/// out is never restricted (see `slots::handle_drop_slot`); this only gates
/// what a click or quick-move can put in. The output slot never accepts a
/// deposit at all, so it isn't listed here -- see [`click_furnace_slot`].
fn furnace_accepts(index: usize, item: ItemId, smelting: &SmeltingRegistry) -> bool {
    match index {
        FURNACE_INPUT => smelting.find(item).is_some(),
        FURNACE_FUEL => is_fuel(item),
        _ => false,
    }
}

/// Applies one `SlotClick` to a furnace's own slots, enforcing what may go
/// where: the input only accepts smeltable items, the fuel slot only
/// accepts fuel, and the output slot can be taken from but never deposited
/// into (not even to merge the same item back in).
pub fn click_furnace_slot(
    state: &mut FurnaceState,
    index: usize,
    cursor: &mut Option<ItemStack>,
    right: bool,
    smelting: &SmeltingRegistry,
    item_reg: &ItemRegistry,
) {
    if index >= FURNACE_SIZE {
        return;
    }
    if index == FURNACE_OUTPUT {
        if cursor.is_some() {
            // Extract-only: any click while already holding something would
            // deposit (or merge) into the slot, which output never allows.
            return;
        }
        click_slot(state.inv.slots_mut(), index, cursor, right, item_reg);
        return;
    }
    if let Some(held) = *cursor {
        // A deposit happens whenever the slot is empty (place) or holds a
        // different item (a swap would put `held` there); merging onto the
        // same item already passed this gate when it was first deposited.
        let deposits_held = match state.inv.slot(index) {
            None => true,
            Some(existing) => !existing.mergeable_with(held),
        };
        if deposits_held && !furnace_accepts(index, held.item, smelting) {
            return;
        }
    }
    click_slot(state.inv.slots_mut(), index, cursor, right, item_reg);
}

/// Shift-clicking a main-inventory slot while a furnace is open: routes the
/// item to whichever of input/fuel accepts it (mirroring
/// [`furnace_accepts`]), merging into an existing matching stack or filling
/// the slot if empty. Returns `false` (and moves nothing) for an item that
/// fits neither, or a target slot occupied by something incompatible or
/// full.
pub fn quick_move_into_furnace(
    main: &mut Inventory,
    main_index: usize,
    state: &mut FurnaceState,
    smelting: &SmeltingRegistry,
    item_reg: &ItemRegistry,
) -> bool {
    let Some(stack) = main.slot(main_index) else {
        return false;
    };
    let target = if smelting.find(stack.item).is_some() {
        FURNACE_INPUT
    } else if is_fuel(stack.item) {
        FURNACE_FUEL
    } else {
        return false;
    };

    let max = item_reg.max_stack(stack.item).max(1);
    let (new_count, moved) = match state.inv.slot(target) {
        Some(existing) if existing.mergeable_with(stack) && existing.count < max => {
            let moved = (max - existing.count).min(stack.count);
            (existing.count + moved, moved)
        }
        None => {
            let moved = stack.count.min(max);
            (moved, moved)
        }
        _ => return false,
    };
    if moved == 0 {
        return false;
    }
    state.inv.set_slot(
        target,
        Some(ItemStack {
            count: new_count,
            ..stack
        }),
    );
    let left = stack.count - moved;
    main.set_slot(
        main_index,
        (left > 0).then_some(ItemStack {
            count: left,
            ..stack
        }),
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsumiki_world::item::items;

    fn regs() -> (SmeltingRegistry, ItemRegistry) {
        (SmeltingRegistry::prototype(), ItemRegistry::prototype())
    }

    #[test]
    fn lighting_requires_something_to_smelt() {
        let (smelting, item_reg) = regs();
        let mut state = FurnaceState::new();
        state
            .inv
            .set_slot(FURNACE_FUEL, Some(ItemStack::new(items::COAL, 1)));

        tick_furnace(&mut state, &smelting, &item_reg, 1.0);

        assert_eq!(
            state.fuel_secs_left, 0.0,
            "fuel must not ignite with no input"
        );
        assert_eq!(
            state.inv.slot(FURNACE_FUEL),
            Some(ItemStack::new(items::COAL, 1)),
            "unlit fuel must not be consumed"
        );
    }

    #[test]
    fn smelting_consumes_fuel_and_input_and_produces_output() {
        let (smelting, item_reg) = regs();
        let mut state = FurnaceState::new();
        state
            .inv
            .set_slot(FURNACE_INPUT, Some(ItemStack::one(items::IRON_ORE)));
        state
            .inv
            .set_slot(FURNACE_FUEL, Some(ItemStack::new(items::COAL, 1)));

        // Coal burns for 80s, the recipe needs 10s: one tick past that
        // completes the smelt with plenty of fuel left over.
        tick_furnace(&mut state, &smelting, &item_reg, 10.0);

        assert_eq!(state.inv.slot(FURNACE_INPUT), None);
        assert_eq!(
            state.inv.slot(FURNACE_OUTPUT),
            Some(ItemStack::one(items::IRON_INGOT))
        );
        assert_eq!(
            state.inv.slot(FURNACE_FUEL),
            None,
            "the fuel item should be consumed on ignition"
        );
        assert!(state.fuel_secs_left > 0.0, "coal should still be burning");
    }

    #[test]
    fn a_full_output_stalls_instead_of_voiding() {
        let (smelting, item_reg) = regs();
        let mut state = FurnaceState::new();
        state
            .inv
            .set_slot(FURNACE_INPUT, Some(ItemStack::new(items::IRON_ORE, 2)));
        state
            .inv
            .set_slot(FURNACE_FUEL, Some(ItemStack::new(items::COAL, 1)));
        state.inv.set_slot(
            FURNACE_OUTPUT,
            Some(ItemStack::new(
                items::IRON_INGOT,
                item_reg.max_stack(items::IRON_INGOT),
            )),
        );

        tick_furnace(&mut state, &smelting, &item_reg, 10.0);

        assert_eq!(
            state.inv.slot(FURNACE_INPUT),
            Some(ItemStack::new(items::IRON_ORE, 2)),
            "input must not be consumed while the output has no room"
        );
        assert_eq!(
            state.fuel_secs_left, 0.0,
            "fuel should never have been lit for a smelt that can't complete"
        );
    }

    #[test]
    fn fuel_keeps_burning_once_lit_even_after_input_runs_out() {
        let (smelting, item_reg) = regs();
        let mut state = FurnaceState::new();
        state
            .inv
            .set_slot(FURNACE_INPUT, Some(ItemStack::one(items::IRON_ORE)));
        state
            .inv
            .set_slot(FURNACE_FUEL, Some(ItemStack::new(items::COAL, 1)));

        // Smelts the one ore and ignites a fresh 80s of coal.
        tick_furnace(&mut state, &smelting, &item_reg, 10.0);
        assert_eq!(state.inv.slot(FURNACE_INPUT), None);
        let fuel_after_smelt = state.fuel_secs_left;
        assert!(fuel_after_smelt > 0.0);

        // No input left, but the already-lit fuel keeps ticking down.
        tick_furnace(&mut state, &smelting, &item_reg, 5.0);
        assert!(
            state.fuel_secs_left < fuel_after_smelt,
            "fuel should keep burning down with no input left"
        );
    }

    #[test]
    fn furnace_slots_reject_the_wrong_kind_of_item() {
        let (smelting, item_reg) = regs();
        let mut state = FurnaceState::new();

        let mut cursor = Some(ItemStack::one(items::DIRT));
        click_furnace_slot(
            &mut state,
            FURNACE_INPUT,
            &mut cursor,
            false,
            &smelting,
            &item_reg,
        );
        assert_eq!(state.inv.slot(FURNACE_INPUT), None, "dirt does not smelt");
        assert_eq!(cursor, Some(ItemStack::one(items::DIRT)));

        let mut cursor = Some(ItemStack::one(items::IRON_ORE));
        click_furnace_slot(
            &mut state,
            FURNACE_FUEL,
            &mut cursor,
            false,
            &smelting,
            &item_reg,
        );
        assert_eq!(state.inv.slot(FURNACE_FUEL), None, "iron ore is not fuel");
        assert_eq!(cursor, Some(ItemStack::one(items::IRON_ORE)));
    }

    #[test]
    fn output_can_be_taken_but_never_deposited_into() {
        let (smelting, item_reg) = regs();
        let mut state = FurnaceState::new();
        state
            .inv
            .set_slot(FURNACE_OUTPUT, Some(ItemStack::one(items::IRON_INGOT)));

        let mut cursor = Some(ItemStack::one(items::IRON_INGOT));
        click_furnace_slot(
            &mut state,
            FURNACE_OUTPUT,
            &mut cursor,
            false,
            &smelting,
            &item_reg,
        );
        assert_eq!(
            state.inv.slot(FURNACE_OUTPUT),
            Some(ItemStack::one(items::IRON_INGOT)),
            "the output slot must not accept a deposit, even of the same item"
        );
        assert_eq!(cursor, Some(ItemStack::one(items::IRON_INGOT)));

        let mut cursor = None;
        click_furnace_slot(
            &mut state,
            FURNACE_OUTPUT,
            &mut cursor,
            false,
            &smelting,
            &item_reg,
        );
        assert_eq!(cursor, Some(ItemStack::one(items::IRON_INGOT)));
        assert_eq!(state.inv.slot(FURNACE_OUTPUT), None);
    }
}
