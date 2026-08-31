//! Tools: what mines faster, and what a block will actually drop
//! (roadmap M6).
//!
//! Two separate questions, deliberately kept apart:
//!
//! - **Speed**: the right *kind* of tool mines a block faster. This is a
//!   convenience and never gates anything.
//! - **Harvest**: some blocks only yield an item when broken with a tool of
//!   the right kind at or above a minimum *tier*. This is the gate, and it
//!   is what makes the wood -> stone -> iron progression a progression.
//!
//! Mining a block you cannot harvest still works -- it just takes longer and
//! yields nothing, so a player learns the rule by losing a block rather than
//! by being blocked with no explanation.

use crate::block::BlockDef;
use crate::item::ItemDef;

/// What a tool is for. A block names at most one of these as the tool that
/// speeds it up (and, if it gates drops, as the kind required).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolKind {
    Pickaxe,
    Axe,
    Shovel,
}

/// Material tier, low to high. Ordering is the whole point, so this is a
/// plain integer rather than an enum: `harvest_tier` compares against it.
pub type ToolTier = u8;

pub const TIER_WOOD: ToolTier = 0;
pub const TIER_STONE: ToolTier = 1;
pub const TIER_IRON: ToolTier = 2;

/// The tool half of an [`ItemDef`]: items without one are not tools.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ToolDef {
    pub kind: ToolKind,
    pub tier: ToolTier,
    /// Divides the block's base break time when the kinds match. Higher
    /// tiers are faster -- design.md's "the same thing, faster" rule, applied
    /// to the very first tier ladder in the game.
    pub speed: f32,
    /// Uses before the tool breaks.
    pub durability: u16,
}

/// Multiplier applied to break time when a block's harvest gate is not met.
///
/// Deliberately punishing but finite: the block still breaks, so the rule is
/// discoverable in-world (you mined it and got nothing) rather than being an
/// invisible prohibition.
pub const WRONG_TOOL_PENALTY: f32 = 3.0;

/// Seconds of hold-to-mine for `block` with `tool` in hand (`None` = bare
/// hands).
///
/// Pure, so the client can predict the progress bar and the server can check
/// the same number without either owning the rule.
pub fn break_time_secs(block: &BlockDef, tool: Option<&ToolDef>) -> f32 {
    let base = block.break_time_secs;
    if base <= 0.0 {
        return 0.0;
    }
    let matching = tool.filter(|t| Some(t.kind) == block.tool);
    let with_speed = match matching {
        Some(t) if t.speed > 0.0 => base / t.speed,
        _ => base,
    };
    if can_harvest(block, tool) {
        with_speed
    } else {
        with_speed * WRONG_TOOL_PENALTY
    }
}

/// Whether breaking `block` with `tool` yields its drop at all.
pub fn can_harvest(block: &BlockDef, tool: Option<&ToolDef>) -> bool {
    let Some(required) = block.harvest_tier else {
        return true;
    };
    tool.is_some_and(|t| Some(t.kind) == block.tool && t.tier >= required)
}

/// The tool an item is, if it is one.
pub fn tool_of(def: &ItemDef) -> Option<&ToolDef> {
    def.tool.as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{BlockRegistry, blocks};
    use crate::item::{ItemRegistry, items};

    fn tool(reg: &ItemRegistry, id: crate::item::ItemId) -> ToolDef {
        *tool_of(reg.get(id)).expect("item should be a tool")
    }

    #[test]
    fn the_right_tool_is_faster() {
        let blocks_reg = BlockRegistry::prototype();
        let items_reg = ItemRegistry::prototype();
        let stone = blocks_reg.get(blocks::STONE);
        let pick = tool(&items_reg, items::STONE_PICKAXE);

        assert!(break_time_secs(stone, Some(&pick)) < break_time_secs(stone, None));
    }

    #[test]
    fn a_higher_tier_is_faster_still() {
        let blocks_reg = BlockRegistry::prototype();
        let items_reg = ItemRegistry::prototype();
        let stone = blocks_reg.get(blocks::STONE);
        let wood = tool(&items_reg, items::WOODEN_PICKAXE);
        let iron = tool(&items_reg, items::IRON_PICKAXE);

        assert!(break_time_secs(stone, Some(&iron)) < break_time_secs(stone, Some(&wood)));
    }

    #[test]
    fn the_wrong_kind_of_tool_gives_no_bonus() {
        let blocks_reg = BlockRegistry::prototype();
        let items_reg = ItemRegistry::prototype();
        let stone = blocks_reg.get(blocks::STONE);
        let axe = tool(&items_reg, items::IRON_AXE);

        assert_eq!(
            break_time_secs(stone, Some(&axe)),
            break_time_secs(stone, None),
            "an axe should neither help nor hurt on stone beyond the gate"
        );
    }

    #[test]
    fn bare_hands_cannot_harvest_a_gated_block() {
        let blocks_reg = BlockRegistry::prototype();
        let stone = blocks_reg.get(blocks::STONE);

        assert!(!can_harvest(stone, None));
    }

    #[test]
    fn an_ungated_block_is_harvestable_by_hand() {
        let blocks_reg = BlockRegistry::prototype();
        for block in [blocks::DIRT, blocks::LOG, blocks::SAND, blocks::LEAVES] {
            assert!(can_harvest(blocks_reg.get(block), None), "{block:?}");
        }
    }

    #[test]
    fn iron_ore_needs_a_stone_pickaxe() {
        let blocks_reg = BlockRegistry::prototype();
        let items_reg = ItemRegistry::prototype();
        let ore = blocks_reg.get(blocks::IRON_ORE);

        assert!(!can_harvest(
            ore,
            Some(&tool(&items_reg, items::WOODEN_PICKAXE))
        ));
        assert!(can_harvest(
            ore,
            Some(&tool(&items_reg, items::STONE_PICKAXE))
        ));
        assert!(can_harvest(
            ore,
            Some(&tool(&items_reg, items::IRON_PICKAXE))
        ));
    }

    #[test]
    fn failing_the_gate_is_slower_than_meeting_it() {
        let blocks_reg = BlockRegistry::prototype();
        let items_reg = ItemRegistry::prototype();
        let ore = blocks_reg.get(blocks::IRON_ORE);
        let wood = tool(&items_reg, items::WOODEN_PICKAXE);
        let stone = tool(&items_reg, items::STONE_PICKAXE);

        assert!(break_time_secs(ore, Some(&wood)) > break_time_secs(ore, Some(&stone)));
    }

    #[test]
    fn unbreakable_blocks_stay_instant() {
        let blocks_reg = BlockRegistry::prototype();
        assert_eq!(break_time_secs(blocks_reg.get(blocks::AIR), None), 0.0);
    }
}
