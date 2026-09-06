//! Block identifiers and the block registry.
//!
//! The catalog is deliberately small (see the design discipline in
//! design.md). Colors follow the pop/toy-like art direction (doc/assets.md):
//! no pure black/white, bright values, warm bias. Far terrain uses colors
//! extracted from the generated block textures; the client renders them as
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
    /// Solid blocks collide with entities.
    pub solid: bool,
    /// Light absorbed on entry, from 0 (clear) to 15 (opaque). Propagated
    /// light loses at least one level per step, except direct vertical sky.
    pub light_opacity: u8,
    /// Emitted block light, with four bits per RGB channel.
    pub light_emission: [u8; 3],
    /// Seconds of hold-to-mine time in survival mode with bare hands and no
    /// harvest penalty (0.0 for blocks that cannot be targeted anyway). See
    /// [`crate::tool::break_time_secs`] for the number actually used.
    pub break_time_secs: f32,
    /// Which tool kind mines this faster, if any (roadmap M6).
    pub tool: Option<ToolKind>,
    /// Minimum tier of [`Self::tool`] required for this block to drop
    /// anything. `None` means bare hands are enough.
    pub harvest_tier: Option<ToolTier>,
    /// Texture-derived representative face colors (sRGB), used by far LOD.
    pub color_top: [u8; 3],
    pub color_side: [u8; 3],
    pub color_bottom: [u8; 3],
    /// What right-clicking this block does; `None` means "nothing special".
    pub interaction: Option<BlockInteraction>,
}

impl BlockDef {
    /// Non-colliding light sources can still be selected and mined.
    pub fn is_targetable(&self) -> bool {
        self.solid || self.light_emission != [0; 3] || self.name.starts_with("wheat_")
    }
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
    pub const TORCH: BlockId = BlockId(15);
    pub const DEMO_RED_LIGHT: BlockId = BlockId(16);
    pub const DEMO_GREEN_LIGHT: BlockId = BlockId(17);
    pub const DEMO_BLUE_LIGHT: BlockId = BlockId(18);
    pub const FARMLAND: BlockId = BlockId(19);
    pub const WHEAT_YOUNG: BlockId = BlockId(20);
    pub const WHEAT_MATURE: BlockId = BlockId(21);
    pub const MINER: BlockId = BlockId(22);
    pub const BELT: BlockId = BlockId(23);
    pub const POWERED_FURNACE: BlockId = BlockId(24);
    pub const FACTORY_STORAGE: BlockId = BlockId(25);
    pub const GENERATOR: BlockId = BlockId(26);
    pub const SNOW: BlockId = BlockId(27);
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
    /// Opens the factory's buffers, direction and production status.
    Factory,
}

/// Lookup table from [`BlockId`] to [`BlockDef`].
pub struct BlockRegistry {
    defs: Vec<BlockDef>,
}

#[derive(Deserialize)]
struct TextureColors {
    id: usize,
    top: [u8; 3],
    side: [u8; 3],
    bottom: [u8; 3],
}

fn texture_colors() -> &'static [TextureColors] {
    static COLORS: std::sync::OnceLock<Vec<TextureColors>> = std::sync::OnceLock::new();
    COLORS.get_or_init(|| {
        serde_json::from_str(include_str!("../../../assets/lod_colors.json"))
            .expect("generated LOD colors must be valid; run the asset generator")
    })
}

impl BlockRegistry {
    /// The fixed prototype catalog.
    pub fn prototype() -> Self {
        // Most blocks differ only in a few fields; these builders keep the
        // table readable and make the exceptions (water, ores) stand out.
        let plain = |name, break_time_secs, tool| BlockDef {
            name,
            opaque: true,
            solid: true,
            light_opacity: 15,
            light_emission: [0; 3],
            break_time_secs,
            tool,
            harvest_tier: None,
            color_top: [0; 3],
            color_side: [0; 3],
            color_bottom: [0; 3],
            interaction: None,
        };
        let mined = |name, break_time_secs, harvest_tier| BlockDef {
            harvest_tier: Some(harvest_tier),
            ..plain(name, break_time_secs, Some(ToolKind::Pickaxe))
        };

        let mut defs = vec![
            BlockDef {
                name: "air",
                opaque: false,
                solid: false,
                light_opacity: 0,
                light_emission: [0; 3],
                break_time_secs: 0.0,
                tool: None,
                harvest_tier: None,
                color_top: [0, 0, 0],
                color_side: [0, 0, 0],
                color_bottom: [0, 0, 0],
                interaction: None,
            },
            mined("stone", 1.5, TIER_WOOD),
            plain("dirt", 0.5, Some(ToolKind::Shovel)),
            plain("grass", 0.6, Some(ToolKind::Shovel)),
            plain("sand", 0.5, Some(ToolKind::Shovel)),
            BlockDef {
                // Rendered opaque in the prototype; translucency comes later.
                // Not solid: entities pass (and sink) through it.
                opaque: true,
                solid: false,
                light_opacity: 2,
                ..plain("water", 0.0, None)
            },
            plain("log", 1.25, Some(ToolKind::Axe)),
            plain("leaves", 0.2, None),
            plain("planks", 0.8, Some(ToolKind::Axe)),
            BlockDef {
                interaction: Some(BlockInteraction::CraftingTable),
                ..plain("crafting_table", 0.8, Some(ToolKind::Axe))
            },
            BlockDef {
                interaction: Some(BlockInteraction::Container),
                ..plain("chest", 1.0, Some(ToolKind::Axe))
            },
            mined("cobblestone", 1.6, TIER_WOOD),
            mined("coal_ore", 2.0, TIER_WOOD),
            mined("iron_ore", 2.5, TIER_STONE),
            BlockDef {
                interaction: Some(BlockInteraction::Furnace),
                ..mined("furnace", 2.0, TIER_WOOD)
            },
            BlockDef {
                opaque: false,
                solid: false,
                light_opacity: 0,
                light_emission: [15, 12, 8],
                ..plain("torch", 0.1, None)
            },
            BlockDef {
                light_emission: [15, 0, 0],
                ..plain("demo_red_light", 0.1, None)
            },
            BlockDef {
                light_emission: [0, 15, 0],
                ..plain("demo_green_light", 0.1, None)
            },
            BlockDef {
                light_emission: [0, 0, 15],
                ..plain("demo_blue_light", 0.1, None)
            },
        ];
        defs.push(plain("farmland", 0.5, Some(ToolKind::Shovel)));
        for name in ["wheat_young", "wheat_mature"] {
            defs.push(BlockDef {
                opaque: false,
                solid: false,
                light_opacity: 0,
                ..plain(name, 0.1, None)
            });
        }
        for name in [
            "miner",
            "belt",
            "powered_furnace",
            "factory_storage",
            "generator",
        ] {
            defs.push(BlockDef {
                interaction: Some(BlockInteraction::Factory),
                ..mined(name, 2.0, TIER_WOOD)
            });
        }
        defs.push(plain("snow", 0.3, Some(ToolKind::Shovel)));
        let colors = texture_colors();
        assert_eq!(defs.len(), colors.len(), "regenerate the block assets");
        for (id, (def, colors)) in defs.iter_mut().zip(colors).enumerate() {
            assert_eq!(id, colors.id, "generated block colors must be in id order");
            def.color_top = colors.top;
            def.color_side = colors.side;
            def.color_bottom = colors.bottom;
        }
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
            (blocks::TORCH, "torch"),
            (blocks::DEMO_RED_LIGHT, "demo_red_light"),
            (blocks::DEMO_GREEN_LIGHT, "demo_green_light"),
            (blocks::DEMO_BLUE_LIGHT, "demo_blue_light"),
        ] {
            assert_eq!(reg.get(id).name, name, "block id {id:?}");
        }
        assert_eq!(reg.get(blocks::SNOW).name, "snow");
        assert_eq!(reg.len(), 28, "a block was added without a constant");
    }

    #[test]
    fn generated_atlas_matches_the_game_catalog_and_face_order() {
        let atlas: serde_json::Value =
            serde_json::from_str(include_str!("../../../assets/atlas.json")).unwrap();
        assert_eq!(atlas["tile_size"], 16);
        assert_eq!(atlas["size"], serde_json::json!([128, 336]));
        assert_eq!(
            atlas["face_order"],
            serde_json::json!(["-X", "+X", "-Y", "+Y", "-Z", "+Z"])
        );
        let blocks = atlas["blocks"].as_array().unwrap();
        let registry = BlockRegistry::prototype();
        assert_eq!(blocks.len(), registry.len());
        for (id, block) in blocks.iter().enumerate() {
            assert_eq!(block["id"], id);
            assert_eq!(block["name"], registry.get(BlockId(id as u16)).name);
            let faces = block["faces"].as_array().unwrap();
            assert_eq!(faces.len(), 6);
            for (face, tile) in faces.iter().enumerate() {
                let index = id * 6 + face;
                assert_eq!(tile["index"], index);
                assert_eq!(
                    tile["rect"],
                    serde_json::json!([index % 8 * 16, index / 8 * 16, 16, 16])
                );
            }
        }
    }

    #[test]
    fn torch_is_selectable_without_blocking_movement_or_light() {
        let reg = BlockRegistry::prototype();
        let torch = reg.get(blocks::TORCH);
        assert!(torch.is_targetable());
        assert!(!torch.solid);
        assert!(!torch.opaque);
        assert_eq!(torch.light_opacity, 0);
        assert_eq!(torch.light_emission, [15, 12, 8]);
        assert!(!reg.get(blocks::WATER).is_targetable());
        assert!(!reg.get(blocks::AIR).is_targetable());
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
