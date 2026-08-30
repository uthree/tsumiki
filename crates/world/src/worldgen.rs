//! Deterministic world generation.
//!
//! Prototype terrain recipe:
//! - fBm (Perlin) heightmap: height ≈ `BASE_HEIGHT` ± `HEIGHT_AMPLITUDE`,
//!   clamped to `1..WORLD_HEIGHT_BLOCKS - 8`.
//! - Below the surface: stone, topped by 3 dirt; the surface block is grass,
//!   or sand when the surface is within ±2 of `SEA_LEVEL`.
//! - Columns whose surface lies below `SEA_LEVEL` are flooded with water up
//!   to `SEA_LEVEL`.
//! - Sparse trees (trunk of logs + leaf blob) on grass, placed via a
//!   deterministic per-column hash. Trees are only placed when they fit
//!   entirely inside one chunk (local x/z in `2..30`), so generation never
//!   needs neighbor chunks.
//!
//! Generation must be deterministic: same seed + same chunk position =>
//! identical chunk, on every platform.

use crate::block::{BlockId, blocks};
use crate::chunk::{CHUNK_SIZE, Chunk};
use bevy_math::{IVec3, UVec3};
use noise::{Fbm, MultiFractal, NoiseFn, Perlin};

/// Sea level, in world-space block Y.
pub const SEA_LEVEL: i32 = 36;

/// Average terrain height, in world-space block Y.
pub const BASE_HEIGHT: f64 = 40.0;

/// Maximum deviation of terrain height from [`BASE_HEIGHT`].
pub const HEIGHT_AMPLITUDE: f64 = 24.0;

/// Number of dirt layers below the surface block, above stone.
const DIRT_DEPTH: i32 = 3;

/// One in `TREE_CHANCE` eligible columns grows a tree.
const TREE_CHANCE: u64 = 40;

/// Horizontal margin from a chunk edge a tree's trunk must keep, so its
/// canopy (radius 2) never reaches into a neighboring chunk.
const TREE_MARGIN: usize = 2;

/// SplitMix64: a small, fast, well-mixed hash used to turn `(seed, x, z)`
/// into a deterministic per-column value, and to derive the `u32` noise seed
/// from the generator's `u64` seed.
fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// Deterministic per-column hash of `(seed, x, z)`.
fn column_hash(seed: u64, x: i32, z: i32) -> u64 {
    let h = splitmix64(seed ^ (x as i64 as u64));
    splitmix64(h ^ (z as i64 as u64))
}

/// The block at world-space `wy`, given this column's terrain `surface`
/// height. Pure function of the layering recipe: stone, then `DIRT_DEPTH`
/// dirt, then the surface block; water fills from just above the surface up
/// to `SEA_LEVEL` for columns below sea level.
fn column_block(surface: i32, wy: i32) -> BlockId {
    if wy > surface {
        if wy <= SEA_LEVEL {
            blocks::WATER
        } else {
            blocks::AIR
        }
    } else if wy == surface {
        surface_block(surface)
    } else if wy > surface - DIRT_DEPTH - 1 {
        blocks::DIRT
    } else {
        blocks::STONE
    }
}

/// The exposed surface block for a column with the given terrain height.
fn surface_block(surface: i32) -> BlockId {
    if (surface - SEA_LEVEL).abs() <= 2 {
        blocks::SAND
    } else {
        blocks::GRASS
    }
}

/// Converts a world-space Y into a chunk-local Y, or `None` if it falls
/// outside the chunk starting at `base_y`.
fn local_y(world_y: i32, base_y: i32) -> Option<u32> {
    let ly = world_y - base_y;
    if (0..CHUNK_SIZE as i32).contains(&ly) {
        Some(ly as u32)
    } else {
        None
    }
}

/// Deterministic chunk generator. Cheap to clone/share; holds no world state.
#[derive(Clone)]
pub struct WorldGenerator {
    seed: u64,
    heightmap: Fbm<Perlin>,
}

impl WorldGenerator {
    pub fn new(seed: u64) -> Self {
        // Perlin/Fbm take a u32 seed; derive one from the u64 seed instead of
        // truncating, so nearby u64 seeds don't collide or correlate.
        let noise_seed = splitmix64(seed) as u32;
        let heightmap = Fbm::<Perlin>::new(noise_seed)
            .set_octaves(4)
            .set_frequency(0.01)
            .set_persistence(0.5);
        Self { seed, heightmap }
    }

    /// Terrain height for world-space column `(x, z)`.
    fn height_at(&self, x: i32, z: i32) -> i32 {
        let n = self.heightmap.get([x as f64, z as f64]);
        let h = (BASE_HEIGHT + n * HEIGHT_AMPLITUDE).round() as i32;
        h.clamp(1, crate::WORLD_HEIGHT_BLOCKS - 9)
    }

    /// Generates the chunk at `chunk_pos` (chunk coordinates;
    /// `chunk_pos.y` in `0..WORLD_HEIGHT_CHUNKS`).
    pub fn generate_chunk(&self, chunk_pos: IVec3) -> Chunk {
        let base = IVec3::new(
            chunk_pos.x * CHUNK_SIZE as i32,
            chunk_pos.y * CHUNK_SIZE as i32,
            chunk_pos.z * CHUNK_SIZE as i32,
        );

        let mut chunk = Chunk::filled(blocks::AIR);

        // Heightmap computed once per column, not per block.
        let mut heights = [[0i32; CHUNK_SIZE]; CHUNK_SIZE];
        for (lx, row) in heights.iter_mut().enumerate() {
            for (lz, h) in row.iter_mut().enumerate() {
                *h = self.height_at(base.x + lx as i32, base.z + lz as i32);
            }
        }

        for lx in 0..CHUNK_SIZE {
            for lz in 0..CHUNK_SIZE {
                let surface = heights[lx][lz];
                for ly in 0..CHUNK_SIZE {
                    let wy = base.y + ly as i32;
                    let block = column_block(surface, wy);
                    if !block.is_air() {
                        chunk.set(UVec3::new(lx as u32, ly as u32, lz as u32), block);
                    }
                }
            }
        }

        // Trees: a second pass over fully-set terrain, so canopy overlap
        // between neighboring tree columns never depends on iteration order.
        for lx in TREE_MARGIN..CHUNK_SIZE - TREE_MARGIN {
            for lz in TREE_MARGIN..CHUNK_SIZE - TREE_MARGIN {
                let surface = heights[lx][lz];
                if surface_block(surface) != blocks::GRASS || surface <= SEA_LEVEL {
                    continue;
                }
                let wx = base.x + lx as i32;
                let wz = base.z + lz as i32;
                let hash = column_hash(self.seed, wx, wz);
                if hash % TREE_CHANCE == 0 {
                    Self::place_tree(&mut chunk, base.y, lx, lz, surface, hash);
                }
            }
        }

        chunk
    }

    /// Places a trunk of logs starting one block above `surface`, plus a
    /// simple leaf blob around its top. Everything is clipped to the current
    /// chunk's vertical range, and leaves never overwrite non-air blocks.
    fn place_tree(chunk: &mut Chunk, base_y: i32, lx: usize, lz: usize, surface: i32, hash: u64) {
        let trunk_height = 4 + (hash % 2) as i32;
        let top_y = surface + trunk_height;

        for i in 1..=trunk_height {
            if let Some(ly) = local_y(surface + i, base_y) {
                chunk.set(UVec3::new(lx as u32, ly, lz as u32), blocks::LOG);
            }
        }

        for dy in -1..=1i32 {
            let Some(ly) = local_y(top_y + dy, base_y) else {
                continue;
            };
            for dx in -2..=2i32 {
                for dz in -2..=2i32 {
                    // Round off the far corners of the wide layers, and keep
                    // the top cap layer narrow, for a simple round-ish blob.
                    if dy < 1 && dx.abs() == 2 && dz.abs() == 2 {
                        continue;
                    }
                    if dy == 1 && (dx.abs() > 1 || dz.abs() > 1) {
                        continue;
                    }
                    // In-bounds by construction: lx/lz keep TREE_MARGIN (2)
                    // from the chunk edge, matching the canopy radius.
                    let local = UVec3::new((lx as i32 + dx) as u32, ly, (lz as i32 + dz) as u32);
                    if chunk.get(local).is_air() {
                        chunk.set(local, blocks::LEAVES);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WORLD_HEIGHT_CHUNKS;

    fn all_blocks(chunk: &Chunk) -> Vec<BlockId> {
        let mut out = Vec::with_capacity(CHUNK_SIZE.pow(3));
        for y in 0..CHUNK_SIZE {
            for z in 0..CHUNK_SIZE {
                for x in 0..CHUNK_SIZE {
                    out.push(chunk.get(UVec3::new(x as u32, y as u32, z as u32)));
                }
            }
        }
        out
    }

    #[test]
    fn same_seed_and_pos_is_deterministic() {
        let world_gen = WorldGenerator::new(999);
        let a = world_gen.generate_chunk(IVec3::new(2, 1, -3));
        let b = world_gen.generate_chunk(IVec3::new(2, 1, -3));
        assert_eq!(all_blocks(&a), all_blocks(&b));
    }

    #[test]
    fn different_seeds_produce_different_terrain() {
        let a = WorldGenerator::new(1).generate_chunk(IVec3::new(0, 1, 0));
        let b = WorldGenerator::new(2).generate_chunk(IVec3::new(0, 1, 0));
        assert_ne!(all_blocks(&a), all_blocks(&b));
    }

    #[test]
    fn high_altitude_chunk_is_air() {
        let world_gen = WorldGenerator::new(7);
        let chunk = world_gen.generate_chunk(IVec3::new(0, 3, 0));
        assert_eq!(chunk.is_uniform(), Some(blocks::AIR));
        assert!(chunk.is_all_air());
    }

    #[test]
    fn generates_grid_without_panicking() {
        let world_gen = WorldGenerator::new(2026);
        for cx in -2..3 {
            for cy in 0..WORLD_HEIGHT_CHUNKS {
                for cz in -2..3 {
                    let _ = world_gen.generate_chunk(IVec3::new(cx, cy, cz));
                }
            }
        }
    }

    #[test]
    fn column_block_layers_are_correct() {
        // Deep underwater column (surface well below sea level): flooded up
        // to sea level, air above.
        assert_eq!(column_block(10, SEA_LEVEL), blocks::WATER);
        assert_eq!(column_block(10, 11), blocks::WATER);
        assert_eq!(column_block(10, SEA_LEVEL + 1), blocks::AIR);
        assert_eq!(surface_block(10), blocks::GRASS);

        // Shoreline column (surface within +-2 of sea level): sand, no
        // flooding needed right at the surface.
        assert_eq!(surface_block(SEA_LEVEL), blocks::SAND);
        assert_eq!(surface_block(SEA_LEVEL - 2), blocks::SAND);
        assert_eq!(surface_block(SEA_LEVEL + 2), blocks::SAND);

        // High, dry column: grass on top, dirt, then stone.
        let surface = SEA_LEVEL + 20;
        assert_eq!(column_block(surface, surface), blocks::GRASS);
        assert_eq!(column_block(surface, surface - 1), blocks::DIRT);
        assert_eq!(column_block(surface, surface - 3), blocks::DIRT);
        assert_eq!(column_block(surface, surface - 4), blocks::STONE);
        assert_eq!(column_block(surface, surface + 1), blocks::AIR);
    }

    #[test]
    fn known_columns_water_flooded_and_grass_in_generated_chunk() {
        let world_gen = WorldGenerator::new(42);

        // Scan real generated terrain for one column clearly underwater and
        // one clearly above the shoreline.
        let mut underwater = None;
        let mut grass = None;
        for x in -300..300 {
            let h = world_gen.height_at(x, 0);
            if underwater.is_none() && h < SEA_LEVEL - 2 {
                underwater = Some((x, h));
            }
            if grass.is_none() && h > SEA_LEVEL + 2 {
                grass = Some((x, h));
            }
            if underwater.is_some() && grass.is_some() {
                break;
            }
        }

        let (ux, _) = underwater.expect("expected an underwater column in scan range");
        let (gx, gh) = grass.expect("expected a grass column in scan range");

        let (chunk_pos, local) = crate::split_block_pos(IVec3::new(ux, SEA_LEVEL, 0));
        let chunk = world_gen.generate_chunk(chunk_pos);
        assert_eq!(
            chunk.get(UVec3::new(local.x as u32, local.y as u32, local.z as u32)),
            blocks::WATER
        );

        let (chunk_pos, local) = crate::split_block_pos(IVec3::new(gx, gh, 0));
        let chunk = world_gen.generate_chunk(chunk_pos);
        assert_eq!(
            chunk.get(UVec3::new(local.x as u32, local.y as u32, local.z as u32)),
            blocks::GRASS
        );
    }

    #[test]
    fn trees_are_generated_and_contained() {
        let world_gen = WorldGenerator::new(2026);
        let mut found_log = false;
        let mut found_leaves = false;
        for cx in -3..4 {
            for cy in 0..WORLD_HEIGHT_CHUNKS {
                for cz in -3..4 {
                    let chunk = world_gen.generate_chunk(IVec3::new(cx, cy, cz));
                    for b in all_blocks(&chunk) {
                        found_log |= b == blocks::LOG;
                        found_leaves |= b == blocks::LEAVES;
                    }
                }
            }
        }
        assert!(found_log, "expected at least one LOG block across scanned chunks");
        assert!(found_leaves, "expected at least one LEAVES block across scanned chunks");
    }
}
