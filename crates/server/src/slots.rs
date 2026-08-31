//! Inventory slot operations, crafting, and container plumbing (design.md
//! §7, roadmap M5): `SlotClick`, `DropSlot`, and the fresh `InventoryUpdate`
//! snapshot the server answers every slot-affecting message with.
//!
//! Kept as a separate module from `lib.rs` for the same reason as `sim.rs`
//! -- this module reaches into `lib.rs`'s private `ClientState` directly.
//! `OpenContainer`/`CloseContainer` stay in `lib.rs` since they need the
//! chunk cache (to look up the block being interacted with), which this
//! module has no other reason to depend on.

use std::collections::HashMap;

use bevy::prelude::Resource;
use bevy_math::IVec3;

use tsumiki_protocol::{ContainerKind, ServerToClient, SlotArea, SlotRef};
use tsumiki_world::inventory::{
    CHEST_SIZE, click_slot, consume_craft, craft_index_usable, craft_view, quick_move,
};
use tsumiki_world::{
    HOTBAR_SIZE, Inventory, ItemRegistry, ItemStack, MAIN_INVENTORY_SIZE, RecipeRegistry,
};

use crate::ClientState;

/// Bundles the item catalog, recipe table, and live chest contents so
/// `tick_server` spends only one parameter on all of M5's crafting state
/// (the same reasoning as `SimRes`, doc/roadmap.md M4).
#[derive(Resource)]
pub struct CraftingRes {
    pub items: ItemRegistry,
    pub recipes: RecipeRegistry,
    /// Chest contents, keyed by block position. Created lazily the first
    /// time a chest is opened; crafting tables hold no items and never get
    /// an entry here.
    pub containers: HashMap<IVec3, Inventory>,
}

impl Default for CraftingRes {
    fn default() -> Self {
        Self {
            items: ItemRegistry::prototype(),
            recipes: RecipeRegistry::prototype(),
            containers: HashMap::new(),
        }
    }
}

/// `true` if `client` currently has a crafting table open, which widens the
/// crafting grid from 2x2 to 3x3.
fn crafting_table_open(client: &ClientState) -> bool {
    matches!(
        client.open_container,
        Some((_, ContainerKind::CraftingTable))
    )
}

/// The side of the currently-usable crafting grid: 2 in the bare inventory
/// screen, 3 at an open crafting table.
fn craft_grid_size(client: &ClientState) -> usize {
    if crafting_table_open(client) { 3 } else { 2 }
}

/// `true` if `index` (into the relevant backing slot array) is addressable
/// for `area` in `client`'s current state. The crafting grid is always the
/// full 9-slot array (never resized), so its check goes through
/// [`craft_index_usable`]'s mask rather than a plain length -- everything
/// else here is a contiguous range. `CraftOutput` has no backing slot array
/// at all and is handled by its own logic in [`handle_slot_click`].
fn slot_usable(
    area: SlotArea,
    index: usize,
    client: &ClientState,
    containers: &HashMap<IVec3, Inventory>,
) -> bool {
    match area {
        SlotArea::Main => index < MAIN_INVENTORY_SIZE,
        SlotArea::Crafting => craft_index_usable(craft_grid_size(client), index),
        SlotArea::Container => match client.open_container {
            Some((pos, ContainerKind::Chest)) => {
                containers.contains_key(&pos) && index < CHEST_SIZE
            }
            _ => false,
        },
        SlotArea::CraftOutput => false,
    }
}

/// Computes the full `InventoryUpdate` snapshot for `client`, including a
/// freshly-computed `craft_output` for whatever the crafting grid currently
/// matches.
pub fn inventory_snapshot(client: &ClientState, crafting: &CraftingRes) -> ServerToClient {
    let size = craft_grid_size(client);
    let view = craft_view(client.crafting.slots(), size);
    let craft_output = crafting.recipes.find(&view, size).map(|r| r.output);
    ServerToClient::InventoryUpdate {
        main: client.main.to_vec(),
        crafting: client.crafting.to_vec(),
        craft_output,
        cursor: client.cursor,
    }
}

/// Shift-clicking a main-inventory slot with no container open moves the
/// stack between the hotbar and the backpack -- Minecraft's own inventory
/// screen behavior. Unlike `tsumiki_world::inventory::quick_move` (which
/// moves between two distinct `Inventory`s), both ranges live in the same
/// one here, so this mirrors its merge-then-fill policy by hand.
fn quick_move_within_main(main: &mut Inventory, index: usize, reg: &ItemRegistry) {
    let Some(stack) = main.slot(index) else {
        return;
    };
    let (start, end) = if index < HOTBAR_SIZE {
        (HOTBAR_SIZE, MAIN_INVENTORY_SIZE)
    } else {
        (0, HOTBAR_SIZE)
    };
    let max = reg.max_stack(stack.item).max(1);
    let mut left = stack.count;

    for slot in &mut main.slots_mut()[start..end] {
        if left == 0 {
            break;
        }
        if let Some(existing) = slot
            && existing.item == stack.item
            && existing.count < max
        {
            let moved = (max - existing.count).min(left);
            existing.count += moved;
            left -= moved;
        }
    }
    for slot in &mut main.slots_mut()[start..end] {
        if left == 0 {
            break;
        }
        if slot.is_none() {
            let moved = max.min(left);
            *slot = Some(ItemStack::new(stack.item, moved));
            left -= moved;
        }
    }

    if left == stack.count {
        // Nothing moved at all.
        return;
    }
    main.set_slot(index, (left > 0).then(|| ItemStack::new(stack.item, left)));
}

/// Performs one craft, if the current grid matches a recipe. The result
/// normally goes to the cursor (merging onto a matching cursor stack, or
/// refusing if the cursor holds something else, or too much of the same
/// thing to fit the result); with `shift` set it instead tries to
/// quick-move the result straight into the main inventory. A non-matching
/// grid, or a result with nowhere to go, is a no-op -- nothing can ever be
/// put *into* the output slot (there is no slot storage for it at all).
fn handle_craft_output(client: &mut ClientState, crafting: &mut CraftingRes, shift: bool) {
    let size = craft_grid_size(client);
    let view = craft_view(client.crafting.slots(), size);
    let Some(output) = crafting.recipes.find(&view, size).map(|r| r.output) else {
        return;
    };

    if shift {
        let mut probe = client.main.clone();
        if probe.insert(output, &crafting.items).is_some() {
            return; // Doesn't fully fit: leave everything untouched.
        }
        client.main = probe;
        consume_craft(client.crafting.slots_mut(), size);
        return;
    }

    let max = crafting.items.max_stack(output.item).max(1);
    let merged = match client.cursor {
        None => Some(output),
        Some(cursor) if cursor.item == output.item && cursor.count + output.count <= max => {
            Some(ItemStack::new(cursor.item, cursor.count + output.count))
        }
        _ => None,
    };
    let Some(merged) = merged else {
        return;
    };
    client.cursor = Some(merged);
    consume_craft(client.crafting.slots_mut(), size);
}

/// Handles one `SlotClick`, mutating `client` and, if a chest is involved,
/// `crafting.containers`. Returns the chest position whose contents changed,
/// if any, so the caller can broadcast `ContainerUpdate` to every other
/// viewer (the acting client's own view comes from the `InventoryUpdate` the
/// caller always sends afterward, regardless of whether anything changed).
pub fn handle_slot_click(
    client: &mut ClientState,
    crafting: &mut CraftingRes,
    slot: SlotRef,
    right: bool,
    shift: bool,
) -> Option<IVec3> {
    if matches!(slot.area, SlotArea::CraftOutput) {
        handle_craft_output(client, crafting, shift);
        return None;
    }

    let index = slot.index as usize;
    if !slot_usable(slot.area, index, client, &crafting.containers) {
        return None;
    }

    match slot.area {
        SlotArea::Main => {
            if shift {
                match client.open_container {
                    Some((pos, ContainerKind::Chest)) => {
                        if let Some(inv) = crafting.containers.get_mut(&pos) {
                            quick_move(&mut client.main, index, inv, &crafting.items);
                            return Some(pos);
                        }
                    }
                    _ => quick_move_within_main(&mut client.main, index, &crafting.items),
                }
            } else {
                click_slot(
                    client.main.slots_mut(),
                    index,
                    &mut client.cursor,
                    right,
                    &crafting.items,
                );
            }
        }
        SlotArea::Crafting => {
            if shift {
                quick_move(
                    &mut client.crafting,
                    index,
                    &mut client.main,
                    &crafting.items,
                );
            } else {
                click_slot(
                    client.crafting.slots_mut(),
                    index,
                    &mut client.cursor,
                    right,
                    &crafting.items,
                );
            }
        }
        SlotArea::Container => {
            let Some((pos, ContainerKind::Chest)) = client.open_container else {
                return None;
            };
            let inv = crafting.containers.get_mut(&pos)?;
            if shift {
                quick_move(inv, index, &mut client.main, &crafting.items);
            } else {
                click_slot(
                    inv.slots_mut(),
                    index,
                    &mut client.cursor,
                    right,
                    &crafting.items,
                );
            }
            return Some(pos);
        }
        SlotArea::CraftOutput => unreachable!("handled above"),
    }
    None
}

/// Handles one `DropSlot`: removes one item (or the whole stack, if `all`)
/// from the named slot. Returns the removed stack (if any) for the caller
/// to spawn into the world, plus the chest position to notify of the
/// change, if the slot was a container slot.
pub fn handle_drop_slot(
    client: &mut ClientState,
    crafting: &mut CraftingRes,
    slot: SlotRef,
    all: bool,
) -> (Option<ItemStack>, Option<IVec3>) {
    if matches!(slot.area, SlotArea::CraftOutput) {
        return (None, None);
    }
    let index = slot.index as usize;
    if !slot_usable(slot.area, index, client, &crafting.containers) {
        return (None, None);
    }

    if let SlotArea::Container = slot.area {
        let Some((pos, ContainerKind::Chest)) = client.open_container else {
            return (None, None);
        };
        let Some(inv) = crafting.containers.get_mut(&pos) else {
            return (None, None);
        };
        let count = if all {
            inv.slot(index).map_or(0, |s| s.count)
        } else {
            1
        };
        let taken = inv.take_from(index, count);
        let changed_pos = taken.is_some().then_some(pos);
        return (taken, changed_pos);
    }

    let inv = match slot.area {
        SlotArea::Main => &mut client.main,
        SlotArea::Crafting => &mut client.crafting,
        SlotArea::Container | SlotArea::CraftOutput => unreachable!("handled above"),
    };
    let count = if all {
        inv.slot(index).map_or(0, |s| s.count)
    } else {
        1
    };
    (inv.take_from(index, count), None)
}
