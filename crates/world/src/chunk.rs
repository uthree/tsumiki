//! Palette-compressed chunk storage (design.md §2).
//!
//! A chunk stores 32³ blocks as a per-chunk palette of occurring block types
//! plus bit-packed indices into that palette. The same representation is used
//! in memory, on disk, and on the network.

use crate::block::BlockId;
use bevy_math::UVec3;
use serde::{Deserialize, Serialize};

/// Chunk edge length in blocks.
pub const CHUNK_SIZE: usize = 32;

/// Number of blocks in a chunk.
pub const CHUNK_VOLUME: usize = CHUNK_SIZE * CHUNK_SIZE * CHUNK_SIZE;

/// Palette-compressed 32³ block storage.
///
/// Representation invariants:
/// - `palette` is non-empty and holds the block types this chunk may contain.
///   Entries are unique. Unused entries may linger after overwrites (no
///   shrinking; that is an accepted inefficiency for now).
/// - `bits == 0` iff the chunk is uniform: `palette.len() == 1` and `data` is
///   empty. Otherwise `bits` is the number of bits per packed index:
///   `ceil(log2(palette.len()))`, minimum 1.
/// - Packed indices are stored little-endian within each `u64`, and an index
///   never straddles a `u64` boundary: each `u64` holds `64 / bits` complete
///   entries (Minecraft 1.16+ style, keeps get/set branch-free).
/// - Block order is linear index `(y * CHUNK_SIZE + z) * CHUNK_SIZE + x`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Chunk {
    palette: Vec<BlockId>,
    bits: u8,
    data: Vec<u64>,
}

/// Returns the number of bits needed to pack `palette_len` distinct indices,
/// minimum 1.
fn bits_for_palette_len(palette_len: usize) -> u8 {
    debug_assert!(palette_len >= 1);
    if palette_len <= 1 {
        return 1;
    }
    let bits = (usize::BITS - (palette_len - 1).leading_zeros()) as u8;
    bits.max(1)
}

/// Number of complete `bits`-wide entries that fit in one `u64` word.
fn entries_per_word(bits: u8) -> usize {
    64 / bits as usize
}

#[inline]
fn local_index(local: UVec3) -> usize {
    debug_assert!(
        local.x < CHUNK_SIZE as u32 && local.y < CHUNK_SIZE as u32 && local.z < CHUNK_SIZE as u32,
        "local coordinate out of chunk bounds: {local:?}"
    );
    ((local.y as usize * CHUNK_SIZE) + local.z as usize) * CHUNK_SIZE + local.x as usize
}

impl Chunk {
    /// Creates a chunk filled entirely with `block` (uniform representation).
    pub fn filled(block: BlockId) -> Self {
        Self {
            palette: vec![block],
            bits: 0,
            data: Vec::new(),
        }
    }

    /// Returns the packed index at linear block index `i`, given `bits`.
    fn packed_index(&self, i: usize, bits: u8) -> u32 {
        let per_word = entries_per_word(bits);
        let word = i / per_word;
        let slot = i % per_word;
        let shift = slot * bits as usize;
        let mask = (1u64 << bits) - 1;
        ((self.data[word] >> shift) & mask) as u32
    }

    /// Writes `value` (must fit in `bits`) into the packed index at linear
    /// block index `i`.
    fn set_packed_index(&mut self, i: usize, bits: u8, value: u32) {
        let per_word = entries_per_word(bits);
        let word = i / per_word;
        let slot = i % per_word;
        let shift = slot * bits as usize;
        let mask = (1u64 << bits) - 1;
        self.data[word] &= !(mask << shift);
        self.data[word] |= (value as u64 & mask) << shift;
    }

    /// Returns the block at `local` (all components must be < 32).
    pub fn get(&self, local: UVec3) -> BlockId {
        let i = local_index(local);
        if self.bits == 0 {
            return self.palette[0];
        }
        let idx = self.packed_index(i, self.bits);
        self.palette[idx as usize]
    }

    /// Sets the block at `local` (all components must be < 32), growing and
    /// repacking the palette when a new block type is introduced.
    pub fn set(&mut self, local: UVec3, block: BlockId) {
        let i = local_index(local);

        // Uniform chunk: fast-path if the write is a no-op, otherwise expand
        // to the general packed representation first.
        if self.bits == 0 {
            if self.palette[0] == block {
                return;
            }
            let bits = bits_for_palette_len(self.palette.len());
            let per_word = entries_per_word(bits);
            let word_count = CHUNK_VOLUME.div_ceil(per_word);
            self.bits = bits;
            self.data = vec![0u64; word_count];
            // All entries already implicitly index 0 (the sole palette entry).
        }

        // Find or insert the palette entry for `block`.
        let new_idx = match self.palette.iter().position(|&b| b == block) {
            Some(idx) => idx,
            None => {
                self.palette.push(block);
                self.palette.len() - 1
            }
        };

        let needed_bits = bits_for_palette_len(self.palette.len());
        if needed_bits > self.bits {
            self.repack(needed_bits);
        }

        self.set_packed_index(i, self.bits, new_idx as u32);
    }

    /// Repacks `data` from the current `bits` width to `new_bits`, widening
    /// the palette index storage without changing any decoded values.
    fn repack(&mut self, new_bits: u8) {
        debug_assert!(new_bits > self.bits);
        let old_bits = self.bits;
        let per_word_new = entries_per_word(new_bits);
        let word_count = CHUNK_VOLUME.div_ceil(per_word_new);
        let mut new_data = vec![0u64; word_count];

        let per_word_old = entries_per_word(old_bits);
        let mask_old = (1u64 << old_bits) - 1;
        let mask_new = (1u64 << new_bits) - 1;

        for i in 0..CHUNK_VOLUME {
            let old_word = i / per_word_old;
            let old_slot = i % per_word_old;
            let old_shift = old_slot * old_bits as usize;
            let value = (self.data[old_word] >> old_shift) & mask_old;

            let new_word = i / per_word_new;
            let new_slot = i % per_word_new;
            let new_shift = new_slot * new_bits as usize;
            new_data[new_word] |= (value & mask_new) << new_shift;
        }

        self.bits = new_bits;
        self.data = new_data;
    }

    /// Returns `Some(block)` if the chunk is stored in uniform form.
    ///
    /// Conservative: a chunk that became uniform through edits without being
    /// re-canonicalized reports `None`. Callers use this only as a fast path.
    pub fn is_uniform(&self) -> Option<BlockId> {
        if self.bits == 0 {
            Some(self.palette[0])
        } else {
            None
        }
    }

    /// Fast path: `true` if the chunk is known to contain only air.
    pub fn is_all_air(&self) -> bool {
        self.is_uniform() == Some(BlockId::AIR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal deterministic LCG so tests need no `rand` dependency.
    struct Lcg(u64);

    impl Lcg {
        fn new(seed: u64) -> Self {
            Self(seed)
        }

        fn next_u64(&mut self) -> u64 {
            // Numerical Recipes LCG constants.
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }

        fn next_range(&mut self, bound: usize) -> usize {
            (self.next_u64() % bound as u64) as usize
        }
    }

    fn local_at(i: usize) -> UVec3 {
        let x = i % CHUNK_SIZE;
        let z = (i / CHUNK_SIZE) % CHUNK_SIZE;
        let y = i / (CHUNK_SIZE * CHUNK_SIZE);
        UVec3::new(x as u32, y as u32, z as u32)
    }

    #[test]
    fn uniform_fast_paths() {
        let chunk = Chunk::filled(BlockId::AIR);
        assert_eq!(chunk.is_uniform(), Some(BlockId::AIR));
        assert!(chunk.is_all_air());

        let stone = BlockId(1);
        let chunk = Chunk::filled(stone);
        assert_eq!(chunk.is_uniform(), Some(stone));
        assert!(!chunk.is_all_air());

        // Every block reads back as the fill value.
        for i in [0, 1, CHUNK_VOLUME / 2, CHUNK_VOLUME - 1] {
            assert_eq!(chunk.get(local_at(i)), stone);
        }
    }

    #[test]
    fn set_same_value_on_uniform_stays_uniform() {
        let mut chunk = Chunk::filled(BlockId::AIR);
        chunk.set(UVec3::new(5, 5, 5), BlockId::AIR);
        assert_eq!(chunk.is_uniform(), Some(BlockId::AIR));
    }

    #[test]
    fn set_breaks_uniformity_and_grows_palette() {
        let mut chunk = Chunk::filled(BlockId::AIR);
        chunk.set(UVec3::new(1, 2, 3), BlockId(1));
        assert_eq!(chunk.is_uniform(), None);
        assert_eq!(chunk.get(UVec3::new(1, 2, 3)), BlockId(1));
        assert_eq!(chunk.get(UVec3::new(0, 0, 0)), BlockId::AIR);
    }

    #[test]
    fn full_chunk_fill_then_overwrite() {
        let mut chunk = Chunk::filled(BlockId::AIR);
        for i in 0..CHUNK_VOLUME {
            chunk.set(local_at(i), BlockId(1));
        }
        for i in 0..CHUNK_VOLUME {
            assert_eq!(chunk.get(local_at(i)), BlockId(1));
        }
        // Overwrite everything with a different block.
        for i in 0..CHUNK_VOLUME {
            chunk.set(local_at(i), BlockId(2));
        }
        for i in 0..CHUNK_VOLUME {
            assert_eq!(chunk.get(local_at(i)), BlockId(2));
        }
    }

    #[test]
    fn roundtrip_against_naive_model() {
        let mut chunk = Chunk::filled(BlockId::AIR);
        let mut model = vec![0u16; CHUNK_VOLUME];
        let mut rng = Lcg::new(0xC0FFEE_u64);

        // Enough distinct block ids to push the palette past 2, 4 and 16
        // entries during the run.
        let block_pool: Vec<u16> = (0..20).collect();

        for _ in 0..20_000 {
            let idx = rng.next_range(CHUNK_VOLUME);
            let block_id = block_pool[rng.next_range(block_pool.len())];
            let local = local_at(idx);

            chunk.set(local, BlockId(block_id));
            model[idx] = block_id;

            // Periodically verify the whole chunk against the model, not
            // just the last write, to catch repack corruption.
            if rng.next_range(200) == 0 {
                for i in 0..CHUNK_VOLUME {
                    assert_eq!(
                        chunk.get(local_at(i)).0,
                        model[i],
                        "mismatch at linear index {i}"
                    );
                }
            }
        }

        for i in 0..CHUNK_VOLUME {
            assert_eq!(
                chunk.get(local_at(i)).0,
                model[i],
                "final mismatch at linear index {i}"
            );
        }
    }

    #[test]
    fn palette_growth_thresholds() {
        // Exercise bits width transitions explicitly: 1 -> 2 -> 3 -> 5 bits.
        let mut chunk = Chunk::filled(BlockId::AIR);
        let ids: Vec<BlockId> = (0..20).map(BlockId).collect();

        for (n, &id) in ids.iter().enumerate() {
            chunk.set(local_at(n), id);
            for (k, &check_id) in ids.iter().enumerate().take(n + 1) {
                assert_eq!(chunk.get(local_at(k)), check_id);
            }
        }
    }

    #[test]
    fn bits_for_palette_len_matches_ceil_log2() {
        assert_eq!(bits_for_palette_len(1), 1);
        assert_eq!(bits_for_palette_len(2), 1);
        assert_eq!(bits_for_palette_len(3), 2);
        assert_eq!(bits_for_palette_len(4), 2);
        assert_eq!(bits_for_palette_len(5), 3);
        assert_eq!(bits_for_palette_len(16), 4);
        assert_eq!(bits_for_palette_len(17), 5);
    }
}
