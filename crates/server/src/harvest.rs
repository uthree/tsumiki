//! Harvest gating and tool durability for `BreakBlock` (roadmap M6).
//!
//! `BreakBlock` names an explicit hotbar slot, exactly like `PlaceBlock`
//! does for placing: the server must not have to guess which of several
//! tools was in hand, and a client must not be able to claim a better one
//! than it actually has selected.
//!
//! Durability decision (see [`wear_tool`]): a tool that matches the block's
//! required kind wears down whether or not its tier actually met the
//! harvest gate. This matches Minecraft's own rule for gated blocks -- a
//! wood pickaxe swung at iron ore still takes damage even though it yields
//! nothing, which is how a player discovers "this tool is too weak" instead
//! of the game silently refusing to spend it.

use tsumiki_world::Inventory;
use tsumiki_world::block::BlockDef;
use tsumiki_world::item::ItemRegistry;
use tsumiki_world::tool::can_harvest;

/// The outcome of resolving one `BreakBlock` against the item in `hotbar`.
pub struct HarvestOutcome {
    /// The hotbar slot passed in, if it holds a tool matching the block's
    /// required kind (regardless of whether its tier was high enough to
    /// actually harvest). `None` if the slot is empty or holds something
    /// that isn't the right *kind* of tool for this block -- either way,
    /// nothing to wear.
    pub tool_slot: Option<usize>,
    /// Whether the block's drop should be spawned as a dropped item.
    pub drop_allowed: bool,
}

/// Resolves what tool (if any) applies to breaking `block` with whatever is
/// in `main`'s `hotbar` slot, and whether that is enough to harvest it.
/// `hotbar` is assumed already range-checked by the caller (see
/// `ClientToServer::BreakBlock`'s doc comment) -- an out-of-range index
/// safely resolves to "no tool" rather than panicking, since
/// [`Inventory::slot`] is itself bounds-checked.
pub fn resolve_harvest(
    block: &BlockDef,
    main: &Inventory,
    hotbar: usize,
    item_reg: &ItemRegistry,
) -> HarvestOutcome {
    let held_tool = main
        .slot(hotbar)
        .and_then(|s| item_reg.tool(s.item).copied());
    let tool_slot = block
        .tool
        .is_some_and(|kind| held_tool.is_some_and(|t| t.kind == kind))
        .then_some(hotbar);
    HarvestOutcome {
        tool_slot,
        drop_allowed: can_harvest(block, held_tool.as_ref()),
    }
}

/// Wears the tool at `slot` (if any) by one use, destroying the stack once it
/// reaches its durability limit. A no-op if `slot` is `None` or no longer
/// names a tool -- shouldn't happen within one `BreakBlock`'s handling
/// (nothing else touches the inventory in between), but keeps this total
/// rather than panicking on a stale index.
pub fn wear_tool(main: &mut Inventory, slot: Option<usize>, item_reg: &ItemRegistry) {
    let Some(i) = slot else { return };
    let Some(stack) = main.slot(i) else { return };
    let Some(tool) = item_reg.tool(stack.item) else {
        return;
    };
    let worn = stack.damage + 1;
    let surviving = (worn < tool.durability).then(|| stack.with_damage(worn));
    main.set_slot(i, surviving);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsumiki_world::block::{BlockRegistry, blocks};
    use tsumiki_world::inventory::HOTBAR_SIZE;
    use tsumiki_world::item::{ItemStack, items};

    fn regs() -> (BlockRegistry, ItemRegistry) {
        (BlockRegistry::prototype(), ItemRegistry::prototype())
    }

    #[test]
    fn bare_hands_never_harvest_a_gated_block() {
        let (blocks_reg, item_reg) = regs();
        let stone = blocks_reg.get(blocks::STONE);
        let main = Inventory::new(HOTBAR_SIZE);

        let outcome = resolve_harvest(stone, &main, 0, &item_reg);

        assert!(outcome.tool_slot.is_none());
        assert!(!outcome.drop_allowed);
    }

    #[test]
    fn the_right_tool_in_the_named_slot_harvests() {
        let (blocks_reg, item_reg) = regs();
        let stone = blocks_reg.get(blocks::STONE);
        let mut main = Inventory::new(HOTBAR_SIZE);
        main.set_slot(4, Some(ItemStack::one(items::WOODEN_PICKAXE)));

        let outcome = resolve_harvest(stone, &main, 4, &item_reg);

        assert_eq!(outcome.tool_slot, Some(4));
        assert!(outcome.drop_allowed);
    }

    #[test]
    fn a_tool_elsewhere_in_the_hotbar_is_never_consulted() {
        // The bug this pins shut: a better tool sitting in a slot the
        // player did NOT select must not be picked up by the gate, and must
        // not wear down either -- only the named slot is "in hand".
        let (blocks_reg, item_reg) = regs();
        let iron_ore = blocks_reg.get(blocks::IRON_ORE);
        let mut main = Inventory::new(HOTBAR_SIZE);
        main.set_slot(0, Some(ItemStack::one(items::WOODEN_PICKAXE)));
        main.set_slot(3, Some(ItemStack::one(items::STONE_PICKAXE)));

        // Slot 3 (the stone pickaxe) selected: harvests normally.
        let good = resolve_harvest(iron_ore, &main, 3, &item_reg);
        assert_eq!(good.tool_slot, Some(3));
        assert!(good.drop_allowed);

        // Slot 0 (the wooden pickaxe) selected: denied, even though a
        // perfectly good stone pickaxe is sitting right there in the same
        // hotbar.
        let weak = resolve_harvest(iron_ore, &main, 0, &item_reg);
        assert_eq!(weak.tool_slot, Some(0));
        assert!(!weak.drop_allowed);
    }

    #[test]
    fn too_low_a_tier_still_identifies_the_tool_but_denies_the_drop() {
        let (blocks_reg, item_reg) = regs();
        let iron_ore = blocks_reg.get(blocks::IRON_ORE);
        let mut main = Inventory::new(HOTBAR_SIZE);
        main.set_slot(0, Some(ItemStack::one(items::WOODEN_PICKAXE)));

        let outcome = resolve_harvest(iron_ore, &main, 0, &item_reg);

        assert_eq!(
            outcome.tool_slot,
            Some(0),
            "the wrong-tier tool is still the one in hand"
        );
        assert!(!outcome.drop_allowed);
    }

    #[test]
    fn a_tool_of_the_wrong_kind_is_never_selected() {
        let (blocks_reg, item_reg) = regs();
        let stone = blocks_reg.get(blocks::STONE);
        let mut main = Inventory::new(HOTBAR_SIZE);
        main.set_slot(0, Some(ItemStack::one(items::WOODEN_AXE)));

        let outcome = resolve_harvest(stone, &main, 0, &item_reg);

        assert!(outcome.tool_slot.is_none());
        assert!(!outcome.drop_allowed);
    }

    #[test]
    fn an_out_of_range_slot_resolves_to_bare_handed_without_panicking() {
        let (blocks_reg, item_reg) = regs();
        let stone = blocks_reg.get(blocks::STONE);
        let main = Inventory::new(HOTBAR_SIZE);

        let outcome = resolve_harvest(stone, &main, 99, &item_reg);

        assert!(outcome.tool_slot.is_none());
        assert!(!outcome.drop_allowed);
    }

    #[test]
    fn wearing_a_tool_increments_damage() {
        let (_, item_reg) = regs();
        let mut main = Inventory::new(HOTBAR_SIZE);
        main.set_slot(0, Some(ItemStack::one(items::WOODEN_PICKAXE)));

        wear_tool(&mut main, Some(0), &item_reg);

        assert_eq!(
            main.slot(0),
            Some(ItemStack::one(items::WOODEN_PICKAXE).with_damage(1))
        );
    }

    #[test]
    fn a_tool_breaks_at_its_durability_limit() {
        let (_, item_reg) = regs();
        let durability = item_reg.tool(items::WOODEN_PICKAXE).unwrap().durability;
        let mut main = Inventory::new(HOTBAR_SIZE);
        main.set_slot(
            0,
            Some(ItemStack::one(items::WOODEN_PICKAXE).with_damage(durability - 1)),
        );

        wear_tool(&mut main, Some(0), &item_reg);

        assert_eq!(main.slot(0), None, "the tool should have broken");
    }
}
