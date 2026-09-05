//! Items and the item registry (design.md §7, roadmap M5/M6).
//!
//! An item is not a block. [`BlockId`] names something that occupies a cell
//! in the world; [`ItemId`] names something that occupies an inventory slot.
//! They are related only by two explicit mappings kept here -- what a block
//! drops when broken, and what block an item places -- and plenty of items
//! (sticks, ingots, tools) have neither.

use serde::{Deserialize, Serialize};

use crate::block::{BlockId, blocks};
use crate::tool::{TIER_IRON, TIER_STONE, TIER_WOOD, ToolDef, ToolKind};

/// Identifier of an item type. `0` is reserved and never a real item, so a
/// zeroed/defaulted id is recognisably invalid rather than silently meaning
/// "stone" (mirroring [`BlockId::AIR`]). Absence is expressed as
/// `Option<ItemStack>`, not as an id.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ItemId(pub u16);

/// A quantity of one item type. `count` is always `>= 1`: an empty slot is
/// `None`, never a zero-count stack.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ItemStack {
    pub item: ItemId,
    pub count: u32,
    /// Uses already spent, for items with a [`ToolDef`]; 0 for everything
    /// else. Two stacks only merge when this matches, so a half-worn pickaxe
    /// never silently averages with a fresh one.
    pub damage: u16,
}

impl ItemStack {
    pub const fn new(item: ItemId, count: u32) -> Self {
        Self {
            item,
            count,
            damage: 0,
        }
    }

    pub const fn one(item: ItemId) -> Self {
        Self::new(item, 1)
    }

    /// The same stack with `damage` uses spent.
    pub const fn with_damage(self, damage: u16) -> Self {
        Self { damage, ..self }
    }

    /// Whether two stacks are the same *kind* of thing, and so may merge.
    pub fn mergeable_with(self, other: ItemStack) -> bool {
        self.item == other.item && self.damage == other.damage
    }
}

/// Static definition of an item type.
#[derive(Clone, Debug)]
pub struct ItemDef {
    pub name: &'static str,
    /// Maximum count in one slot. Tools use 1, since each carries its own
    /// wear.
    pub max_stack: u32,
    /// Block this item places when used against a surface, if any.
    pub places: Option<BlockId>,
    /// What this item is as a tool, if it is one (roadmap M6).
    pub tool: Option<ToolDef>,
    /// Temporary light-demo items are restricted to creative placement.
    pub demo_only: bool,
}

/// Well-known item ids for the prototype catalog.
///
/// Ids must match the order of definitions in [`ItemRegistry::prototype`],
/// which starts at 1 (see [`ItemId`]).
pub mod items {
    use super::ItemId;

    pub const STONE: ItemId = ItemId(1);
    pub const DIRT: ItemId = ItemId(2);
    pub const GRASS: ItemId = ItemId(3);
    pub const SAND: ItemId = ItemId(4);
    pub const LOG: ItemId = ItemId(5);
    pub const LEAVES: ItemId = ItemId(6);
    pub const PLANKS: ItemId = ItemId(7);
    pub const CRAFTING_TABLE: ItemId = ItemId(8);
    pub const CHEST: ItemId = ItemId(9);
    pub const STICK: ItemId = ItemId(10);
    pub const COBBLESTONE: ItemId = ItemId(11);
    pub const COAL: ItemId = ItemId(12);
    pub const IRON_ORE: ItemId = ItemId(13);
    pub const IRON_INGOT: ItemId = ItemId(14);
    pub const FURNACE: ItemId = ItemId(15);
    pub const WOODEN_PICKAXE: ItemId = ItemId(16);
    pub const WOODEN_AXE: ItemId = ItemId(17);
    pub const WOODEN_SHOVEL: ItemId = ItemId(18);
    pub const STONE_PICKAXE: ItemId = ItemId(19);
    pub const STONE_AXE: ItemId = ItemId(20);
    pub const STONE_SHOVEL: ItemId = ItemId(21);
    pub const IRON_PICKAXE: ItemId = ItemId(22);
    pub const IRON_AXE: ItemId = ItemId(23);
    pub const IRON_SHOVEL: ItemId = ItemId(24);
    pub const TORCH: ItemId = ItemId(25);
    pub const DEMO_RED_LIGHT: ItemId = ItemId(26);
    pub const DEMO_GREEN_LIGHT: ItemId = ItemId(27);
    pub const DEMO_BLUE_LIGHT: ItemId = ItemId(28);
    pub const FARMLAND: ItemId = ItemId(29);
    pub const WHEAT_SEEDS: ItemId = ItemId(30);
    pub const WHEAT: ItemId = ItemId(31);
    pub const BREAD: ItemId = ItemId(32);
    pub const TOAST: ItemId = ItemId(33);
    pub const MINER: ItemId = ItemId(34);
    pub const BELT: ItemId = ItemId(35);
    pub const POWERED_FURNACE: ItemId = ItemId(36);
    pub const FACTORY_STORAGE: ItemId = ItemId(37);
    pub const GENERATOR: ItemId = ItemId(38);
}

/// Hide the demo items and reject their placement when false. Keep their
/// registry IDs and textures so existing saved worlds remain readable.
pub const DEMO_LIGHTS_ENABLED: bool = true;

/// Default stack size, shared by everything that is not a tool.
pub const DEFAULT_MAX_STACK: u32 = 64;

/// Lookup tables for items, plus the block <-> item mappings.
pub struct ItemRegistry {
    /// Indexed by [`ItemId`]; index 0 is the reserved placeholder.
    defs: Vec<ItemDef>,
    /// Indexed by [`BlockId`]: what breaking that block yields, if anything.
    drops: Vec<Option<ItemStack>>,
}

impl ItemRegistry {
    /// The fixed prototype catalog.
    pub fn prototype() -> Self {
        // Index 0: reserved, never referenced by a real stack.
        let mut defs = vec![ItemDef {
            name: "<none>",
            max_stack: 0,
            places: None,
            tool: None,
            demo_only: false,
        }];

        let block_item = |name, block: BlockId| ItemDef {
            name,
            max_stack: DEFAULT_MAX_STACK,
            places: Some(block),
            tool: None,
            demo_only: false,
        };
        let material = |name| ItemDef {
            name,
            max_stack: DEFAULT_MAX_STACK,
            places: None,
            tool: None,
            demo_only: false,
        };
        // Higher tiers are strictly faster and last longer -- design.md's
        // "the same thing, faster", which is why a tier needs no new
        // materials of its own beyond the one it is named for.
        let tool_item = |name, kind, tier, speed, durability| ItemDef {
            name,
            max_stack: 1,
            places: None,
            tool: Some(ToolDef {
                kind,
                tier,
                speed,
                durability,
            }),
            demo_only: false,
        };

        defs.push(block_item("stone", blocks::STONE));
        defs.push(block_item("dirt", blocks::DIRT));
        defs.push(block_item("grass", blocks::GRASS));
        defs.push(block_item("sand", blocks::SAND));
        defs.push(block_item("log", blocks::LOG));
        defs.push(block_item("leaves", blocks::LEAVES));
        defs.push(block_item("planks", blocks::PLANKS));
        defs.push(block_item("crafting_table", blocks::CRAFTING_TABLE));
        defs.push(block_item("chest", blocks::CHEST));
        defs.push(material("stick"));
        defs.push(block_item("cobblestone", blocks::COBBLESTONE));
        defs.push(material("coal"));
        defs.push(material("iron_ore"));
        defs.push(material("iron_ingot"));
        defs.push(block_item("furnace", blocks::FURNACE));

        for (tier_name, tier, speed, durability) in [
            ("wooden", TIER_WOOD, 2.0, 60),
            ("stone", TIER_STONE, 4.0, 132),
            ("iron", TIER_IRON, 6.0, 251),
        ] {
            for (kind_name, kind) in [
                ("pickaxe", ToolKind::Pickaxe),
                ("axe", ToolKind::Axe),
                ("shovel", ToolKind::Shovel),
            ] {
                // Leaked so `ItemDef::name` can stay `&'static str` like the
                // rest of the catalog; the registry lives for the process.
                let name: &'static str =
                    Box::leak(format!("{tier_name}_{kind_name}").into_boxed_str());
                defs.push(tool_item(name, kind, tier, speed, durability));
            }
        }

        defs.push(block_item("torch", blocks::TORCH));
        for (name, block) in [
            ("demo_red_light", blocks::DEMO_RED_LIGHT),
            ("demo_green_light", blocks::DEMO_GREEN_LIGHT),
            ("demo_blue_light", blocks::DEMO_BLUE_LIGHT),
        ] {
            defs.push(ItemDef {
                demo_only: true,
                ..block_item(name, block)
            });
        }

        defs.push(block_item("farmland", blocks::FARMLAND));
        defs.push(block_item("wheat_seeds", blocks::WHEAT_YOUNG));
        defs.push(material("wheat"));
        defs.push(material("bread"));
        defs.push(material("toast"));
        for (name, block) in [
            ("miner", blocks::MINER),
            ("belt", blocks::BELT),
            ("powered_furnace", blocks::POWERED_FURNACE),
            ("factory_storage", blocks::FACTORY_STORAGE),
            ("generator", blocks::GENERATOR),
        ] {
            defs.push(block_item(name, block));
        }

        // Block -> drop. Grass yields dirt (the turf does not survive being
        // dug up), stone yields cobblestone, and ores yield their material;
        // water is not breakable and yields nothing.
        let mut drops = vec![None; blocks::GENERATOR.0 as usize + 1];
        let mut drop = |block: BlockId, item| drops[block.0 as usize] = Some(ItemStack::one(item));
        drop(blocks::STONE, items::COBBLESTONE);
        drop(blocks::DIRT, items::DIRT);
        drop(blocks::GRASS, items::DIRT);
        drop(blocks::FARMLAND, items::DIRT);
        drop(blocks::WHEAT_YOUNG, items::WHEAT_SEEDS);
        drop(blocks::WHEAT_MATURE, items::WHEAT);
        drop(blocks::MINER, items::MINER);
        drop(blocks::BELT, items::BELT);
        drop(blocks::POWERED_FURNACE, items::POWERED_FURNACE);
        drop(blocks::FACTORY_STORAGE, items::FACTORY_STORAGE);
        drop(blocks::GENERATOR, items::GENERATOR);
        drop(blocks::SAND, items::SAND);
        drop(blocks::LOG, items::LOG);
        drop(blocks::LEAVES, items::LEAVES);
        drop(blocks::PLANKS, items::PLANKS);
        drop(blocks::CRAFTING_TABLE, items::CRAFTING_TABLE);
        drop(blocks::CHEST, items::CHEST);
        drop(blocks::COBBLESTONE, items::COBBLESTONE);
        drop(blocks::COAL_ORE, items::COAL);
        drop(blocks::IRON_ORE, items::IRON_ORE);
        drop(blocks::FURNACE, items::FURNACE);
        drop(blocks::TORCH, items::TORCH);

        Self { defs, drops }
    }

    #[inline]
    pub fn get(&self, id: ItemId) -> &ItemDef {
        &self.defs[id.0 as usize]
    }

    /// `true` if `id` names a real item (index 0 does not).
    #[inline]
    pub fn is_valid(&self, id: ItemId) -> bool {
        id.0 != 0 && (id.0 as usize) < self.defs.len()
    }

    #[inline]
    pub fn max_stack(&self, id: ItemId) -> u32 {
        self.get(id).max_stack
    }

    /// What breaking `block` yields, if anything. Whether the player's tool
    /// is good enough to get it is [`crate::tool::can_harvest`]'s question.
    #[inline]
    pub fn drop_of(&self, block: BlockId) -> Option<ItemStack> {
        self.drops.get(block.0 as usize).copied().flatten()
    }

    /// The block `item` places, if it places one.
    #[inline]
    pub fn places(&self, item: ItemId) -> Option<BlockId> {
        if self.is_valid(item) && enabled(self.get(item), DEMO_LIGHTS_ENABLED) {
            self.get(item).places
        } else {
            None
        }
    }

    /// The tool `item` is, if it is one.
    #[inline]
    pub fn tool(&self, item: ItemId) -> Option<&ToolDef> {
        if self.is_valid(item) {
            self.get(item).tool.as_ref()
        } else {
            None
        }
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    /// Every enabled placeable item, in catalog order. Used to fill the
    /// creative inventory.
    pub fn placeable(&self) -> impl Iterator<Item = ItemId> + '_ {
        (1..self.defs.len() as u16)
            .map(ItemId)
            .filter(|&id| self.places(id).is_some())
    }
}

fn enabled(item: &ItemDef, demo_lights_enabled: bool) -> bool {
    !item.demo_only || demo_lights_enabled
}

impl Default for ItemRegistry {
    fn default() -> Self {
        Self::prototype()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_icons_match_item_ids_and_placeable_blocks() {
        let atlas: serde_json::Value =
            serde_json::from_str(include_str!("../../../assets/icons.json")).unwrap();
        assert_eq!(atlas["tile_size"], 32);
        assert_eq!(atlas["size"], serde_json::json!([256, 160]));
        let icons = atlas["items"].as_array().unwrap();
        let registry = ItemRegistry::prototype();
        assert_eq!(icons.len(), registry.len() - 1);
        for (index, icon) in icons.iter().enumerate() {
            let id = index + 1;
            let item = registry.get(ItemId(id as u16));
            assert_eq!(icon["id"], id);
            assert_eq!(icon["name"], item.name);
            assert_eq!(
                icon["placeable_block"],
                serde_json::json!(item.places.map(|block| block.0))
            );
            assert_eq!(
                icon["rect"],
                serde_json::json!([id % 8 * 32, id / 8 * 32, 32, 32])
            );
        }
    }

    /// As with `blocks`, the `items` constants are hand-written indices; a
    /// definition inserted in the middle would renumber everything after it.
    #[test]
    fn item_constants_match_their_definitions() {
        let reg = ItemRegistry::prototype();
        for (id, name) in [
            (items::STONE, "stone"),
            (items::STICK, "stick"),
            (items::COBBLESTONE, "cobblestone"),
            (items::COAL, "coal"),
            (items::IRON_ORE, "iron_ore"),
            (items::IRON_INGOT, "iron_ingot"),
            (items::FURNACE, "furnace"),
            (items::WOODEN_PICKAXE, "wooden_pickaxe"),
            (items::STONE_AXE, "stone_axe"),
            (items::IRON_SHOVEL, "iron_shovel"),
            (items::TORCH, "torch"),
            (items::DEMO_RED_LIGHT, "demo_red_light"),
            (items::DEMO_GREEN_LIGHT, "demo_green_light"),
            (items::DEMO_BLUE_LIGHT, "demo_blue_light"),
        ] {
            assert_eq!(reg.get(id).name, name, "item id {id:?}");
        }
    }

    #[test]
    fn tools_do_not_stack() {
        let reg = ItemRegistry::prototype();
        for id in [items::WOODEN_PICKAXE, items::STONE_SHOVEL, items::IRON_AXE] {
            assert_eq!(reg.max_stack(id), 1, "{:?} stacks", reg.get(id).name);
            assert!(reg.tool(id).is_some());
        }
        assert!(reg.tool(items::COAL).is_none());
    }

    #[test]
    fn demo_lights_are_separate_from_survival_acquisition_and_can_be_disabled() {
        let registry = ItemRegistry::prototype();
        let recipes = crate::RecipeRegistry::prototype();
        for id in [
            items::DEMO_RED_LIGHT,
            items::DEMO_GREEN_LIGHT,
            items::DEMO_BLUE_LIGHT,
        ] {
            let def = registry.get(id);
            assert!(def.demo_only);
            assert!(enabled(def, true));
            assert!(!enabled(def, false));
            assert_eq!(
                registry.placeable().any(|item| item == id),
                DEMO_LIGHTS_ENABLED
            );
            assert!(registry.drop_of(def.places.unwrap()).is_none());
            assert!(recipes.recipes().iter().all(|recipe| {
                recipe.output.item != id && recipe.inputs.iter().all(|input| input.item != id)
            }));
        }
        assert!(enabled(registry.get(items::STONE), false));
    }

    #[test]
    fn stone_drops_cobblestone_not_stone() {
        let reg = ItemRegistry::prototype();
        assert_eq!(
            reg.drop_of(blocks::STONE),
            Some(ItemStack::one(items::COBBLESTONE))
        );
    }

    #[test]
    fn worn_stacks_do_not_merge_with_fresh_ones() {
        let fresh = ItemStack::one(items::IRON_PICKAXE);
        let worn = fresh.with_damage(17);

        assert!(fresh.mergeable_with(fresh));
        assert!(!fresh.mergeable_with(worn));
    }
}
