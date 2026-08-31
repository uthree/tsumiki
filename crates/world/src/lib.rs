//! World data shared by server and client: block definitions, chunk storage,
//! and world generation. See `doc/design.md` §2.
//!
//! This crate must stay free of rendering and networking dependencies.

// Index-based loops over fixed voxel grids are the local idiom; iterator
// rewrites obscure the coordinate math.
#![allow(clippy::needless_range_loop)]

pub mod block;
pub mod chunk;
pub mod inventory;
pub mod item;
pub mod lod;
pub mod physics;
pub mod raycast;
pub mod recipe;
pub mod worldgen;

pub use block::{BlockDef, BlockId, BlockInteraction, BlockRegistry, blocks};
pub use chunk::{CHUNK_SIZE, Chunk};
pub use inventory::{HOTBAR_SIZE, Inventory, MAIN_INVENTORY_SIZE};
pub use item::{ItemDef, ItemId, ItemRegistry, ItemStack, items};
pub use recipe::{Recipe, RecipeInput, RecipeRegistry};
pub use worldgen::WorldGenerator;

use bevy_math::IVec3;

/// Vertical world bounds, in chunks. Valid chunk Y coordinates lie in
/// `0..WORLD_HEIGHT_CHUNKS`; there are no chunks above or below.
pub const WORLD_HEIGHT_CHUNKS: i32 = 4;

/// World height in blocks.
pub const WORLD_HEIGHT_BLOCKS: i32 = WORLD_HEIGHT_CHUNKS * CHUNK_SIZE as i32;

/// Splits a world-space block position into `(chunk position, local position)`.
///
/// Uses euclidean division so negative coordinates map correctly
/// (e.g. block x = -1 lives in chunk x = -1 at local x = 31).
pub fn split_block_pos(pos: IVec3) -> (IVec3, IVec3) {
    let size = CHUNK_SIZE as i32;
    let chunk = IVec3::new(
        pos.x.div_euclid(size),
        pos.y.div_euclid(size),
        pos.z.div_euclid(size),
    );
    let local = IVec3::new(
        pos.x.rem_euclid(size),
        pos.y.rem_euclid(size),
        pos.z.rem_euclid(size),
    );
    (chunk, local)
}
