//! Inventory slot operations and container plumbing (design.md §7, roadmap
//! M5): `SlotClick`, `DropSlot`, and the fresh `InventoryUpdate` snapshot the
//! server answers every slot-affecting message with. Crafting itself lives
//! in `lib.rs`'s `Craft` handler (recipes are crafted by id straight out of
//! the main inventory -- there is no crafting-grid slot area anymore).
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
use tsumiki_world::inventory::{CHEST_SIZE, click_slot, quick_move};
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

/// `true` if `index` is addressable for `area` in `client`'s current state.
fn slot_usable(
    area: SlotArea,
    index: usize,
    client: &ClientState,
    containers: &HashMap<IVec3, Inventory>,
) -> bool {
    match area {
        SlotArea::Main => index < MAIN_INVENTORY_SIZE,
        SlotArea::Container => match client.open_container {
            Some((pos, ContainerKind::Chest)) => {
                containers.contains_key(&pos) && index < CHEST_SIZE
            }
            _ => false,
        },
    }
}

/// Computes the full `InventoryUpdate` snapshot for `client`. Which recipes
/// are craftable is deliberately not part of this: the client derives that
/// itself from the same recipe registry plus this snapshot's `main`.
pub fn inventory_snapshot(client: &ClientState) -> ServerToClient {
    ServerToClient::InventoryUpdate {
        main: client.main.to_vec(),
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

    let inv = &mut client.main;
    let count = if all {
        inv.slot(index).map_or(0, |s| s.count)
    } else {
        1
    };
    (inv.take_from(index, count), None)
}
