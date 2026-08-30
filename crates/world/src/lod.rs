//! LOD pyramid (design.md §3).
//!
//! A level-L LOD chunk reuses [`Chunk`] (32³ palette-compressed cells), but
//! each cell stands for a `2^L`-block cube, so the chunk spans `32 * 2^L`
//! blocks per axis. Level 0 is the real world; levels `1..=MAX_LOD` are
//! generated server-side and streamed to clients by distance band.
//!
//! Two sources combine into one LOD chunk:
//! - [`WorldGenerator::generate_lod_chunk`]: pristine terrain, sampled from
//!   the height field at cell resolution — O(cells), never touching level-0
//!   chunks.
//! - [`overlay_downsampled`]: projects one real (level-0) chunk into the
//!   covering LOD chunk's cells, so generated-and-possibly-edited regions
//!   (player builds!) appear in the distance.
//!
//! Cell rule, used by both sources' downsampling semantics: a cell is air
//! when more than half of the blocks it covers are air; otherwise it takes
//! the most frequent non-air block (ties broken deterministically, e.g.
//! lowest block id). Water counts as non-air.

use crate::block::{BlockId, blocks};
use crate::chunk::Chunk;
use crate::worldgen::{SEA_LEVEL, WorldGenerator, surface_block};
use bevy_math::{IVec3, UVec3};

/// Deepest LOD level. Bands double per level: with the default view distance
/// (8 chunks = 256 blocks), level 3 puts the horizon at ~2048 blocks.
pub const MAX_LOD: u8 = 3;

/// Edge length of one cell at `level`, in blocks (`2^level`).
#[inline]
pub fn cell_size(level: u8) -> i32 {
    1 << level
}

/// Edge length of one level-`level` chunk, in blocks (`32 * 2^level`).
#[inline]
pub fn chunk_span(level: u8) -> i32 {
    crate::CHUNK_SIZE as i32 * cell_size(level)
}

/// Number of level-`level` chunks stacked vertically to cover the world
/// height (at least 1; higher levels cover the whole height in one chunk,
/// leaving upper cells as air).
pub fn world_height_lod_chunks(level: u8) -> i32 {
    (crate::WORLD_HEIGHT_CHUNKS + (1 << level) - 1) >> level.min(31)
}

impl WorldGenerator {
    /// Generates the pristine-terrain LOD chunk at `pos` (level-`level`
    /// chunk coordinates, `level` in `1..=MAX_LOD`,
    /// `pos.y` in `0..world_height_lod_chunks(level)`).
    ///
    /// Samples the height field once per cell column (cell-center) and fills
    /// the column at cell granularity with the same layering rules as
    /// level-0 terrain (stone body, grass or near-sea-level sand surface,
    /// water up to sea level). No trees. Deterministic: same seed + level +
    /// pos ⇒ identical chunk.
    ///
    /// Convention: a cell is addressed by its *bottom* world-space Y
    /// (`cell_y = (pos.y * CHUNK_SIZE + local_y) * cell_size(level)`), and is
    /// compared against the sampled `surface` the same way level-0's
    /// `column_block` compares a block's world Y: a cell whose bottom lies at
    /// or below `surface` is solid; the *topmost* solid cell (the one whose
    /// `cell_size`-tall span straddles `surface`) gets the surface block
    /// (grass/sand), every solid cell below it is stone. Cells above the
    /// surface are water up to `SEA_LEVEL`, air beyond it.
    ///
    /// Simplification: unlike level 0, there is no separate dirt band — at
    /// cell sizes of 2 blocks or more a few dirt blocks would not survive
    /// downsampling into a visible layer anyway, so LOD terrain is simply
    /// stone capped by the surface block.
    pub fn generate_lod_chunk(&self, level: u8, pos: IVec3) -> Chunk {
        debug_assert!((1..=MAX_LOD).contains(&level));
        let size = cell_size(level);
        let cells = crate::CHUNK_SIZE as i32;
        // Origin of this LOD chunk, in cell coordinates (cells are the unit
        // of the LOD grid, just as blocks are the unit of a level-0 chunk).
        let base_cell = pos * cells;

        let mut chunk = Chunk::filled(blocks::AIR);

        // Height sampled once per cell column, at the cell's center world
        // X/Z, from the same noise field level-0 terrain uses.
        let mut surfaces = [[0i32; crate::CHUNK_SIZE]; crate::CHUNK_SIZE];
        for lx in 0..crate::CHUNK_SIZE {
            for lz in 0..crate::CHUNK_SIZE {
                let cx = base_cell.x + lx as i32;
                let cz = base_cell.z + lz as i32;
                let center_x = cx * size + size / 2;
                let center_z = cz * size + size / 2;
                surfaces[lx][lz] = self.column_height(center_x, center_z);
            }
        }

        for lx in 0..crate::CHUNK_SIZE {
            for lz in 0..crate::CHUNK_SIZE {
                let surface = surfaces[lx][lz];
                for ly in 0..crate::CHUNK_SIZE {
                    let cell_y = (base_cell.y + ly as i32) * size;
                    let block = if cell_y > surface {
                        if cell_y <= SEA_LEVEL {
                            blocks::WATER
                        } else {
                            blocks::AIR
                        }
                    } else if cell_y + size > surface {
                        // This cell's block span straddles the sampled
                        // surface height: it's the exposed cell.
                        surface_block(surface)
                    } else {
                        blocks::STONE
                    };
                    if !block.is_air() {
                        chunk.set(UVec3::new(lx as u32, ly as u32, lz as u32), block);
                    }
                }
            }
        }

        chunk
    }
}

/// Projects the real (level-0) chunk `source` at `source_pos` into the cells
/// of `lod` (a level-`level` chunk at `lod_pos`), applying the cell rule
/// above to each covered cell. `source_pos` must lie inside `lod_pos`'s
/// footprint (debug_assert). Cells not covered by `source` are untouched.
pub fn overlay_downsampled(
    lod: &mut Chunk,
    level: u8,
    lod_pos: IVec3,
    source: &Chunk,
    source_pos: IVec3,
) {
    debug_assert_eq!(
        lod_pos_of_chunk(level, source_pos),
        lod_pos,
        "source_pos {source_pos:?} is not inside lod_pos {lod_pos:?}'s footprint at level {level}"
    );

    let chunks_per_axis = 1i32 << level;
    // One level-0 chunk covers (32 >> level) cells per axis.
    let cells_per_chunk = crate::CHUNK_SIZE as i32 >> level;
    let size = cell_size(level);

    // Where this source chunk's cells land within the LOD chunk's 32^3 cell
    // grid.
    let chunk_offset = source_pos - lod_pos * chunks_per_axis;
    debug_assert!(
        (0..chunks_per_axis).contains(&chunk_offset.x)
            && (0..chunks_per_axis).contains(&chunk_offset.y)
            && (0..chunks_per_axis).contains(&chunk_offset.z)
    );
    let cell_base = chunk_offset * cells_per_chunk;

    let total = (size * size * size) as usize;

    for cx in 0..cells_per_chunk {
        for cy in 0..cells_per_chunk {
            for cz in 0..cells_per_chunk {
                let mut air_count = 0usize;
                // Small linear frequency table: a cell covers at most 8^3 =
                // 512 blocks (MAX_LOD), and the catalog is tiny, so a Vec
                // scan beats a HashMap here.
                let mut freq: Vec<(BlockId, usize)> = Vec::new();

                for i in 0..size {
                    for j in 0..size {
                        for k in 0..size {
                            let local = UVec3::new(
                                (cx * size + i) as u32,
                                (cy * size + j) as u32,
                                (cz * size + k) as u32,
                            );
                            let b = source.get(local);
                            if b.is_air() {
                                air_count += 1;
                            } else {
                                match freq.iter_mut().find(|(id, _)| *id == b) {
                                    Some(entry) => entry.1 += 1,
                                    None => freq.push((b, 1)),
                                }
                            }
                        }
                    }
                }

                let lod_local = UVec3::new(
                    (cell_base.x + cx) as u32,
                    (cell_base.y + cy) as u32,
                    (cell_base.z + cz) as u32,
                );

                let winner = if air_count * 2 > total {
                    BlockId::AIR
                } else {
                    // Most frequent non-air block; ties go to the lowest
                    // block id.
                    let mut best: Option<(BlockId, usize)> = None;
                    for &(id, count) in &freq {
                        let better = match best {
                            None => true,
                            Some((best_id, best_count)) => {
                                count > best_count || (count == best_count && id.0 < best_id.0)
                            }
                        };
                        if better {
                            best = Some((id, count));
                        }
                    }
                    best.map(|(id, _)| id).unwrap_or(BlockId::AIR)
                };

                lod.set(lod_local, winner);
            }
        }
    }
}

/// The level-`level` LOD chunk position whose footprint contains the level-0
/// chunk position `chunk_pos`.
#[inline]
pub fn lod_pos_of_chunk(level: u8, chunk_pos: IVec3) -> IVec3 {
    IVec3::new(
        chunk_pos.x.div_euclid(1 << level),
        chunk_pos.y.div_euclid(1 << level),
        chunk_pos.z.div_euclid(1 << level),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worldgen::SEA_LEVEL;
    use crate::{CHUNK_SIZE, WORLD_HEIGHT_CHUNKS};

    fn all_cells(chunk: &Chunk) -> Vec<BlockId> {
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
    fn generate_lod_chunk_is_deterministic() {
        let world_gen = WorldGenerator::new(1234);
        let a = world_gen.generate_lod_chunk(1, IVec3::new(2, 0, -3));
        let b = world_gen.generate_lod_chunk(1, IVec3::new(2, 0, -3));
        assert_eq!(all_cells(&a), all_cells(&b));
    }

    #[test]
    fn world_height_lod_chunks_matches_expected_levels() {
        assert_eq!(WORLD_HEIGHT_CHUNKS, 4);
        assert_eq!(world_height_lod_chunks(0), 4);
        assert_eq!(world_height_lod_chunks(1), 2);
        assert_eq!(world_height_lod_chunks(2), 1);
        assert_eq!(world_height_lod_chunks(3), 1);
    }

    /// At LOD1, the topmost solid cell of a column should sit within one
    /// cell of the true (un-downsampled) height-field surface.
    #[test]
    fn lod1_surface_height_matches_height_field_within_one_cell() {
        let world_gen = WorldGenerator::new(77);
        let level = 1;
        let size = cell_size(level);
        // Level-1 chunk (0, 0, 0) covers world Y in 0..64, which safely
        // contains the clamped [1, WORLD_HEIGHT_BLOCKS - 9] surface range.
        let chunk = world_gen.generate_lod_chunk(level, IVec3::new(0, 0, 0));

        for &(lx, lz) in &[(0usize, 0usize), (5, 9), (17, 3), (31, 31), (12, 20)] {
            let center_x = lx as i32 * size + size / 2;
            let center_z = lz as i32 * size + size / 2;
            let true_surface = world_gen.column_height(center_x, center_z);

            // Find the topmost non-air cell in this column.
            let mut top_solid_cell_y = None;
            for ly in (0..CHUNK_SIZE).rev() {
                let b = chunk.get(UVec3::new(lx as u32, ly as u32, lz as u32));
                if !b.is_air() && b != blocks::WATER {
                    top_solid_cell_y = Some(ly as i32 * size);
                    break;
                }
            }

            if let Some(cell_y) = top_solid_cell_y {
                let diff = (cell_y - true_surface).abs();
                assert!(
                    diff <= size,
                    "column ({lx},{lz}): topmost solid cell y {cell_y} too far from \
                     true surface {true_surface} (diff {diff}, cell size {size})"
                );
            }
            // If no solid cell was found the column is fully underwater at
            // this level, which is covered by the sea-level test below.
        }
    }

    #[test]
    fn sea_level_columns_produce_water_at_all_levels() {
        let world_gen = WorldGenerator::new(42);

        // Find a column clearly underwater (mirrors the equivalent worldgen
        // test), so we know a chunk covering it must contain water cells.
        let mut underwater_x = None;
        for x in -300..300 {
            if world_gen.column_height(x, 0) < SEA_LEVEL - 2 {
                underwater_x = Some(x);
                break;
            }
        }
        let ux = underwater_x.expect("expected an underwater column in scan range");

        for level in 1..=MAX_LOD {
            let size = cell_size(level);
            let span = chunk_span(level);
            let cx = ux.div_euclid(span);
            let cy = SEA_LEVEL.div_euclid(span);
            let pos = IVec3::new(cx, cy.max(0), 0);
            let chunk = world_gen.generate_lod_chunk(level, pos);

            let mut found_water = false;
            for y in 0..CHUNK_SIZE {
                for z in 0..CHUNK_SIZE {
                    for x in 0..CHUNK_SIZE {
                        if chunk.get(UVec3::new(x as u32, y as u32, z as u32)) == blocks::WATER {
                            found_water = true;
                        }
                    }
                }
            }
            let _ = size;
            assert!(
                found_water,
                "expected at least one water cell at level {level} near an underwater column"
            );
        }
    }

    #[test]
    fn lod_pos_of_chunk_handles_negative_coords() {
        assert_eq!(
            lod_pos_of_chunk(1, IVec3::new(-1, -1, -1)),
            IVec3::new(-1, -1, -1)
        );
        assert_eq!(
            lod_pos_of_chunk(1, IVec3::new(-2, -2, -2)),
            IVec3::new(-1, -1, -1)
        );
        assert_eq!(
            lod_pos_of_chunk(1, IVec3::new(-3, 0, 1)),
            IVec3::new(-2, 0, 0)
        );
        assert_eq!(
            lod_pos_of_chunk(2, IVec3::new(-5, -1, 3)),
            IVec3::new(-2, -1, 0)
        );
    }

    #[test]
    fn overlay_downsampled_applies_cell_rule_exactly() {
        let mut source = Chunk::filled(BlockId::AIR);

        // Cell (0,0,0) -> source blocks x0..2,y0..2,z0..2: all stone.
        for x in 0..2u32 {
            for y in 0..2u32 {
                for z in 0..2u32 {
                    source.set(UVec3::new(x, y, z), blocks::STONE);
                }
            }
        }

        // Cell (0,0,1) -> x0..2,y0..2,z2..4: 4 stone / 4 air tie -> stone.
        for (x, y, z) in [(0, 0, 2), (1, 0, 2), (0, 1, 2), (1, 1, 2)] {
            source.set(UVec3::new(x, y, z), blocks::STONE);
        }

        // Cell (1,0,0) -> x2..4,y0..2,z0..2: 3 stone / 5 air -> air.
        for (x, y, z) in [(2, 0, 0), (3, 0, 0), (2, 1, 0)] {
            source.set(UVec3::new(x, y, z), blocks::STONE);
        }

        // Cell (1,0,1) -> x2..4,y0..2,z2..4: 4 dirt / 4 sand, no air ->
        // non-air tie, lowest id (dirt) wins.
        for (x, y, z) in [(2, 0, 2), (3, 0, 2)] {
            source.set(UVec3::new(x, y, z), blocks::DIRT);
        }
        for (x, y, z) in [(2, 1, 2), (3, 1, 2)] {
            source.set(UVec3::new(x, y, z), blocks::DIRT);
        }
        for (x, y, z) in [(2, 0, 3), (3, 0, 3)] {
            source.set(UVec3::new(x, y, z), blocks::SAND);
        }
        for (x, y, z) in [(2, 1, 3), (3, 1, 3)] {
            source.set(UVec3::new(x, y, z), blocks::SAND);
        }

        let mut lod = Chunk::filled(BlockId::AIR);
        overlay_downsampled(&mut lod, 1, IVec3::ZERO, &source, IVec3::ZERO);

        assert_eq!(lod.get(UVec3::new(0, 0, 0)), blocks::STONE);
        assert_eq!(
            lod.get(UVec3::new(0, 0, 1)),
            blocks::STONE,
            "4-air/4-stone tie must resolve to solid stone"
        );
        assert_eq!(lod.get(UVec3::new(1, 0, 0)), BlockId::AIR);
        assert_eq!(
            lod.get(UVec3::new(1, 0, 1)),
            blocks::DIRT,
            "non-air tie must resolve to the lowest block id"
        );
    }

    #[test]
    fn overlay_downsampled_leaves_uncovered_cells_untouched() {
        // Level 2: one source chunk covers (32 >> 2) = 8 cells per axis,
        // i.e. a strict sub-region of the 32^3 LOD cell grid.
        let level = 2;
        let source = Chunk::filled(blocks::STONE);

        let mut lod = Chunk::filled(BlockId::AIR);
        // Sentinel far outside the source chunk's footprint (0..8 per axis).
        lod.set(UVec3::new(20, 10, 5), blocks::LOG);

        overlay_downsampled(&mut lod, level, IVec3::ZERO, &source, IVec3::ZERO);

        assert_eq!(
            lod.get(UVec3::new(20, 10, 5)),
            blocks::LOG,
            "cell outside the source chunk's footprint must stay untouched"
        );
        // Sanity: a cell inside the covered footprint did get overwritten.
        assert_eq!(lod.get(UVec3::new(2, 2, 2)), blocks::STONE);
    }

    #[test]
    #[should_panic]
    fn overlay_downsampled_rejects_mismatched_footprint() {
        let source = Chunk::filled(blocks::STONE);
        let mut lod = Chunk::filled(BlockId::AIR);
        // source_pos (4,0,0) at level 1 belongs to lod_pos (2,0,0), not
        // (0,0,0): the debug_assert must catch this.
        overlay_downsampled(&mut lod, 1, IVec3::ZERO, &source, IVec3::new(4, 0, 0));
    }
}
