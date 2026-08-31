//! Block identifiers and the block registry.
//!
//! The catalog is deliberately small (see the design discipline in
//! design.md). Colors follow the pop/toy-like art direction (doc/assets.md):
//! no pure black/white, bright values, warm bias. These flat colors are
//! placeholders until the texture pipeline exists; the client renders them as
//! vertex colors.

use serde::{Deserialize, Serialize};

use crate::tool::{TIER_STONE, TIER_WOOD, ToolKind, ToolTier};

/// Identifier of a block type. `0` is always air.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct BlockId(pub u16);

impl BlockId {
    pub const AIR: BlockId = BlockId(0);

    #[inline]
    pub fn is_air(self) -> bool {
        self == Self::AIR
    }
}

/// Static definition of a block type.
#[derive(Clone, Debug)]
pub struct BlockDef {
    pub name: &'static str,
    /// Opaque blocks hide the faces of adjacent opaque blocks (rendering).
    pub opaque: bool,
    /// Solid blocks collide with entities and can be targeted for editing.
    pub solid: bool,
    /// Seconds of hold-to-mine time in survival mode with bare hands and no
    /// harvest penalty (0.0 for blocks that cannot be targeted anyway). See
    /// [`crate::tool::break_time_secs`] for the number actually used.
    pub break_time_secs: f32,
    /// Which tool kind mines this faster, if any (roadmap M6).
    pub tool: Option<ToolKind>,
    /// Minimum tier of [`Self::tool`] required for this block to drop
    /// anything. `None` means bare hands are enough.
    pub harvest_tier: Option<ToolTier>,
    /// Placeholder face colors (sRGB), until real textures exist.
    pub color_top: [u8; 3],
    pub color_side: [u8; 3],
    pub color_bottom: [u8; 3],
    /// What right-clicking this block does; `None` means "nothing special".
    pub interaction: Option<BlockInteraction>,
}

/// Well-known block ids for the prototype catalog.
///
/// Ids must match the order of definitions in [`BlockRegistry::prototype`].
pub mod blocks {
    use super::BlockId;

    pub const AIR: BlockId = BlockId(0);
    pub const STONE: BlockId = BlockId(1);
    pub const DIRT: BlockId = BlockId(2);
    pub const GRASS: BlockId = BlockId(3);
    pub const SAND: BlockId = BlockId(4);
    pub const WATER: BlockId = BlockId(5);
    pub const LOG: BlockId = BlockId(6);
    pub const LEAVES: BlockId = BlockId(7);
    pub const PLANKS: BlockId = BlockId(8);
    pub const CRAFTING_TABLE: BlockId = BlockId(9);
    pub const CHEST: BlockId = BlockId(10);
    pub const COBBLESTONE: BlockId = BlockId(11);
    pub const COAL_ORE: BlockId = BlockId(12);
    pub const IRON_ORE: BlockId = BlockId(13);
    pub const FURNACE: BlockId = BlockId(14);
}

/// What right-clicking a block does, if anything (roadmap M5).
///
/// Blocks without an interaction fall through to normal placement, so this
/// is what the client consults to decide between "open this" and "put a
/// block against this".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockInteraction {
    /// Opens a container UI with its own inventory attached to the block
    /// position (chest).
    Container,
    /// Opens the crafting UI, unlocking the recipes that need a station.
    CraftingTable,
    /// Opens the furnace UI: input, fuel and output slots plus a smelting
    /// progress bar (roadmap M6).
    Furnace,
}

/// Lookup table from [`BlockId`] to [`BlockDef`].
pub struct BlockRegistry {
    defs: Vec<BlockDef>,
}

impl BlockRegistry {
    /// The fixed prototype catalog.
    pub fn prototype() -> Self {
        // Most blocks differ only in a few fields; these builders keep the
        // table readable and make the exceptions (water, ores) stand out.
        let plain = |name, break_time_secs, tool, colors: [[u8; 3]; 3]| BlockDef {
            name,
            opaque: true,
            solid: true,
            break_time_secs,
            tool,
            harvest_tier: None,
            color_top: colors[0],
            color_side: colors[1],
            color_bottom: colors[2],
            interaction: None,
        };
        let mined = |name, break_time_secs, harvest_tier, colors: [[u8; 3]; 3]| BlockDef {
            harvest_tier: Some(harvest_tier),
            ..plain(name, break_time_secs, Some(ToolKind::Pickaxe), colors)
        };

        let defs = vec![
            BlockDef {
                name: "air",
                opaque: false,
                solid: false,
                break_time_secs: 0.0,
                tool: None,
                harvest_tier: None,
                color_top: [0, 0, 0],
                color_side: [0, 0, 0],
                color_bottom: [0, 0, 0],
                interaction: None,
            },
            mined(
                "stone",
                1.5,
                TIER_WOOD,
                [[158, 156, 170], [148, 146, 162], [138, 136, 152]],
            ),
            plain(
                "dirt",
                0.5,
                Some(ToolKind::Shovel),
                [[158, 110, 76], [150, 104, 72], [142, 98, 68]],
            ),
            plain(
                "grass",
                0.6,
                Some(ToolKind::Shovel),
                [[110, 198, 92], [146, 154, 78], [142, 98, 68]],
            ),
            plain(
                "sand",
                0.5,
                Some(ToolKind::Shovel),
                [[240, 218, 150], [232, 210, 142], [224, 202, 134]],
            ),
            BlockDef {
                // Rendered opaque in the prototype; translucency comes later.
                // Not solid: entities pass (and sink) through it.
                opaque: true,
                solid: false,
                ..plain(
                    "water",
                    0.0,
                    None,
                    [[72, 156, 228], [64, 146, 218], [58, 138, 210]],
                )
            },
            plain(
                "log",
                1.25,
                Some(ToolKind::Axe),
                [[172, 138, 94], [128, 98, 66], [172, 138, 94]],
            ),
            plain(
                "leaves",
                0.2,
                None,
                [[82, 172, 74], [74, 160, 68], [66, 148, 62]],
            ),
            plain(
                "planks",
                0.8,
                Some(ToolKind::Axe),
                [[214, 170, 110], [206, 162, 104], [198, 154, 98]],
            ),
            BlockDef {
                interaction: Some(BlockInteraction::CraftingTable),
                ..plain(
                    "crafting_table",
                    0.8,
                    Some(ToolKind::Axe),
                    [[196, 146, 88], [176, 126, 80], [206, 162, 104]],
                )
            },
            BlockDef {
                interaction: Some(BlockInteraction::Container),
                ..plain(
                    "chest",
                    1.0,
                    Some(ToolKind::Axe),
                    [[214, 156, 76], [200, 140, 66], [184, 128, 60]],
                )
            },
            mined(
                "cobblestone",
                1.6,
                TIER_WOOD,
                [[140, 138, 150], [130, 128, 142], [122, 120, 134]],
            ),
            mined(
                "coal_ore",
                2.0,
                TIER_WOOD,
                [[120, 118, 130], [110, 108, 120], [104, 102, 114]],
            ),
            mined(
                "iron_ore",
                2.5,
                TIER_STONE,
                [[190, 166, 140], [180, 156, 132], [170, 148, 126]],
            ),
            BlockDef {
                interaction: Some(BlockInteraction::Furnace),
                ..mined(
                    "furnace",
                    2.0,
                    TIER_WOOD,
                    [[132, 130, 142], [112, 110, 124], [120, 118, 130]],
                )
            },
        ];
        Self { defs }
    }

    #[inline]
    pub fn get(&self, id: BlockId) -> &BlockDef {
        &self.defs[id.0 as usize]
    }

    /// `true` if `id` refers to a defined block type.
    #[inline]
    pub fn is_valid(&self, id: BlockId) -> bool {
        (id.0 as usize) < self.defs.len()
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `blocks` module's constants are hand-written indices into
    /// `prototype()`'s vec; a definition inserted in the middle would silently
    /// renumber everything after it.
    #[test]
    fn block_constants_match_their_definitions() {
        let reg = BlockRegistry::prototype();
        for (id, name) in [
            (blocks::AIR, "air"),
            (blocks::STONE, "stone"),
            (blocks::DIRT, "dirt"),
            (blocks::GRASS, "grass"),
            (blocks::SAND, "sand"),
            (blocks::WATER, "water"),
            (blocks::LOG, "log"),
            (blocks::LEAVES, "leaves"),
            (blocks::PLANKS, "planks"),
            (blocks::CRAFTING_TABLE, "crafting_table"),
            (blocks::CHEST, "chest"),
            (blocks::COBBLESTONE, "cobblestone"),
            (blocks::COAL_ORE, "coal_ore"),
            (blocks::IRON_ORE, "iron_ore"),
            (blocks::FURNACE, "furnace"),
        ] {
            assert_eq!(reg.get(id).name, name, "block id {id:?}");
        }
        assert_eq!(reg.len(), 15, "a block was added without a constant");
    }

    #[test]
    fn every_gated_block_names_the_tool_that_gates_it() {
        let reg = BlockRegistry::prototype();
        for id in 0..reg.len() as u16 {
            let def = reg.get(BlockId(id));
            if def.harvest_tier.is_some() {
                assert!(
                    def.tool.is_some(),
                    "{} gates drops but names no tool kind, so nothing could ever harvest it",
                    def.name
                );
            }
        }
    }
}
