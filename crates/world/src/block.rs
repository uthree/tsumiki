//! Block identifiers and the block registry.
//!
//! The catalog is deliberately small (see the design discipline in
//! design.md). Colors follow the
//! pop/toy-like art direction (doc/assets.md): no pure black/white, bright
//! values, warm bias. These flat colors are placeholders until the texture
//! pipeline exists; the client renders them as vertex colors.

use serde::{Deserialize, Serialize};

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
    /// Seconds of hold-to-mine time in survival mode (0.0 for blocks that
    /// cannot be targeted anyway).
    pub break_time_secs: f32,
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
    /// Opens the 3x3 crafting UI; holds no items of its own.
    CraftingTable,
}

/// Lookup table from [`BlockId`] to [`BlockDef`].
pub struct BlockRegistry {
    defs: Vec<BlockDef>,
}

impl BlockRegistry {
    /// The fixed prototype catalog.
    pub fn prototype() -> Self {
        let defs = vec![
            BlockDef {
                name: "air",
                opaque: false,
                solid: false,
                break_time_secs: 0.0,
                color_top: [0, 0, 0],
                color_side: [0, 0, 0],
                color_bottom: [0, 0, 0],
                interaction: None,
            },
            BlockDef {
                name: "stone",
                opaque: true,
                solid: true,
                break_time_secs: 1.5,
                color_top: [158, 156, 170],
                color_side: [148, 146, 162],
                color_bottom: [138, 136, 152],
                interaction: None,
            },
            BlockDef {
                name: "dirt",
                opaque: true,
                solid: true,
                break_time_secs: 0.5,
                color_top: [158, 110, 76],
                color_side: [150, 104, 72],
                color_bottom: [142, 98, 68],
                interaction: None,
            },
            BlockDef {
                name: "grass",
                opaque: true,
                solid: true,
                break_time_secs: 0.6,
                color_top: [110, 198, 92],
                color_side: [146, 154, 78],
                color_bottom: [142, 98, 68],
                interaction: None,
            },
            BlockDef {
                name: "sand",
                opaque: true,
                solid: true,
                break_time_secs: 0.5,
                color_top: [240, 218, 150],
                color_side: [232, 210, 142],
                color_bottom: [224, 202, 134],
                interaction: None,
            },
            BlockDef {
                // Rendered opaque in the prototype; translucency comes later.
                // Not solid: entities pass (and sink) through it.
                name: "water",
                opaque: true,
                solid: false,
                break_time_secs: 0.0,
                color_top: [72, 156, 228],
                color_side: [64, 146, 218],
                color_bottom: [58, 138, 210],
                interaction: None,
            },
            BlockDef {
                name: "log",
                opaque: true,
                solid: true,
                break_time_secs: 1.25,
                color_top: [172, 138, 94],
                color_side: [128, 98, 66],
                color_bottom: [172, 138, 94],
                interaction: None,
            },
            BlockDef {
                name: "leaves",
                opaque: true,
                solid: true,
                break_time_secs: 0.2,
                color_top: [82, 172, 74],
                color_side: [74, 160, 68],
                color_bottom: [66, 148, 62],
                interaction: None,
            },
            BlockDef {
                name: "planks",
                opaque: true,
                solid: true,
                break_time_secs: 0.8,
                color_top: [214, 170, 110],
                color_side: [206, 162, 104],
                color_bottom: [198, 154, 98],
                interaction: None,
            },
            BlockDef {
                name: "crafting_table",
                opaque: true,
                solid: true,
                break_time_secs: 0.8,
                color_top: [196, 146, 88],
                color_side: [176, 126, 80],
                color_bottom: [206, 162, 104],
                interaction: Some(BlockInteraction::CraftingTable),
            },
            BlockDef {
                name: "chest",
                opaque: true,
                solid: true,
                break_time_secs: 1.0,
                color_top: [214, 156, 76],
                color_side: [200, 140, 66],
                color_bottom: [184, 128, 60],
                interaction: Some(BlockInteraction::Container),
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
