//! Crafting recipes (design.md §7, roadmap M5).
//!
//! A recipe is declarative: inputs -> output. Matching is a pure function of
//! the grid contents, which makes it directly testable and -- the reason
//! this exists before the factory (roadmap M9) -- makes this table the same
//! data a machine node will consume. A machine is a recipe plus a rate.

use crate::item::{ItemId, ItemStack, items};

/// How a recipe's inputs are arranged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecipeInput {
    /// Position matters. `cells` is row-major, `width * height` long, and
    /// must be tight: no fully-empty leading/trailing row or column, so the
    /// pattern can be slid anywhere in a larger grid when matching.
    Shaped {
        width: usize,
        height: usize,
        cells: Vec<Option<ItemId>>,
    },
    /// Position does not matter; the grid must contain exactly this multiset
    /// of items (one each; duplicates are listed twice).
    Shapeless { items: Vec<ItemId> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recipe {
    pub input: RecipeInput,
    pub output: ItemStack,
}

pub struct RecipeRegistry {
    recipes: Vec<Recipe>,
}

impl RecipeRegistry {
    /// The M5 recipe set. Deliberately tiny, and deliberately arranged so
    /// that the crafting table is required for anything 3 wide -- that is
    /// what makes building one the first real goal of a new world.
    ///
    /// - 1 log -> 4 planks (shapeless)
    /// - 2 planks stacked vertically -> 4 sticks (shaped 1x2)
    /// - 4 planks in a 2x2 -> 1 crafting table
    /// - 8 planks ringing an empty centre (3x3) -> 1 chest
    pub fn prototype() -> Self {
        let p = Some(items::PLANKS);
        let recipes = vec![
            Recipe {
                input: RecipeInput::Shapeless {
                    items: vec![items::LOG],
                },
                output: ItemStack::new(items::PLANKS, 4),
            },
            Recipe {
                input: RecipeInput::Shaped {
                    width: 1,
                    height: 2,
                    cells: vec![p, p],
                },
                output: ItemStack::new(items::STICK, 4),
            },
            Recipe {
                input: RecipeInput::Shaped {
                    width: 2,
                    height: 2,
                    cells: vec![p, p, p, p],
                },
                output: ItemStack::one(items::CRAFTING_TABLE),
            },
            Recipe {
                input: RecipeInput::Shaped {
                    width: 3,
                    height: 3,
                    cells: vec![p, p, p, p, None, p, p, p, p],
                },
                output: ItemStack::one(items::CHEST),
            },
        ];
        Self { recipes }
    }

    pub fn recipes(&self) -> &[Recipe] {
        &self.recipes
    }

    /// Finds the recipe a crafting grid currently satisfies, if any.
    ///
    /// `grid` is row-major and `size * size` long (`size` is 2 for the
    /// inventory's hand-crafting square, 3 at a crafting table). Counts in
    /// the grid are ignored beyond "present": crafting consumes exactly one
    /// per occupied cell.
    ///
    /// Shaped matching slides the pattern over every offset that fits, so a
    /// 2x2 recipe is craftable in any corner of a 3x3 grid, and requires
    /// every cell outside the pattern to be empty.
    pub fn find(&self, grid: &[Option<ItemStack>], size: usize) -> Option<&Recipe> {
        if size == 0 || grid.len() < size * size {
            return None;
        }
        self.recipes
            .iter()
            .find(|recipe| matches(&recipe.input, grid, size))
    }
}

fn matches(input: &RecipeInput, grid: &[Option<ItemStack>], size: usize) -> bool {
    match input {
        RecipeInput::Shapeless { items } => {
            let mut present: Vec<ItemId> = grid[..size * size]
                .iter()
                .flatten()
                .map(|stack| stack.item)
                .collect();
            if present.len() != items.len() {
                return false;
            }
            for want in items {
                match present.iter().position(|have| have == want) {
                    Some(at) => {
                        present.swap_remove(at);
                    }
                    None => return false,
                }
            }
            true
        }
        RecipeInput::Shaped {
            width,
            height,
            cells,
        } => {
            if *width > size || *height > size || *width == 0 || *height == 0 {
                return false;
            }
            (0..=size - height)
                .flat_map(|oy| (0..=size - width).map(move |ox| (ox, oy)))
                .any(|(ox, oy)| shaped_matches_at(cells, *width, *height, grid, size, ox, oy))
        }
    }
}

/// Whether the pattern, placed with its top-left at `(ox, oy)`, accounts for
/// exactly the occupied cells of the grid: every covered cell must hold the
/// pattern's item, and every cell outside it must be empty.
fn shaped_matches_at(
    cells: &[Option<ItemId>],
    width: usize,
    height: usize,
    grid: &[Option<ItemStack>],
    size: usize,
    ox: usize,
    oy: usize,
) -> bool {
    for y in 0..size {
        for x in 0..size {
            let want = if x >= ox && x < ox + width && y >= oy && y < oy + height {
                cells[(y - oy) * width + (x - ox)]
            } else {
                None
            };
            let have = grid[y * size + x].map(|stack| stack.item);
            if want != have {
                return false;
            }
        }
    }
    true
}

impl Default for RecipeRegistry {
    fn default() -> Self {
        Self::prototype()
    }
}

/// Consumes the inputs of one craft: decrements every non-empty cell by one,
/// clearing cells that reach zero. Correct for every recipe shape because a
/// recipe uses exactly one item from each cell it covers, and a match
/// guarantees no other cell is occupied.
pub fn consume_one_craft(grid: &mut [Option<ItemStack>]) {
    for slot in grid.iter_mut() {
        let Some(stack) = slot else { continue };
        if stack.count <= 1 {
            *slot = None;
        } else {
            stack.count -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `size * size` grid from item ids, `None` for empty cells.
    fn grid(size: usize, cells: &[Option<ItemId>]) -> Vec<Option<ItemStack>> {
        assert_eq!(cells.len(), size * size);
        cells.iter().map(|c| c.map(ItemStack::one)).collect()
    }

    const P: Option<ItemId> = Some(items::PLANKS);
    const X: Option<ItemId> = None;

    #[test]
    fn shapeless_log_makes_planks_anywhere_in_the_grid() {
        let reg = RecipeRegistry::prototype();
        for at in 0..4 {
            let mut cells = [X; 4];
            cells[at] = Some(items::LOG);
            let found = reg.find(&grid(2, &cells), 2).expect("log should craft");
            assert_eq!(found.output, ItemStack::new(items::PLANKS, 4));
        }
    }

    #[test]
    fn shaped_pattern_matches_at_any_offset() {
        let reg = RecipeRegistry::prototype();
        // 1x2 sticks recipe, placed in the right-hand column of a 2x2 grid.
        let found = reg.find(&grid(2, &[X, P, X, P]), 2).expect("sticks");
        assert_eq!(found.output, ItemStack::new(items::STICK, 4));
    }

    #[test]
    fn shaped_pattern_rejects_extra_items_outside_it() {
        let reg = RecipeRegistry::prototype();
        // A 2x2 planks square plus a stray plank is not the table recipe...
        let five = grid(3, &[P, P, X, P, P, X, P, X, X]);
        assert!(reg.find(&five, 3).is_none());
    }

    #[test]
    fn two_by_two_recipe_works_in_a_three_by_three_grid() {
        let reg = RecipeRegistry::prototype();
        let corner = grid(3, &[X, X, X, X, P, P, X, P, P]);
        let found = reg.find(&corner, 3).expect("crafting table");
        assert_eq!(found.output, ItemStack::one(items::CRAFTING_TABLE));
    }

    #[test]
    fn three_wide_recipes_need_a_crafting_table() {
        let reg = RecipeRegistry::prototype();
        let ring = grid(3, &[P, P, P, P, X, P, P, P, P]);
        assert_eq!(
            reg.find(&ring, 3).map(|r| r.output),
            Some(ItemStack::one(items::CHEST))
        );
        // The same recipe cannot be reached from the 2x2 hand-crafting grid.
        assert!(
            reg.find(&grid(2, &[P, P, P, P]), 2).map(|r| r.output)
                != Some(ItemStack::one(items::CHEST))
        );
    }

    #[test]
    fn chest_requires_the_centre_to_be_empty() {
        let reg = RecipeRegistry::prototype();
        let filled = grid(3, &[P, P, P, P, P, P, P, P, P]);
        assert!(reg.find(&filled, 3).is_none());
    }

    #[test]
    fn an_empty_grid_matches_nothing() {
        let reg = RecipeRegistry::prototype();
        assert!(reg.find(&grid(3, &[X; 9]), 3).is_none());
    }

    #[test]
    fn crafting_consumes_exactly_one_per_occupied_cell() {
        let mut cells: Vec<Option<ItemStack>> = vec![
            Some(ItemStack::new(items::PLANKS, 3)),
            None,
            Some(ItemStack::one(items::PLANKS)),
            None,
        ];

        consume_one_craft(&mut cells);

        assert_eq!(cells[0], Some(ItemStack::new(items::PLANKS, 2)));
        assert_eq!(cells[1], None);
        assert_eq!(cells[2], None, "a spent cell kept a zero-count stack");
    }

    #[test]
    fn counts_in_the_grid_do_not_affect_matching() {
        let reg = RecipeRegistry::prototype();
        let mut stacked = grid(2, &[P, P, P, P]);
        for slot in stacked.iter_mut().flatten() {
            slot.count = 40;
        }
        assert_eq!(
            reg.find(&stacked, 2).map(|r| r.output),
            Some(ItemStack::one(items::CRAFTING_TABLE))
        );
    }
}
