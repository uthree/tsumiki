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
//! - Ore veins (coal, iron) in stone, placed the same cross-chunk-anchor
//!   way as trees, but in 3D and drawn from a coarse lattice rather than
//!   every block: space is divided into [`ORE_CELL_SIZE`]-block cells, each
//!   hashed once to decide whether it holds a vein and where inside the
//!   cell the anchor jitters to, keeping the scan a few hundred cells
//!   instead of tens of thousands of block candidates. A chunk considers
//!   cells covering its volume expanded by [`VEIN_REACH`] blocks on every
//!   axis, and each anchor's short random-walk cluster is drawn wherever it
//!   falls inside the current chunk. Coal is common at any depth; iron is
//!   rarer and gets more common the lower the absolute world Y, so digging
//!   down is what finds it. Veins only ever replace `STONE`.
//! - Two intersecting 3D Perlin fields carve winding caves before ore
//!   placement. Their vertical scale keeps passages broad enough to walk
//!   through. A third, 2D field limits surface openings to occasional dry
//!   land patches; the sea floor and the bottom three world layers stay
//!   intact. All masks use world coordinates, including across chunk edges.
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

/// Three untouched bottom layers prevent natural caves opening into the void.
const CAVE_FLOOR_Y: i32 = 3;
/// Thickness of the roof below sea beds and closed surface patches.
const CAVE_ROOF_DEPTH: i32 = 5;
/// Intersecting thick noise isosurfaces form passages instead of isolated
/// spherical pockets. These frequencies make them several blocks wide and
/// somewhat flatter vertically, without constraining them to a fixed Y band.
const CAVE_HORIZONTAL_FREQUENCY: f64 = 0.024;
const CAVE_VERTICAL_FREQUENCY: f64 = 0.040;
const CAVE_RADIUS_SQUARED: f64 = 0.0324;

/// One in `TREE_CHANCE` eligible columns grows a tree.
const TREE_CHANCE: u64 = 40;

/// Maximum horizontal reach of a tree's canopy from its trunk column. Used to
/// size the neighbor-column scan so cross-chunk canopies aren't missed.
const TREE_CANOPY_RADIUS: i32 = 2;

/// Minimum depth below the local surface for a position to be `STONE`
/// (mirrors [`column_block`]'s own layering: dirt occupies
/// `surface - DIRT_DEPTH ..= surface`, stone starts one below that). Ore
/// vein anchors are only rolled at or below this depth.
const MIN_STONE_DEPTH: i32 = DIRT_DEPTH + 1;

/// Edge length of one ore-anchor lattice cell, in blocks. Vein anchors are
/// drawn from a coarse lattice rather than tested at every block: each cell
/// is hashed once (see [`cell_hash`]) to decide whether it holds a vein and
/// where inside the cell the anchor jitters to, so the scan cost is
/// `(chunk span / ORE_CELL_SIZE)^3` instead of `(chunk span)^3` — a few
/// hundred cells instead of tens of thousands of blocks. Small enough that
/// jitter still hides the lattice (see the vein-clustering test and this
/// module's manual visual check), large enough to keep the cell count low.
const ORE_CELL_SIZE: i32 = 6;

/// One in `COAL_CELL_DIVISOR` depth-gated lattice cells holds a coal vein.
/// Flat: coal is equally likely at any depth.
///
/// This is a per-*cell* rate (one candidate anchor per `ORE_CELL_SIZE^3`
/// cell), not the resulting ore density — see
/// `ore_density_and_depth_bias_are_within_bounds` for the test that pins
/// it. Measured at this value: coal is ~0.47% of stone-family blocks
/// (STONE/COAL_ORE/IRON_ORE) at both sampled depths — common enough to
/// reliably find within a short dig, comfortably under the ~1% mark that
/// would start to look like stone visibly speckled with coal. Retune
/// together with that test's bounds if this changes.
const COAL_CELL_DIVISOR: u64 = 4;
/// Coal veins are `COAL_VEIN_MIN_SIZE..=COAL_VEIN_MAX_SIZE` blocks (walk
/// steps; some steps may miss stone and place nothing, see
/// [`WorldGenerator::place_vein`]).
const COAL_VEIN_MIN_SIZE: u64 = 3;
const COAL_VEIN_MAX_SIZE: u64 = 6;

/// Maximum reach (on any single axis) of an ore vein's random walk from its
/// anchor: each of a vein's `size - 1` moves (the walk places at its
/// current position, then moves, so the last move is never used) can nudge
/// one axis by at most 1, so the true bound is `size - 1`; this rounds up
/// to the larger vein's full step count for a safety margin. Used to size
/// the neighbor-anchor scan so a chunk always has an anchor in its own scan
/// margin whenever that anchor's walk could reach into the chunk — the same
/// reason trees scan a margin wider than the canopy actually needs. This is
/// a margin in blocks, not lattice cells; the lattice-cell range that
/// covers it is derived from it directly (see `generate_chunk`), so a cell
/// containing an anchor whose walk could reach this chunk is never missed
/// regardless of where in its cell that anchor's jitter lands.
const VEIN_REACH: i32 = COAL_VEIN_MAX_SIZE as i32;

/// World-Y band over which iron's vein chance ramps from rare to common:
/// at or above `IRON_RAMP_TOP_Y` iron uses [`IRON_CELL_MAX_DIVISOR`] (rare);
/// at or below `IRON_RAMP_BOTTOM_Y` (the world floor) it uses
/// [`IRON_CELL_MIN_DIVISOR`] (still rarer than coal, but the most common
/// iron gets). Absolute world Y, not depth below the local surface, so
/// iron's rarity at a given altitude is the same everywhere on the map —
/// the incentive is "go down", not "dig deeper than your local hill happens
/// to be tall".
const IRON_RAMP_TOP_Y: i32 = SEA_LEVEL;
const IRON_RAMP_BOTTOM_Y: i32 = 0;
/// Per-cell divisors (see [`COAL_CELL_DIVISOR`] for what that means): kept
/// well above `COAL_CELL_DIVISOR` at every depth so iron always stays
/// rarer than coal, with a wide max/min ratio for a pronounced depth bias.
const IRON_CELL_MAX_DIVISOR: u64 = 56;
const IRON_CELL_MIN_DIVISOR: u64 = 7;
/// Iron veins are smaller than coal's: they're the reward for depth, not a
/// bulk resource.
const IRON_VEIN_MIN_SIZE: u64 = 2;
const IRON_VEIN_MAX_SIZE: u64 = 4;

/// Arbitrary tags mixed into [`cell_hash`] so coal and iron roll
/// independently at the same lattice cell instead of always agreeing (or
/// jittering to the same spot) together.
const COAL_VEIN_TAG: u64 = 0xC0A1_0000_C0A1;
const IRON_VEIN_TAG: u64 = 0x1520_0000_1520;

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

/// Deterministic hash of `(seed, cell, tag)`, used to roll one ore vein
/// candidate per lattice cell (see [`ORE_CELL_SIZE`]) instead of testing
/// every block: `cell` is a lattice-cell coordinate (world position divided
/// by `ORE_CELL_SIZE`), not a block position. `tag` decorrelates different
/// ore kinds so coal and iron don't always agree (or jitter to the same
/// spot) at the same cell.
fn cell_hash(seed: u64, cell: IVec3, tag: u64) -> u64 {
    let h = splitmix64(seed ^ tag);
    let h = splitmix64(h ^ (cell.x as i64 as u64));
    let h = splitmix64(h ^ (cell.y as i64 as u64));
    splitmix64(h ^ (cell.z as i64 as u64))
}

/// Divisor for iron's vein-cell chance check at world-space `wy`: a smaller
/// divisor means a higher chance. See [`IRON_RAMP_TOP_Y`] for why this
/// ramps by absolute Y rather than depth below the local surface.
fn iron_divisor(wy: i32) -> u64 {
    let span = (IRON_RAMP_TOP_Y - IRON_RAMP_BOTTOM_Y) as i64;
    let t = (IRON_RAMP_TOP_Y - wy).clamp(0, span as i32) as i64;
    let range = (IRON_CELL_MAX_DIVISOR - IRON_CELL_MIN_DIVISOR) as i64;
    (IRON_CELL_MAX_DIVISOR as i64 - range * t / span) as u64
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
    cave_a: Perlin,
    cave_b: Perlin,
    cave_entrances: Perlin,
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
        Self {
            seed,
            heightmap,
            cave_a: Perlin::new(splitmix64(seed ^ 0x000C_A7EA) as u32),
            cave_b: Perlin::new(splitmix64(seed ^ 0x000C_A7EB) as u32),
            cave_entrances: Perlin::new(splitmix64(seed ^ 0x000C_A7EE) as u32),
        }
    }

    /// Terrain height for world-space column `(x, z)`. Shared by level-0
    /// generation and the LOD pyramid ([`crate::lod`]) so both sample the
    /// exact same noise field.
    pub(crate) fn column_height(&self, x: i32, z: i32) -> i32 {
        let n = self.heightmap.get([x as f64, z as f64]);
        let h = (BASE_HEIGHT + n * HEIGHT_AMPLITUDE).round() as i32;
        h.clamp(1, crate::WORLD_HEIGHT_BLOCKS - 9)
    }

    /// Maximum carved Y in a column. Including its immediate neighbors in
    /// the sea-bed check prevents a dry-land mouth opening sideways into
    /// water in an adjacent column, even at a chunk boundary.
    fn cave_ceiling(&self, x: i32, z: i32, surface: i32, neighborhood_min: i32) -> i32 {
        if neighborhood_min <= SEA_LEVEL + 2 {
            return neighborhood_min - CAVE_ROOF_DEPTH;
        }
        let entrance = self
            .cave_entrances
            .get([x as f64 * 0.013 + 31.7, z as f64 * 0.013 - 9.3]);
        if entrance > 0.22 {
            surface
        } else {
            surface - CAVE_ROOF_DEPTH
        }
    }

    /// The cave field has no chunk-local inputs or cached neighbor state.
    /// Fractional offsets keep the common integer-lattice zeros of Perlin
    /// noise from forcing a cave at the spawn column in every seed.
    fn cave_at(&self, pos: IVec3, ceiling: i32) -> bool {
        if pos.y < CAVE_FLOOR_Y || pos.y > ceiling {
            return false;
        }
        let x = pos.x as f64 * CAVE_HORIZONTAL_FREQUENCY;
        let y = pos.y as f64 * CAVE_VERTICAL_FREQUENCY;
        let z = pos.z as f64 * CAVE_HORIZONTAL_FREQUENCY;
        let a = self.cave_a.get([x + 23.2, y + 7.8, z - 16.4]);
        let b = self.cave_b.get([x - 8.7, y - 21.1, z + 34.6]);
        a * a + b * b < CAVE_RADIUS_SQUARED
    }

    fn column_cave_ceiling(&self, x: i32, z: i32, surface: i32) -> i32 {
        let mut lowest = surface;
        for dx in -1..=1 {
            for dz in -1..=1 {
                lowest = lowest.min(self.column_height(x + dx, z + dz));
            }
        }
        self.cave_ceiling(x, z, surface, lowest)
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

        // A one-column halo supplies the sea-bed roof check. Sharing this
        // cache avoids nine heightmap evaluations for each terrain column.
        let mut heights = [[0i32; CHUNK_SIZE + 2]; CHUNK_SIZE + 2];
        for (lx, row) in heights.iter_mut().enumerate() {
            for (lz, h) in row.iter_mut().enumerate() {
                *h = self.column_height(base.x + lx as i32 - 1, base.z + lz as i32 - 1);
            }
        }

        for lx in 0..CHUNK_SIZE {
            for lz in 0..CHUNK_SIZE {
                let surface = heights[lx + 1][lz + 1];
                let neighborhood_min = heights[lx..=lx + 2]
                    .iter()
                    .flat_map(|row| row[lz..=lz + 2].iter())
                    .copied()
                    .min()
                    .unwrap();
                let wx = base.x + lx as i32;
                let wz = base.z + lz as i32;
                let ceiling = self.cave_ceiling(wx, wz, surface, neighborhood_min);
                for ly in 0..CHUNK_SIZE {
                    let wy = base.y + ly as i32;
                    let block = column_block(surface, wy);
                    if !block.is_air() && !self.cave_at(IVec3::new(wx, wy, wz), ceiling) {
                        chunk.set(UVec3::new(lx as u32, ly as u32, lz as u32), block);
                    }
                }
            }
        }

        // Ore veins: a pass over the already-filled terrain (so the STONE
        // check in place_vein always sees finished terrain), scanning vein
        // candidates on a coarse lattice of ORE_CELL_SIZE^3 cells covering
        // this chunk's volume expanded by VEIN_REACH blocks on every axis —
        // not just this chunk's own volume. Testing every block as a
        // candidate anchor (the original design) cost `(chunk span)^3`
        // hash-and-modulo checks per ore type just to place a handful of
        // veins; hashing a lattice cell once instead, and jittering the
        // anchor to a position inside it, cuts that to
        // `(chunk span / ORE_CELL_SIZE)^3` — a few hundred cells rather
        // than tens of thousands of blocks — while keeping every property
        // the per-block version had: an anchor is a pure function of
        // `(seed, cell, tag)` (see `cell_hash`), independent of generation
        // order, so whichever chunk owns a given step of the resulting vein
        // draws it identically. This is the same cross-chunk-anchor trick
        // the trees above use, just with candidates drawn from a lattice
        // instead of every block.
        //
        // The cell range is derived from the same VEIN_REACH block margin
        // as before (not "margin + one cell"): converting the outer edge of
        // that margin to cell coordinates already yields the outermost cell
        // that could contain *any* block within reach, regardless of where
        // that cell's own jitter lands an anchor, so no case is missed.
        let scan_lo = base - IVec3::splat(VEIN_REACH);
        let scan_hi = base + IVec3::splat(CHUNK_SIZE as i32 + VEIN_REACH); // exclusive
        let cell_lo = IVec3::new(
            scan_lo.x.div_euclid(ORE_CELL_SIZE),
            scan_lo.y.div_euclid(ORE_CELL_SIZE),
            scan_lo.z.div_euclid(ORE_CELL_SIZE),
        );
        let cell_hi = IVec3::new(
            (scan_hi.x - 1).div_euclid(ORE_CELL_SIZE),
            (scan_hi.y - 1).div_euclid(ORE_CELL_SIZE),
            (scan_hi.z - 1).div_euclid(ORE_CELL_SIZE),
        );
        for cx in cell_lo.x..=cell_hi.x {
            for cy in cell_lo.y..=cell_hi.y {
                for cz in cell_lo.z..=cell_hi.z {
                    let cell = IVec3::new(cx, cy, cz);

                    let coal_hash = cell_hash(self.seed, cell, COAL_VEIN_TAG);
                    self.try_place_vein_cell(
                        &mut chunk,
                        base,
                        cell,
                        coal_hash,
                        blocks::COAL_ORE,
                        |_wy| COAL_CELL_DIVISOR,
                        COAL_VEIN_MIN_SIZE,
                        COAL_VEIN_MAX_SIZE,
                    );

                    // Iron's divisor depends on the jittered anchor's own Y
                    // (not the cell's), so it's resolved via a callback once
                    // try_place_vein_cell knows where in the cell the
                    // anchor landed.
                    let iron_hash = cell_hash(self.seed, cell, IRON_VEIN_TAG);
                    self.try_place_vein_cell(
                        &mut chunk,
                        base,
                        cell,
                        iron_hash,
                        blocks::IRON_ORE,
                        iron_divisor,
                        IRON_VEIN_MIN_SIZE,
                        IRON_VEIN_MAX_SIZE,
                    );
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
                    let ceiling = self.column_cave_ceiling(wx, wz, surface);
                    if self.cave_at(IVec3::new(wx, surface, wz), ceiling) {
                        continue;
                    }
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

    /// Rolls one ore-vein candidate for lattice `cell`: derives a jittered
    /// anchor position inside the cell from `hash` (so the anchor doesn't
    /// sit on a visible grid), and — if that position is deep enough to be
    /// natural stone and passes the `divisor(anchor.y)` chance check —
    /// draws a vein of `ore` into `chunk`. `divisor` is a callback rather
    /// than a plain value because iron's chance depends on the *anchor's*
    /// Y, which is only known after jittering (see [`iron_divisor`]); coal
    /// just ignores the argument and returns a constant.
    #[allow(clippy::too_many_arguments)]
    fn try_place_vein_cell(
        &self,
        chunk: &mut Chunk,
        base: IVec3,
        cell: IVec3,
        hash: u64,
        ore: BlockId,
        divisor: impl Fn(i32) -> u64,
        min_size: u64,
        max_size: u64,
    ) {
        // Jitter bits come from well-separated ranges of the hash (same
        // spacing trick place_vein uses for its walk directions) so the
        // three axes don't correlate.
        let dx = (hash % ORE_CELL_SIZE as u64) as i32;
        let dy = ((hash >> 21) % ORE_CELL_SIZE as u64) as i32;
        let dz = ((hash >> 42) % ORE_CELL_SIZE as u64) as i32;
        let anchor = cell * ORE_CELL_SIZE + IVec3::new(dx, dy, dz);

        let surface = self.column_height(anchor.x, anchor.z);
        if surface - anchor.y < MIN_STONE_DEPTH {
            return;
        }

        // Roll the accept/reject decision forward from the jitter bits
        // (rather than reusing them) so the two draws are independent, then
        // roll forward again for the walk so accept/reject and vein shape
        // don't correlate either.
        let decision_hash = splitmix64(hash);
        if !decision_hash.is_multiple_of(divisor(anchor.y)) {
            return;
        }
        let walk_hash = splitmix64(decision_hash);
        Self::place_vein(chunk, base, anchor, walk_hash, ore, min_size, max_size);
    }

    /// Places a small `ore` vein anchored at world position `anchor`, via a
    /// short deterministic pseudo-random walk seeded by `hash`, into
    /// `chunk` (which spans `base..base + CHUNK_SIZE` on every axis).
    ///
    /// Mirrors [`Self::place_tree`]'s cross-chunk technique: the walk is a
    /// pure function of the anchor position, the ore's tag and the world
    /// seed, so whichever chunk owns a given step draws it identically —
    /// this is what keeps veins seam-consistent across chunk borders.
    /// Every step only ever overwrites `STONE` (checked in `chunk`, which
    /// by this point in generation holds finished terrain): never dirt,
    /// sand, water, air, another ore, or a player edit, so a vein can only
    /// appear where natural stone generated.
    fn place_vein(
        chunk: &mut Chunk,
        base: IVec3,
        anchor: IVec3,
        mut hash: u64,
        ore: BlockId,
        min_size: u64,
        max_size: u64,
    ) {
        let size = min_size + hash % (max_size - min_size + 1);
        let mut pos = anchor;
        for _ in 0..size {
            if let (Some(lx), Some(ly), Some(lz)) = (
                local_axis(pos.x, base.x),
                local_axis(pos.y, base.y),
                local_axis(pos.z, base.z),
            ) {
                let local = UVec3::new(lx, ly, lz);
                if chunk.get(local) == blocks::STONE {
                    chunk.set(local, ore);
                }
            }
            // Advance the walk: each axis nudged by -1, 0 or +1 (bits from
            // well-separated ranges of the 64-bit hash, so the three axes
            // don't correlate), keeping the shape a compact clump rather
            // than a scattered trail.
            hash = splitmix64(hash);
            let dx = (hash % 3) as i32 - 1;
            let dy = ((hash >> 21) % 3) as i32 - 1;
            let dz = ((hash >> 42) % 3) as i32 - 1;
            pos += IVec3::new(dx, dy, dz);
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
            if grass.is_none()
                && h > SEA_LEVEL + 2
                && !world_gen.cave_at(IVec3::new(x, h, 0), world_gen.column_cave_ceiling(x, 0, h))
            {
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
                    if world_gen.cave_at(
                        IVec3::new(wx, surface, wz),
                        world_gen.column_cave_ceiling(wx, wz, surface),
                    ) {
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

    #[test]
    fn ore_generation_is_deterministic() {
        let world_gen = WorldGenerator::new(3141);
        // Bottom-of-world chunk: deep enough that both ores are all but
        // guaranteed, so this test isn't vacuously true.
        let pos = IVec3::new(1, 0, -2);
        let a = world_gen.generate_chunk(pos);
        let b = world_gen.generate_chunk(pos);
        assert_eq!(all_blocks(&a), all_blocks(&b));
        assert!(
            all_blocks(&a)
                .iter()
                .any(|b| *b == blocks::COAL_ORE || *b == blocks::IRON_ORE),
            "expected this chunk to contain ore; test would be vacuous otherwise"
        );
    }

    #[test]
    fn ore_only_ever_replaces_stone() {
        let world_gen = WorldGenerator::new(555);
        let mut found_ore = false;
        for cx in -2..2 {
            for cz in -2..2 {
                let base = IVec3::new(cx * CHUNK_SIZE as i32, 0, cz * CHUNK_SIZE as i32);
                let chunk = world_gen.generate_chunk(IVec3::new(cx, 0, cz));
                for lx in 0..CHUNK_SIZE {
                    for lz in 0..CHUNK_SIZE {
                        let wx = base.x + lx as i32;
                        let wz = base.z + lz as i32;
                        let surface = world_gen.column_height(wx, wz);
                        for ly in 0..CHUNK_SIZE {
                            let wy = base.y + ly as i32;
                            let b = chunk.get(UVec3::new(lx as u32, ly as u32, lz as u32));
                            if b == blocks::COAL_ORE || b == blocks::IRON_ORE {
                                found_ore = true;
                                assert_eq!(
                                    column_block(surface, wy),
                                    blocks::STONE,
                                    "ore at {:?} sits where natural terrain would be {:?}, not stone",
                                    IVec3::new(wx, wy, wz),
                                    column_block(surface, wy)
                                );
                            }
                        }
                    }
                }
            }
        }
        assert!(found_ore, "expected to find ore in the scanned volume");
    }

    /// Measures coal and iron frequency (ore blocks per stone-family block:
    /// `STONE`, `COAL_ORE` or `IRON_ORE`) in two absolute-Y bands, rather
    /// than asserting on any one hand-picked block or trusting a
    /// back-of-envelope estimate from the anchor-scan rate. This pins two
    /// things pinned separately would otherwise silently drift apart:
    ///
    /// - **Absolute density**: coal must read as "common enough to rely on
    ///   for fuel" without reading as "stone is visibly speckled with it" —
    ///   see [`COAL_CELL_DIVISOR`] for the measured numbers and the target
    ///   this pins.
    /// - **Depth shape**: iron's chance ramps with world Y
    ///   ([`IRON_RAMP_TOP_Y`]/[`IRON_RAMP_BOTTOM_Y`]); coal's does not, so
    ///   iron should swing sharply between bands while coal stays flat, and
    ///   iron should stay rarer than coal at both.
    ///
    /// Two seeds are scanned so the bounds reflect the configured rates,
    /// not a lucky/unlucky draw from a single seed.
    #[test]
    fn ore_density_and_depth_bias_are_within_bounds() {
        // "Shallow" sits entirely at/above IRON_RAMP_TOP_Y (SEA_LEVEL, 36),
        // iron's flat rare plateau. "Deep" sits mostly below the ramp,
        // where iron is markedly more common. Scanning cy 0 and 1 covers
        // world Y 0..64, containing both bands.
        let shallow = 40..60;
        let deep = 0..20;

        let mut stone_family = [0u64; 2]; // [shallow, deep]
        let mut coal = [0u64; 2];
        let mut iron = [0u64; 2];

        for seed in [9001, 424_242] {
            let world_gen = WorldGenerator::new(seed);
            for cx in -5..5 {
                for cz in -5..5 {
                    for cy in 0..2 {
                        let base_y = cy * CHUNK_SIZE as i32;
                        let chunk = world_gen.generate_chunk(IVec3::new(cx, cy, cz));
                        for lx in 0..CHUNK_SIZE {
                            for lz in 0..CHUNK_SIZE {
                                for ly in 0..CHUNK_SIZE {
                                    let wy = base_y + ly as i32;
                                    let bin = if shallow.contains(&wy) {
                                        0
                                    } else if deep.contains(&wy) {
                                        1
                                    } else {
                                        continue;
                                    };
                                    let b = chunk.get(UVec3::new(lx as u32, ly as u32, lz as u32));
                                    if b == blocks::STONE
                                        || b == blocks::COAL_ORE
                                        || b == blocks::IRON_ORE
                                    {
                                        stone_family[bin] += 1;
                                        if b == blocks::COAL_ORE {
                                            coal[bin] += 1;
                                        }
                                        if b == blocks::IRON_ORE {
                                            iron[bin] += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        assert!(
            stone_family[0] > 10_000 && stone_family[1] > 10_000,
            "sample too small: shallow={}, deep={}",
            stone_family[0],
            stone_family[1]
        );
        assert!(
            coal[0] > 0 && coal[1] > 0 && iron[1] > 0,
            "expected ore in the sampled volume: coal={coal:?}, iron={iron:?}"
        );

        let freq = |count: u64, total: u64| count as f64 / total as f64;
        let coal_shallow = freq(coal[0], stone_family[0]);
        let coal_deep = freq(coal[1], stone_family[1]);
        let iron_shallow = freq(iron[0], stone_family[0]);
        let iron_deep = freq(iron[1], stone_family[1]);

        // Absolute density, pinned so a change to the vein-chance constants
        // can't silently push coal into "speckled everywhere" territory (or
        // iron into "may as well not exist"). Measured at the time this was
        // written (lattice-based anchor scan, see COAL_CELL_DIVISOR's doc
        // comment): coal ~0.47-0.48%, iron ~0.02-0.08% — bounds below are
        // deliberately loose around that, not a re-assertion of the exact
        // figure.
        assert!(
            (0.001..0.009).contains(&coal_shallow) && (0.001..0.009).contains(&coal_deep),
            "coal density out of the defensible 0.1%-0.9% range: shallow={:.4}%, deep={:.4}%",
            coal_shallow * 100.0,
            coal_deep * 100.0
        );
        assert!(
            iron_shallow < 0.001 && (0.0002..0.002).contains(&iron_deep),
            "iron density out of range: shallow={:.4}%, deep={:.4}%",
            iron_shallow * 100.0,
            iron_deep * 100.0
        );

        // Iron gets meaningfully more common going down.
        assert!(
            iron_deep > iron_shallow * 1.5,
            "expected iron to be much more common deep: shallow={iron_shallow:.6}, deep={iron_deep:.6}"
        );
        // Coal's depth swing, if any, is far smaller than iron's: it's
        // deliberately depth-independent.
        let iron_swing = iron_deep / iron_shallow.max(f64::MIN_POSITIVE);
        let coal_swing = coal_deep / coal_shallow.max(f64::MIN_POSITIVE);
        assert!(
            iron_swing > coal_swing * 1.3,
            "expected iron's deep/shallow ratio ({iron_swing:.2}) to clearly exceed coal's ({coal_swing:.2})"
        );
        // Iron stays rarer than coal at both sampled depths.
        assert!(iron_shallow < coal_shallow);
        assert!(iron_deep < coal_deep);
    }

    /// Ore blocks should come in small clumps, not be scattered one at a
    /// time: most ore blocks should have at least one same-type neighbor.
    #[test]
    fn ore_veins_are_clustered_not_isolated() {
        let world_gen = WorldGenerator::new(2718);
        let mut coal_positions = std::collections::HashSet::new();
        let mut iron_positions = std::collections::HashSet::new();

        for cx in -3..3 {
            for cz in -3..3 {
                for cy in 0..2 {
                    let base = IVec3::new(
                        cx * CHUNK_SIZE as i32,
                        cy * CHUNK_SIZE as i32,
                        cz * CHUNK_SIZE as i32,
                    );
                    let chunk = world_gen.generate_chunk(IVec3::new(cx, cy, cz));
                    for lx in 0..CHUNK_SIZE {
                        for ly in 0..CHUNK_SIZE {
                            for lz in 0..CHUNK_SIZE {
                                let b = chunk.get(UVec3::new(lx as u32, ly as u32, lz as u32));
                                let pos = IVec3::new(
                                    base.x + lx as i32,
                                    base.y + ly as i32,
                                    base.z + lz as i32,
                                );
                                if b == blocks::COAL_ORE {
                                    coal_positions.insert(pos);
                                } else if b == blocks::IRON_ORE {
                                    iron_positions.insert(pos);
                                }
                            }
                        }
                    }
                }
            }
        }

        let has_same_type_neighbor = |set: &std::collections::HashSet<IVec3>, pos: IVec3| {
            for dx in -1..=1i32 {
                for dy in -1..=1i32 {
                    for dz in -1..=1i32 {
                        if dx == 0 && dy == 0 && dz == 0 {
                            continue;
                        }
                        if set.contains(&(pos + IVec3::new(dx, dy, dz))) {
                            return true;
                        }
                    }
                }
            }
            false
        };

        for (name, set) in [("coal", &coal_positions), ("iron", &iron_positions)] {
            assert!(
                set.len() > 20,
                "expected a decent number of {name} blocks, found {}",
                set.len()
            );
            let clustered = set
                .iter()
                .filter(|&&p| has_same_type_neighbor(set, p))
                .count();
            let ratio = clustered as f64 / set.len() as f64;
            assert!(
                ratio > 0.5,
                "expected most {name} ore blocks to have a same-type neighbor \
                 (clustered veins), got {ratio:.2} ({clustered}/{})",
                set.len()
            );
        }
    }

    /// Replicates [`WorldGenerator::place_vein`]'s walk in isolation: the
    /// world positions a vein anchored at `anchor` would touch, restricted
    /// to positions where natural terrain is `STONE` (matches production,
    /// which gates on the already-generated STONE terrain rather than
    /// recomputing it, but the two agree since terrain generation is
    /// itself seam-consistent).
    fn simulate_vein_walk(
        world_gen: &WorldGenerator,
        anchor: IVec3,
        hash: u64,
        min_size: u64,
        max_size: u64,
    ) -> Vec<IVec3> {
        let size = min_size + hash % (max_size - min_size + 1);
        let mut pos = anchor;
        let mut h = hash;
        let mut touched = Vec::new();
        for _ in 0..size {
            let surface = world_gen.column_height(pos.x, pos.z);
            if column_block(surface, pos.y) == blocks::STONE
                && !world_gen.cave_at(pos, world_gen.column_cave_ceiling(pos.x, pos.z, surface))
            {
                touched.push(pos);
            }
            h = splitmix64(h);
            let dx = (h % 3) as i32 - 1;
            let dy = ((h >> 21) % 3) as i32 - 1;
            let dz = ((h >> 42) % 3) as i32 - 1;
            pos += IVec3::new(dx, dy, dz);
        }
        touched
    }

    /// Replicates the resolution half of
    /// [`WorldGenerator::try_place_vein_cell`]: from a lattice cell's hash,
    /// the jittered anchor position and, if it clears the depth gate and
    /// the `divisor(anchor.y)` chance check, the hash that seeds its walk.
    /// `None` means this cell doesn't produce a vein of this kind.
    fn resolve_cell_anchor(
        world_gen: &WorldGenerator,
        cell: IVec3,
        tag: u64,
        divisor: impl Fn(i32) -> u64,
    ) -> Option<(IVec3, u64)> {
        let hash = cell_hash(world_gen.seed, cell, tag);
        let dx = (hash % ORE_CELL_SIZE as u64) as i32;
        let dy = ((hash >> 21) % ORE_CELL_SIZE as u64) as i32;
        let dz = ((hash >> 42) % ORE_CELL_SIZE as u64) as i32;
        let anchor = cell * ORE_CELL_SIZE + IVec3::new(dx, dy, dz);

        let surface = world_gen.column_height(anchor.x, anchor.z);
        if surface - anchor.y < MIN_STONE_DEPTH {
            return None;
        }
        let decision_hash = splitmix64(hash);
        if !decision_hash.is_multiple_of(divisor(anchor.y)) {
            return None;
        }
        Some((anchor, splitmix64(decision_hash)))
    }

    /// A vein anchored right at a chunk border must still be drawn in full
    /// by whichever chunk owns each of its cells: the lattice cell's anchor
    /// and its walk are a pure function of world position + seed, and
    /// [`VEIN_REACH`] is sized so every chunk whose volume the walk could
    /// reach always finds the anchor's cell in its own scan margin. This is
    /// the ore-vein counterpart of `cross_chunk_tree_shape_is_seam_consistent`;
    /// the assertion is weaker than that test's exact shape match because,
    /// unlike sparse trees, ore veins are dense enough that a neighboring,
    /// independently rolled vein can legitimately claim one of this vein's
    /// cells first (first-writer-wins is intentional, see `place_vein`) —
    /// what must never happen is a cell being left as plain `STONE` because
    /// no chunk's margin reached far enough to draw *any* vein into it.
    #[test]
    fn cross_chunk_ore_vein_is_seam_consistent() {
        let world_gen = WorldGenerator::new(2026);
        let chunk_size = CHUNK_SIZE as i32;
        // How many lattice cells away a competing anchor could still be and
        // have its walk reach one of our vein's cells: its own jitter can
        // land it up to ORE_CELL_SIZE away from the cell's origin, plus the
        // walk's reach, rounded up to whole cells with a little slack.
        let cell_reach = (VEIN_REACH + ORE_CELL_SIZE - 1) / ORE_CELL_SIZE + 1;

        // Find a coal vein cell whose jittered anchor sits within
        // VEIN_REACH of an X chunk border (so its walk can straddle two
        // chunks) and whose touched cells aren't also touched by another
        // nearby coal/iron cell — so the exact-match assertion below is
        // unambiguous, not just "some ore".
        let mut found = None;
        'search: for boundary_chunk in -8..8i32 {
            let border_x = boundary_chunk * chunk_size;
            let cell_lo = (border_x - VEIN_REACH).div_euclid(ORE_CELL_SIZE);
            let cell_hi = (border_x + VEIN_REACH - 1).div_euclid(ORE_CELL_SIZE);
            for cx in cell_lo..=cell_hi {
                for cy in 0..16i32 {
                    for cz in -14..14i32 {
                        let cell = IVec3::new(cx, cy, cz);
                        let Some((anchor, walk_hash)) =
                            resolve_cell_anchor(&world_gen, cell, COAL_VEIN_TAG, |_| {
                                COAL_CELL_DIVISOR
                            })
                        else {
                            continue;
                        };
                        if (anchor.x - border_x).abs() > VEIN_REACH {
                            continue; // jitter landed away from the border after all
                        }
                        let touched = simulate_vein_walk(
                            &world_gen,
                            anchor,
                            walk_hash,
                            COAL_VEIN_MIN_SIZE,
                            COAL_VEIN_MAX_SIZE,
                        );
                        if touched.is_empty() {
                            continue;
                        }
                        // Reject if any other nearby cell's resolved anchor
                        // (coal or iron) also touches one of the same
                        // blocks.
                        let contested = (-cell_reach..=cell_reach).any(|ox| {
                            (-cell_reach..=cell_reach).any(|oy| {
                                (-cell_reach..=cell_reach).any(|oz| {
                                    if ox == 0 && oy == 0 && oz == 0 {
                                        return false;
                                    }
                                    let other_cell = cell + IVec3::new(ox, oy, oz);
                                    let coal_hits = resolve_cell_anchor(
                                        &world_gen,
                                        other_cell,
                                        COAL_VEIN_TAG,
                                        |_| COAL_CELL_DIVISOR,
                                    )
                                    .is_some_and(|(a, h)| {
                                        simulate_vein_walk(
                                            &world_gen,
                                            a,
                                            h,
                                            COAL_VEIN_MIN_SIZE,
                                            COAL_VEIN_MAX_SIZE,
                                        )
                                        .iter()
                                        .any(|t| touched.contains(t))
                                    });
                                    if coal_hits {
                                        return true;
                                    }
                                    resolve_cell_anchor(
                                        &world_gen,
                                        other_cell,
                                        IRON_VEIN_TAG,
                                        iron_divisor,
                                    )
                                    .is_some_and(|(a, h)| {
                                        simulate_vein_walk(
                                            &world_gen,
                                            a,
                                            h,
                                            IRON_VEIN_MIN_SIZE,
                                            IRON_VEIN_MAX_SIZE,
                                        )
                                        .iter()
                                        .any(|t| touched.contains(t))
                                    })
                                })
                            })
                        });
                        if contested {
                            continue;
                        }
                        found = Some((anchor, touched));
                        break 'search;
                    }
                }
            }
        }
        let (anchor, expected) =
            found.expect("expected an uncontested coal vein anchor near a chunk border");

        for world_pos in expected {
            assert_eq!(
                read_world_block(&world_gen, world_pos),
                blocks::COAL_ORE,
                "mismatch at {world_pos:?} (vein anchor {anchor:?})"
            );
        }
    }

    /// Exercise actual generated blocks, not only the noise predicate:
    /// caves must have two-block headroom, routes from the surface that
    /// need no flight or digging, and useful ore exposed along their walls.
    #[test]
    fn caves_have_walkable_routes_from_land_and_expose_ore() {
        use std::collections::{HashMap, HashSet, VecDeque};

        let world_gen = WorldGenerator::new(2026);
        let mut chunks = HashMap::new();
        for x in -2..2 {
            for z in -2..2 {
                for y in 0..2 {
                    let pos = IVec3::new(x, y, z);
                    chunks.insert(pos, world_gen.generate_chunk(pos));
                }
            }
        }
        let read = |pos: IVec3| {
            let (chunk_pos, local) = crate::split_block_pos(pos);
            chunks
                .get(&chunk_pos)
                .map(|chunk| chunk.get(local.as_uvec3()))
        };
        let mut floors = HashSet::new();
        let mut reachable = HashSet::new();
        let mut queue = VecDeque::new();
        let mut exposed_coal = 0;
        let mut exposed_iron = 0;
        let neighbors = [
            IVec3::X,
            IVec3::NEG_X,
            IVec3::Y,
            IVec3::NEG_Y,
            IVec3::Z,
            IVec3::NEG_Z,
        ];
        for x in -63..63 {
            for z in -63..63 {
                let surface = world_gen.column_height(x, z);
                for y in CAVE_FLOOR_Y..62 {
                    let pos = IVec3::new(x, y, z);
                    let block = read(pos).unwrap();
                    if y < surface - DIRT_DEPTH
                        && neighbors
                            .iter()
                            .any(|&d| read(pos + d) == Some(blocks::AIR))
                    {
                        exposed_coal += usize::from(block == blocks::COAL_ORE);
                        exposed_iron += usize::from(block == blocks::IRON_ORE);
                    }
                    if block == blocks::AIR
                        && read(pos + IVec3::Y) == Some(blocks::AIR)
                        && read(pos - IVec3::Y)
                            .is_some_and(|b| b != blocks::AIR && b != blocks::WATER)
                    {
                        floors.insert(pos);
                        if y > surface && surface > SEA_LEVEL + 2 {
                            reachable.insert(pos);
                            queue.push_back(pos);
                        }
                    }
                }
            }
        }
        while let Some(pos) = queue.pop_front() {
            for horizontal in [IVec3::X, IVec3::NEG_X, IVec3::Z, IVec3::NEG_Z] {
                for dy in -1..=1 {
                    let next = pos + horizontal + IVec3::Y * dy;
                    // A one-block ascent also needs clearance above the
                    // starting head while jumping onto the next floor.
                    if dy > 0 && read(pos + IVec3::Y * 2) != Some(blocks::AIR) {
                        continue;
                    }
                    if floors.contains(&next) && reachable.insert(next) {
                        queue.push_back(next);
                    }
                }
            }
        }
        let entrance = reachable
            .iter()
            .filter(|p| {
                p.y <= world_gen.column_height(p.x, p.z)
                    && [IVec3::X, IVec3::NEG_X, IVec3::Z, IVec3::NEG_Z]
                        .iter()
                        .any(|&d| {
                            let other = **p + d;
                            other.y > world_gen.column_height(other.x, other.z)
                                && floors.contains(&other)
                        })
            })
            .min_by_key(|p| (p.x * p.x + p.z * p.z, p.y, p.x, p.z))
            .copied();
        let mut underground: Vec<_> = reachable
            .iter()
            .copied()
            .filter(|p| world_gen.column_height(p.x, p.z) - p.y >= 8)
            .collect();
        underground.sort_by_key(|p| (p.y, p.x, p.z));
        assert!(
            underground.len() >= 64,
            "expected a substantial walkable cave reachable from land; got {} floor cells",
            underground.len()
        );
        assert!(exposed_coal > 0, "caves should expose coal veins");
        assert!(exposed_iron > 0, "caves should expose iron veins");
        println!(
            "seed 2026: {} reachable underground floor cells; entrance {:?}; deepest {:?}; exposed coal {}, iron {}",
            underground.len(),
            entrance,
            underground.first(),
            exposed_coal,
            exposed_iron
        );
    }

    #[test]
    fn caves_preserve_the_world_floor_and_seal_water_at_chunk_borders() {
        let world_gen = WorldGenerator::new(42);
        let mut carved = 0;
        let mut wet_columns = 0;
        for cx in -2..2 {
            for cz in -2..2 {
                let low = world_gen.generate_chunk(IVec3::new(cx, 0, cz));
                let high = world_gen.generate_chunk(IVec3::new(cx, 1, cz));
                for lx in 0..CHUNK_SIZE {
                    for lz in 0..CHUNK_SIZE {
                        let x = cx * CHUNK_SIZE as i32 + lx as i32;
                        let z = cz * CHUNK_SIZE as i32 + lz as i32;
                        let surface = world_gen.column_height(x, z);
                        for y in 0..=surface {
                            let chunk = if y < CHUNK_SIZE as i32 { &low } else { &high };
                            let block = chunk.get(UVec3::new(
                                lx as u32,
                                y.rem_euclid(CHUNK_SIZE as i32) as u32,
                                lz as u32,
                            ));
                            if y < CAVE_FLOOR_Y {
                                assert_ne!(block, blocks::AIR, "floor hole at {x},{y},{z}");
                            }
                            if block == blocks::AIR {
                                carved += 1;
                                // Every carved block must be isolated from
                                // natural ocean water on all six faces.
                                for d in [
                                    IVec3::X,
                                    IVec3::NEG_X,
                                    IVec3::Y,
                                    IVec3::NEG_Y,
                                    IVec3::Z,
                                    IVec3::NEG_Z,
                                ] {
                                    let p = IVec3::new(x, y, z) + d;
                                    assert_ne!(
                                        column_block(world_gen.column_height(p.x, p.z), p.y),
                                        blocks::WATER,
                                        "cave opens into water at {x},{y},{z} toward {d:?}"
                                    );
                                }
                            }
                        }
                        if surface < SEA_LEVEL {
                            wet_columns += 1;
                            for y in surface + 1..=SEA_LEVEL {
                                let chunk = if y < CHUNK_SIZE as i32 { &low } else { &high };
                                assert_eq!(
                                    chunk.get(UVec3::new(
                                        lx as u32,
                                        y.rem_euclid(CHUNK_SIZE as i32) as u32,
                                        lz as u32
                                    )),
                                    blocks::WATER
                                );
                            }
                        }
                    }
                }
            }
        }
        assert!(carved > 1000, "safety checks must include real caves");
        assert!(wet_columns > 0, "safety checks must include an ocean shore");
    }

    #[test]
    fn cave_passages_continue_across_chunk_faces_in_any_generation_order() {
        let world_gen = WorldGenerator::new(2026);
        let positions = [IVec3::new(-1, 0, 0), IVec3::new(0, 0, 0)];
        let first = positions.map(|p| world_gen.generate_chunk(p));
        let reverse = positions
            .into_iter()
            .rev()
            .map(|p| world_gen.generate_chunk(p))
            .collect::<Vec<_>>();
        assert_eq!(all_blocks(&first[0]), all_blocks(&reverse[1]));
        assert_eq!(all_blocks(&first[1]), all_blocks(&reverse[0]));
        let mut open_faces = 0;
        for y in CAVE_FLOOR_Y as u32..CHUNK_SIZE as u32 - 1 {
            for z in 0..CHUNK_SIZE as u32 {
                if first[0].get(UVec3::new(CHUNK_SIZE as u32 - 1, y, z)) == blocks::AIR
                    && first[1].get(UVec3::new(0, y, z)) == blocks::AIR
                    && first[0].get(UVec3::new(CHUNK_SIZE as u32 - 1, y + 1, z)) == blocks::AIR
                    && first[1].get(UVec3::new(0, y + 1, z)) == blocks::AIR
                {
                    open_faces += 1;
                }
            }
        }
        assert!(
            open_faces > 8,
            "expected broad underground passages across X=0"
        );
    }
}
