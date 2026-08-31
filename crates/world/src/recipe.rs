//! Crafting recipes (design.md §7, roadmap M5).
//!
//! A recipe is declarative: a set of input stacks -> one output stack. There
//! is deliberately no spatial pattern to arrange. Players pick what to make
//! from a list, so nothing has to be memorised or looked up outside the
//! game -- the cost of a crafting grid is paid by every new player, forever,
//! and buys only the ritual of arranging squares.
//!
//! This also makes the table the same shape the factory graph (roadmap M9)
//! consumes: a machine node is one of these plus a rate (design.md §4.3).

use crate::inventory::Inventory;
use crate::item::{ItemRegistry, ItemStack, items};

/// Index into [`RecipeRegistry`]'s list. Sent over the network, so a client
/// asking to craft names one of these rather than describing a recipe.
pub type RecipeId = u16;

/// A block that must be open to reach a recipe. Recipes with `None` are
/// craftable anywhere.
///
/// This is what keeps the crafting table meaningful now that recipes are a
/// list: it is not a place to arrange items, it is what unlocks the rest of
/// the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CraftingStation {
    CraftingTable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recipe {
    /// One entry per distinct input item, with the count consumed per craft.
    pub inputs: Vec<ItemStack>,
    pub output: ItemStack,
    /// Where this can be crafted; `None` means by hand, anywhere.
    pub station: Option<CraftingStation>,
}

pub struct RecipeRegistry {
    recipes: Vec<Recipe>,
}

impl RecipeRegistry {
    /// The M5 recipe set. Small on purpose, and arranged so the crafting
    /// table is what unlocks anything beyond the basics -- building one is
    /// still the first real goal of a new world.
    pub fn prototype() -> Self {
        let recipes = vec![
            Recipe {
                inputs: vec![ItemStack::one(items::LOG)],
                output: ItemStack::new(items::PLANKS, 4),
                station: None,
            },
            Recipe {
                inputs: vec![ItemStack::new(items::PLANKS, 2)],
                output: ItemStack::new(items::STICK, 4),
                station: None,
            },
            Recipe {
                inputs: vec![ItemStack::new(items::PLANKS, 4)],
                output: ItemStack::one(items::CRAFTING_TABLE),
                station: None,
            },
            Recipe {
                inputs: vec![ItemStack::new(items::PLANKS, 8)],
                output: ItemStack::one(items::CHEST),
                station: Some(CraftingStation::CraftingTable),
            },
        ];
        Self { recipes }
    }

    pub fn recipes(&self) -> &[Recipe] {
        &self.recipes
    }

    pub fn get(&self, id: RecipeId) -> Option<&Recipe> {
        self.recipes.get(id as usize)
    }

    /// Every recipe reachable with `station` open, in catalog order, paired
    /// with its id. Hand recipes stay available at a station, so opening a
    /// crafting table only ever adds to the list.
    pub fn available(
        &self,
        station: Option<CraftingStation>,
    ) -> impl Iterator<Item = (RecipeId, &Recipe)> {
        self.recipes
            .iter()
            .enumerate()
            .filter(move |(_, recipe)| recipe.station.is_none() || recipe.station == station)
            .map(|(index, recipe)| (index as RecipeId, recipe))
    }

    /// `true` if `station` reaches `id` at all, regardless of materials. The
    /// server checks this before touching an inventory: a client may name
    /// any recipe id it likes.
    pub fn is_reachable(&self, id: RecipeId, station: Option<CraftingStation>) -> bool {
        self.get(id)
            .is_some_and(|recipe| recipe.station.is_none() || recipe.station == station)
    }
}

impl Default for RecipeRegistry {
    fn default() -> Self {
        Self::prototype()
    }
}

/// How many times `recipe` could be crafted from `inv` right now, ignoring
/// whether the output would fit.
pub fn craftable_times(recipe: &Recipe, inv: &Inventory) -> u32 {
    recipe
        .inputs
        .iter()
        // A zero-count input would be a malformed recipe; treat it as no
        // constraint rather than dividing by zero.
        .map(|input| {
            inv.count_of(input.item)
                .checked_div(input.count)
                .unwrap_or(u32::MAX)
        })
        .min()
        .unwrap_or(0)
}

pub fn can_craft(recipe: &Recipe, inv: &Inventory) -> bool {
    craftable_times(recipe, inv) > 0
}

/// Crafts `recipe` up to `times` times out of `inv`, consuming inputs and
/// inserting outputs.
///
/// Returns `(times actually crafted, output that did not fit)`. Overflow is
/// handed back rather than dropped here, because only the caller knows where
/// in the world it should land.
pub fn craft(
    recipe: &Recipe,
    times: u32,
    inv: &mut Inventory,
    reg: &ItemRegistry,
) -> (u32, Vec<ItemStack>) {
    let runs = times.min(craftable_times(recipe, inv));
    if runs == 0 {
        return (0, Vec::new());
    }

    for input in &recipe.inputs {
        let consumed = input.count.saturating_mul(runs);
        debug_assert!(
            inv.count_of(input.item) >= consumed,
            "craftable_times over-reported"
        );
        inv.remove(input.item, consumed);
    }

    // Insert a stack at a time so a nearly-full inventory still takes what it
    // can, rather than an all-or-nothing insert of the whole run.
    let mut overflow = Vec::new();
    let mut produced = recipe.output.count.saturating_mul(runs);
    let max_stack = reg.max_stack(recipe.output.item).max(1);
    while produced > 0 {
        let chunk = produced.min(max_stack);
        produced -= chunk;
        if let Some(left) = inv.insert(ItemStack::new(recipe.output.item, chunk), reg) {
            overflow.push(left);
        }
    }
    (runs, overflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::MAIN_INVENTORY_SIZE;
    use crate::item::DEFAULT_MAX_STACK;

    fn inv_with(stacks: &[ItemStack]) -> Inventory {
        let reg = ItemRegistry::prototype();
        let mut inv = Inventory::new(MAIN_INVENTORY_SIZE);
        for &stack in stacks {
            inv.insert(stack, &reg);
        }
        inv
    }

    #[test]
    fn hand_recipes_are_available_everywhere() {
        let reg = RecipeRegistry::prototype();
        let by_hand: Vec<_> = reg.available(None).map(|(id, _)| id).collect();
        let at_table: Vec<_> = reg
            .available(Some(CraftingStation::CraftingTable))
            .map(|(id, _)| id)
            .collect();

        assert!(by_hand.iter().all(|id| at_table.contains(id)));
        assert!(
            at_table.len() > by_hand.len(),
            "a crafting table must unlock something"
        );
    }

    #[test]
    fn the_chest_needs_a_crafting_table() {
        let reg = RecipeRegistry::prototype();
        let chest = reg
            .recipes()
            .iter()
            .position(|r| r.output.item == items::CHEST)
            .expect("chest recipe") as RecipeId;

        assert!(!reg.is_reachable(chest, None));
        assert!(reg.is_reachable(chest, Some(CraftingStation::CraftingTable)));
    }

    #[test]
    fn unknown_recipe_ids_are_not_reachable() {
        let reg = RecipeRegistry::prototype();
        assert!(!reg.is_reachable(9999, Some(CraftingStation::CraftingTable)));
    }

    #[test]
    fn craftable_times_is_limited_by_the_scarcest_input() {
        let reg = RecipeRegistry::prototype();
        let table = &reg.recipes()[2];
        assert_eq!(table.output.item, items::CRAFTING_TABLE);

        assert_eq!(craftable_times(table, &inv_with(&[])), 0);
        assert_eq!(
            craftable_times(table, &inv_with(&[ItemStack::new(items::PLANKS, 3)])),
            0
        );
        assert_eq!(
            craftable_times(table, &inv_with(&[ItemStack::new(items::PLANKS, 9)])),
            2
        );
    }

    #[test]
    fn crafting_consumes_inputs_and_yields_output() {
        let items_reg = ItemRegistry::prototype();
        let reg = RecipeRegistry::prototype();
        let planks = &reg.recipes()[0];
        let mut inv = inv_with(&[ItemStack::new(items::LOG, 3)]);

        let (runs, overflow) = craft(planks, 1, &mut inv, &items_reg);

        assert_eq!(runs, 1);
        assert!(overflow.is_empty());
        assert_eq!(inv.count_of(items::LOG), 2);
        assert_eq!(inv.count_of(items::PLANKS), 4);
    }

    #[test]
    fn crafting_more_than_the_materials_allow_crafts_what_it_can() {
        let items_reg = ItemRegistry::prototype();
        let reg = RecipeRegistry::prototype();
        let sticks = &reg.recipes()[1];
        let mut inv = inv_with(&[ItemStack::new(items::PLANKS, 5)]);

        let (runs, _) = craft(sticks, 99, &mut inv, &items_reg);

        assert_eq!(runs, 2, "2 planks per craft, 5 available");
        assert_eq!(inv.count_of(items::PLANKS), 1);
        assert_eq!(inv.count_of(items::STICK), 8);
    }

    #[test]
    fn crafting_with_no_materials_changes_nothing() {
        let items_reg = ItemRegistry::prototype();
        let reg = RecipeRegistry::prototype();
        let chest = &reg.recipes()[3];
        let mut inv = inv_with(&[ItemStack::new(items::PLANKS, 2)]);

        let (runs, overflow) = craft(chest, 1, &mut inv, &items_reg);

        assert_eq!(runs, 0);
        assert!(overflow.is_empty());
        assert_eq!(inv.count_of(items::PLANKS), 2, "inputs were eaten anyway");
    }

    #[test]
    fn output_that_does_not_fit_comes_back_as_overflow() {
        let items_reg = ItemRegistry::prototype();
        let reg = RecipeRegistry::prototype();
        let planks = &reg.recipes()[0];

        // A single slot, already holding a full stack of the input: the
        // output has nowhere to land.
        let mut inv = Inventory::new(1);
        inv.set_slot(0, Some(ItemStack::new(items::LOG, DEFAULT_MAX_STACK)));

        let (runs, overflow) = craft(planks, 1, &mut inv, &items_reg);

        assert_eq!(runs, 1);
        assert_eq!(overflow, vec![ItemStack::new(items::PLANKS, 4)]);
    }
}
