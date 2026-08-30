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
//!   deterministic per-column hash. A chunk considers tree anchors from
//!   every column in its 3x3 chunk neighborhood (not just its own columns),
//!   and draws whichever part of each anchor's shape falls inside itself.
//!   This makes trees near chunk borders seam-consistent: the anchor is a
//!   pure function of world column + seed, so neighboring chunks agree
//!   exactly on the blocks they share.
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

/// Maximum horizontal reach of a tree's canopy from its trunk column. Used to
/// size the neighbor-column scan so cross-chunk canopies aren't missed.
const TREE_CANOPY_RADIUS: i32 = 2;

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
pub(crate) fn surface_block(surface: i32) -> BlockId {
    if (surface - SEA_LEVEL).abs() <= 2 {
        blocks::SAND
    } else {
        blocks::GRASS
    }
}

/// Converts a world-space coordinate into a chunk-local coordinate on one
/// axis, or `None` if it falls outside the chunk starting at `base`.
fn local_axis(world: i32, base: i32) -> Option<u32> {
    let l = world - base;
    if (0..CHUNK_SIZE as i32).contains(&l) {
        Some(l as u32)
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

    /// Terrain height for world-space column `(x, z)`. Shared by level-0
    /// generation and the LOD pyramid ([`crate::lod`]) so both sample the
    /// exact same noise field.
    pub(crate) fn column_height(&self, x: i32, z: i32) -> i32 {
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
                *h = self.column_height(base.x + lx as i32, base.z + lz as i32);
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

        // Trees: a second pass over fully-set terrain (so leaf placement's
        // "only over air" check always sees finished terrain, never
        // depending on iteration order), scanning tree ANCHORS from every
        // column in this chunk's 3x3 chunk neighborhood — not just its own
        // columns. An anchor is a pure function of world column + seed, so
        // whichever chunk owns a given block of its shape draws it the same
        // way; this is what keeps trees seam-consistent across borders.
        let margin = CHUNK_SIZE as i32;
        for wx in (base.x - margin)..(base.x + 2 * margin) {
            for wz in (base.z - margin)..(base.z + 2 * margin) {
                let surface = self.column_height(wx, wz);
                if surface_block(surface) != blocks::GRASS || surface <= SEA_LEVEL {
                    continue;
                }
                let hash = column_hash(self.seed, wx, wz);
                if hash.is_multiple_of(TREE_CHANCE) {
                    Self::place_tree(&mut chunk, base, wx, wz, surface, hash);
                }
            }
        }

        chunk
    }

    /// Places one tree anchored at world column `(anchor_x, anchor_z)` —
    /// trunk of logs starting one block above `surface`, plus a simple leaf
    /// blob around its top — into `chunk`, which spans `base..base +
    /// CHUNK_SIZE` on every axis.
    ///
    /// The anchor may belong to a neighboring chunk: every block of the
    /// shape is bounds-checked against `chunk` independently and skipped if
    /// it falls outside, so only the portion that actually intersects
    /// `chunk` gets drawn here. Trunk blocks always win (as before); leaves
    /// never overwrite non-air terrain.
    fn place_tree(
        chunk: &mut Chunk,
        base: IVec3,
        anchor_x: i32,
        anchor_z: i32,
        surface: i32,
        hash: u64,
    ) {
        let trunk_height = 4 + (hash % 2) as i32;
        let top_y = surface + trunk_height;

        // Trunk: only relevant if the anchor column itself is inside this
        // chunk (the trunk never leaves its own column).
        if let (Some(lx), Some(lz)) = (local_axis(anchor_x, base.x), local_axis(anchor_z, base.z)) {
            for i in 1..=trunk_height {
                if let Some(ly) = local_axis(surface + i, base.y) {
                    chunk.set(UVec3::new(lx, ly, lz), blocks::LOG);
                }
            }
        }

        // Leaf canopy: checked block-by-block, since it can reach into a
        // neighboring chunk even when the trunk column cannot.
        for dy in -1..=1i32 {
            let Some(ly) = local_axis(top_y + dy, base.y) else {
                continue;
            };
            for dx in -TREE_CANOPY_RADIUS..=TREE_CANOPY_RADIUS {
                for dz in -TREE_CANOPY_RADIUS..=TREE_CANOPY_RADIUS {
                    // Round off the far corners of the wide layers, and keep
                    // the top cap layer narrow, for a simple round-ish blob.
                    if dy < 1 && dx.abs() == 2 && dz.abs() == 2 {
                        continue;
                    }
                    if dy == 1 && (dx.abs() > 1 || dz.abs() > 1) {
                        continue;
                    }
                    let (Some(lx), Some(lz)) = (
                        local_axis(anchor_x + dx, base.x),
                        local_axis(anchor_z + dz, base.z),
                    ) else {
                        continue;
                    };
                    let local = UVec3::new(lx, ly, lz);
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
            let h = world_gen.column_height(x, 0);
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
    fn trees_are_generated() {
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
        assert!(
            found_log,
            "expected at least one LOG block across scanned chunks"
        );
        assert!(
            found_leaves,
            "expected at least one LEAVES block across scanned chunks"
        );
    }

    /// Reads the block at a world-space position by generating whichever
    /// chunk owns it. Used to cross-check cross-chunk tree shapes: since
    /// each block belongs to exactly one chunk, this is the ground truth for
    /// "what does the world actually contain here".
    fn read_world_block(world_gen: &WorldGenerator, world_pos: IVec3) -> BlockId {
        let (chunk_pos, local) = crate::split_block_pos(world_pos);
        let chunk = world_gen.generate_chunk(chunk_pos);
        chunk.get(UVec3::new(local.x as u32, local.y as u32, local.z as u32))
    }

    /// Now that trees are no longer confined to a chunk-interior margin, a
    /// tree's trunk can land inside the outer 2-block band of a chunk (i.e.
    /// with no room left for the old margin) — statistically confirm this
    /// actually happens, rather than merely compiling.
    #[test]
    fn trunks_occur_within_two_blocks_of_a_chunk_border() {
        let world_gen = WorldGenerator::new(2026);
        let border = 2usize;
        let mut found_border_trunk = false;

        'scan: for cx in -3..4 {
            for cy in 0..WORLD_HEIGHT_CHUNKS {
                for cz in -3..4 {
                    let chunk = world_gen.generate_chunk(IVec3::new(cx, cy, cz));
                    for lz in 0..CHUNK_SIZE {
                        let z_edge = lz < border || lz >= CHUNK_SIZE - border;
                        for lx in 0..CHUNK_SIZE {
                            let x_edge = lx < border || lx >= CHUNK_SIZE - border;
                            if !x_edge && !z_edge {
                                continue;
                            }
                            for ly in 0..CHUNK_SIZE {
                                if chunk.get(UVec3::new(lx as u32, ly as u32, lz as u32))
                                    == blocks::LOG
                                {
                                    found_border_trunk = true;
                                    break 'scan;
                                }
                            }
                        }
                    }
                }
            }
        }

        assert!(
            found_border_trunk,
            "expected at least one tree trunk within 2 blocks of a chunk border"
        );
    }

    /// A tree anchored right at a chunk border must come out identical
    /// whether its blocks are read from the chunk to its west or the chunk
    /// to its east: the anchor is a pure function of world column + seed, so
    /// neither chunk may clip or duplicate part of the shape.
    #[test]
    fn cross_chunk_tree_shape_is_seam_consistent() {
        let world_gen = WorldGenerator::new(2026);

        // Find an isolated anchor whose trunk column sits exactly on a
        // chunk border (world x == 31 or 32, a multiple-of-32 boundary), so
        // its canopy (radius 2) necessarily spans two chunks. "Isolated"
        // means no other anchor within canopy-overlap range, so the
        // expected shape is unambiguous.
        let mut found = None;
        let size = CHUNK_SIZE as i32;
        'search: for boundary_chunk in -6..6i32 {
            for &wx in &[boundary_chunk * size - 1, boundary_chunk * size] {
                for wz in -200..200i32 {
                    let surface = world_gen.column_height(wx, wz);
                    if surface_block(surface) != blocks::GRASS || surface <= SEA_LEVEL {
                        continue;
                    }
                    let hash = column_hash(world_gen.seed, wx, wz);
                    if !hash.is_multiple_of(TREE_CHANCE) {
                        continue;
                    }
                    let isolated = (-4..=4i32).all(|dz| {
                        (-4..=4i32).all(|dx| {
                            if dx == 0 && dz == 0 {
                                return true;
                            }
                            let (ox, oz) = (wx + dx, wz + dz);
                            let osurface = world_gen.column_height(ox, oz);
                            if surface_block(osurface) != blocks::GRASS || osurface <= SEA_LEVEL {
                                return true;
                            }
                            !column_hash(world_gen.seed, ox, oz).is_multiple_of(TREE_CHANCE)
                        })
                    });
                    if isolated {
                        found = Some((wx, wz, surface, hash));
                        break 'search;
                    }
                }
            }
        }
        let (anchor_x, anchor_z, surface, hash) =
            found.expect("expected an isolated border-straddling tree anchor in scan range");

        // Replicate place_tree's shape formula to compute the expected
        // block at every position it touches, trunk taking priority over
        // leaves (matches production: trunk is set unconditionally first),
        // and natural terrain showing through wherever a leaf would land on
        // non-air ground.
        let trunk_height = 4 + (hash % 2) as i32;
        let top_y = surface + trunk_height;

        let mut expected: std::collections::HashMap<IVec3, BlockId> =
            std::collections::HashMap::new();
        for i in 1..=trunk_height {
            expected.insert(IVec3::new(anchor_x, surface + i, anchor_z), blocks::LOG);
        }
        for dy in -1..=1i32 {
            for dx in -2..=2i32 {
                for dz in -2..=2i32 {
                    if dy < 1 && dx.abs() == 2 && dz.abs() == 2 {
                        continue;
                    }
                    if dy == 1 && (dx.abs() > 1 || dz.abs() > 1) {
                        continue;
                    }
                    let pos = IVec3::new(anchor_x + dx, top_y + dy, anchor_z + dz);
                    if expected.contains_key(&pos) {
                        continue; // trunk already claims this position
                    }
                    let local_surface = world_gen.column_height(pos.x, pos.z);
                    let terrain = column_block(local_surface, pos.y);
                    expected.insert(
                        pos,
                        if terrain.is_air() {
                            blocks::LEAVES
                        } else {
                            terrain
                        },
                    );
                }
            }
        }

        for (pos, expected_block) in expected {
            assert_eq!(
                read_world_block(&world_gen, pos),
                expected_block,
                "mismatch at {pos:?} (anchor {anchor_x},{anchor_z})"
            );
        }
    }
}
