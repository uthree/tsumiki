//! Items and the item registry (design.md §7, roadmap M5).
//!
//! An item is not a block. [`BlockId`] names something that occupies a cell
//! in the world; [`ItemId`] names something that occupies an inventory slot.
//! They are related only by two explicit mappings kept here -- what a block
//! drops when broken, and what block an item places -- and plenty of items
//! (sticks now, ingots and tools in M6) have neither.

use serde::{Deserialize, Serialize};

use crate::block::{BlockId, blocks};

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
}

impl ItemStack {
    pub const fn new(item: ItemId, count: u32) -> Self {
        Self { item, count }
    }

    pub const fn one(item: ItemId) -> Self {
        Self { item, count: 1 }
    }
}

/// Static definition of an item type.
#[derive(Clone, Debug)]
pub struct ItemDef {
    pub name: &'static str,
    /// Maximum count in one slot. Tools (M6) will use 1.
    pub max_stack: u32,
    /// Block this item places when used against a surface, if any.
    pub places: Option<BlockId>,
    /// Placeholder icon color (sRGB), until item textures exist. The client
    /// draws items as colored squares with a count label.
    pub color: [u8; 3],
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
}

/// Default stack size. Kept as a constant because the whole catalog shares
/// it today; [`ItemDef::max_stack`] exists so M6's tools can differ.
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
            color: [0, 0, 0],
        }];

        let block_item = |name, block: BlockId, color| ItemDef {
            name,
            max_stack: DEFAULT_MAX_STACK,
            places: Some(block),
            color,
        };
        defs.push(block_item("stone", blocks::STONE, [152, 150, 166]));
        defs.push(block_item("dirt", blocks::DIRT, [152, 106, 74]));
        defs.push(block_item("grass", blocks::GRASS, [110, 198, 92]));
        defs.push(block_item("sand", blocks::SAND, [236, 214, 146]));
        defs.push(block_item("log", blocks::LOG, [140, 106, 70]));
        defs.push(block_item("leaves", blocks::LEAVES, [78, 166, 70]));
        defs.push(block_item("planks", blocks::PLANKS, [210, 166, 106]));
        defs.push(block_item(
            "crafting_table",
            blocks::CRAFTING_TABLE,
            [188, 138, 84],
        ));
        defs.push(block_item("chest", blocks::CHEST, [204, 146, 70]));
        defs.push(ItemDef {
            name: "stick",
            max_stack: DEFAULT_MAX_STACK,
            places: None,
            color: [178, 134, 88],
        });

        // Block -> drop. Grass yields dirt (the turf does not survive being
        // dug up); water is not breakable and yields nothing.
        let mut drops = vec![None; blocks::CHEST.0 as usize + 1];
        drops[blocks::STONE.0 as usize] = Some(ItemStack::one(items::STONE));
        drops[blocks::DIRT.0 as usize] = Some(ItemStack::one(items::DIRT));
        drops[blocks::GRASS.0 as usize] = Some(ItemStack::one(items::DIRT));
        drops[blocks::SAND.0 as usize] = Some(ItemStack::one(items::SAND));
        drops[blocks::LOG.0 as usize] = Some(ItemStack::one(items::LOG));
        drops[blocks::LEAVES.0 as usize] = Some(ItemStack::one(items::LEAVES));
        drops[blocks::PLANKS.0 as usize] = Some(ItemStack::one(items::PLANKS));
        drops[blocks::CRAFTING_TABLE.0 as usize] = Some(ItemStack::one(items::CRAFTING_TABLE));
        drops[blocks::CHEST.0 as usize] = Some(ItemStack::one(items::CHEST));

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

    /// What breaking `block` yields, if anything.
    #[inline]
    pub fn drop_of(&self, block: BlockId) -> Option<ItemStack> {
        self.drops.get(block.0 as usize).copied().flatten()
    }

    /// The block `item` places, if it places one.
    #[inline]
    pub fn places(&self, item: ItemId) -> Option<BlockId> {
        if self.is_valid(item) {
            self.get(item).places
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

    /// Every placeable item, in catalog order. Used to fill the creative
    /// hotbar.
    pub fn placeable(&self) -> impl Iterator<Item = ItemId> + '_ {
        (1..self.defs.len() as u16)
            .map(ItemId)
            .filter(|&id| self.get(id).places.is_some())
    }
}

impl Default for ItemRegistry {
    fn default() -> Self {
        Self::prototype()
    }
}
