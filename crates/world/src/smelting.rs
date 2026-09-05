//! Smelting: the furnace's recipe table and what burns (roadmap M6).
//!
//! Kept separate from [`crate::recipe`] because a furnace is not a crafting
//! station -- it consumes fuel and takes time, so its recipes carry a
//! duration and its inputs are a single item rather than a set.
//!
//! This is deliberately the shape the factory graph (design.md §4.3) wants:
//! an input rate, an output rate, and a fuel rate. The furnace is the bridge
//! block -- the first machine a player meets, and the last one that will be
//! simulated by ticking rather than by the graph.

use crate::item::{ItemId, ItemStack, items};

/// A furnace's slots: input, fuel, output. Fixed layout, so the UI and the
/// server agree without negotiating.
pub const FURNACE_SIZE: usize = 3;
pub const FURNACE_INPUT: usize = 0;
pub const FURNACE_FUEL: usize = 1;
pub const FURNACE_OUTPUT: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmeltRecipe {
    pub input: ItemId,
    pub output: ItemStack,
    /// Seconds of burning to convert one input.
    pub secs_per_item: u32,
}

pub struct SmeltingRegistry {
    recipes: Vec<SmeltRecipe>,
}

impl SmeltingRegistry {
    pub fn prototype() -> Self {
        Self {
            recipes: vec![
                SmeltRecipe {
                    input: items::IRON_ORE,
                    output: ItemStack::one(items::IRON_INGOT),
                    secs_per_item: 10,
                },
                SmeltRecipe {
                    input: items::BREAD,
                    output: ItemStack::one(items::TOAST),
                    secs_per_item: 5,
                },
            ],
        }
    }

    pub fn recipes(&self) -> &[SmeltRecipe] {
        &self.recipes
    }

    /// What `input` smelts into, if anything.
    pub fn find(&self, input: ItemId) -> Option<&SmeltRecipe> {
        self.recipes.iter().find(|recipe| recipe.input == input)
    }
}

impl Default for SmeltingRegistry {
    fn default() -> Self {
        Self::prototype()
    }
}

/// Seconds of burn time one unit of `item` provides, or `None` if it does
/// not burn.
///
/// Coal is worth far more than wood per item, which is the whole reason to
/// go looking for it -- the first time scarcity of a *material* rather than
/// of time pushes a player somewhere new.
pub fn fuel_secs(item: ItemId) -> Option<u32> {
    Some(match item {
        items::COAL => 80,
        items::LOG => 15,
        items::PLANKS => 15,
        items::CRAFTING_TABLE => 15,
        items::CHEST => 15,
        items::STICK => 5,
        _ => return None,
    })
}

/// Whether `item` can go in a furnace's fuel slot at all.
pub fn is_fuel(item: ItemId) -> bool {
    fuel_secs(item).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iron_ore_smelts_into_an_ingot() {
        let reg = SmeltingRegistry::prototype();
        let recipe = reg.find(items::IRON_ORE).expect("iron ore should smelt");

        assert_eq!(recipe.output, ItemStack::one(items::IRON_INGOT));
        assert!(recipe.secs_per_item > 0);
    }

    #[test]
    fn things_that_do_not_smelt_have_no_recipe() {
        let reg = SmeltingRegistry::prototype();
        for item in [items::DIRT, items::COAL, items::IRON_INGOT] {
            assert!(reg.find(item).is_none(), "{item:?} should not smelt");
        }
    }

    #[test]
    fn coal_burns_far_longer_than_wood() {
        let coal = fuel_secs(items::COAL).expect("coal burns");
        let planks = fuel_secs(items::PLANKS).expect("planks burn");

        assert!(coal >= planks * 4, "coal should be worth seeking out");
    }

    #[test]
    fn stone_and_metal_do_not_burn() {
        for item in [items::COBBLESTONE, items::IRON_INGOT, items::IRON_ORE] {
            assert!(!is_fuel(item), "{item:?} should not burn");
        }
    }

    #[test]
    fn every_fuel_burns_for_a_positive_time() {
        for item in [
            items::COAL,
            items::LOG,
            items::PLANKS,
            items::STICK,
            items::CHEST,
        ] {
            assert!(fuel_secs(item).is_some_and(|secs| secs > 0), "{item:?}");
        }
    }
}
