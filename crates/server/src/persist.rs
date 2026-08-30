//! World persistence: chunks + player state saved to disk (doc/roadmap.md
//! M1, "World persistence").
//!
//! Format contract:
//! - `<world_dir>/meta.bin`: postcard-serialized [`WorldMeta`] (format
//!   version, world seed, and the single M1 player slot).
//! - `<world_dir>/regions/r.<rx>.<rz>.bin`: postcard-serialized
//!   `Vec<(IVec3, Chunk)>` holding every chunk (at any Y level) that has ever
//!   been modified and whose region is `(x.div_euclid(REGION_SIZE),
//!   z.div_euclid(REGION_SIZE))`. Unmodified chunks regenerate deterministically
//!   from the seed and are deliberately never written.
//!
//! Writes are atomic enough for a game save: each file is written to a
//! sibling `.tmp` path first, then renamed into place, so a crash mid-write
//! never leaves a half-written file at the real path.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use bevy::prelude::Resource;
use bevy_math::IVec3;
use serde::{Deserialize, Serialize};

use tsumiki_protocol::PlayerSave;
use tsumiki_world::Chunk;

/// Chunk columns per region edge; a region file covers all Y levels of an
/// 8x8 chunk-column footprint.
pub const REGION_SIZE: i32 = 8;

const META_FORMAT_VERSION: u32 = 1;

/// On-disk contents of `meta.bin`.
#[derive(Serialize, Deserialize)]
struct WorldMeta {
    version: u32,
    seed: u64,
    /// Single-slot player save. M1 has one player; per-name keying for
    /// multiple persisted players is an M2 concern (real multiplayer
    /// identities).
    player: Option<PlayerSave>,
}

/// The chunks and metadata read back from disk at startup.
pub struct LoadedWorld {
    pub seed: u64,
    pub player: Option<PlayerSave>,
    pub chunks: Vec<(IVec3, Chunk)>,
}

/// Region coordinate a chunk position falls into.
fn region_of(chunk_pos: IVec3) -> (i32, i32) {
    (
        chunk_pos.x.div_euclid(REGION_SIZE),
        chunk_pos.z.div_euclid(REGION_SIZE),
    )
}

fn meta_path(world_dir: &Path) -> PathBuf {
    world_dir.join("meta.bin")
}

fn regions_dir(world_dir: &Path) -> PathBuf {
    world_dir.join("regions")
}

fn region_path(world_dir: &Path, region: (i32, i32)) -> PathBuf {
    regions_dir(world_dir).join(format!("r.{}.{}.bin", region.0, region.1))
}

fn postcard_err(e: postcard::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

/// Serializes `value` and writes it to `path` atomically: the bytes land in
/// a sibling `.tmp` file first, which is then renamed over `path`. A crash or
/// power loss between the write and the rename leaves the original file (or
/// no file) intact, never a truncated one.
fn write_atomic<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = postcard::to_allocvec(value).map_err(postcard_err)?;
    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, &bytes)?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Server-side persistence bookkeeping. Owns no chunk data itself (that
/// lives in the server's `ChunkCache`); tracks which chunks are persisted and
/// which have changed since the last save, and performs the actual disk I/O
/// on request.
#[derive(Resource)]
pub struct Persistence {
    world_dir: Option<PathBuf>,
    autosave_interval_secs: f64,
    /// Seconds accumulated since the last autosave check.
    accumulator: f64,
    /// Every chunk position that is persisted: loaded from disk at startup,
    /// or edited this session. Only these are ever written to region files;
    /// everything else regenerates deterministically from the seed.
    modified: HashSet<IVec3>,
    /// Chunk positions edited since the last save. Drives which region files
    /// get rewritten; cleared after a successful save.
    dirty_chunks: HashSet<IVec3>,
    /// Set when the player save changes since the last save.
    player_dirty: bool,
}

impl Persistence {
    pub fn new(world_dir: Option<PathBuf>, autosave_interval_secs: f64) -> Self {
        Self {
            world_dir,
            autosave_interval_secs,
            accumulator: 0.0,
            modified: HashSet::new(),
            dirty_chunks: HashSet::new(),
            player_dirty: false,
        }
    }

    /// Loads persisted state from `world_dir`, if any. Returns `Ok(None)`
    /// for an ephemeral server (`world_dir` is `None`) or a fresh world
    /// directory (no `meta.bin` yet). Every loaded chunk is recorded as
    /// `modified` so later autosaves keep persisting it.
    pub fn load(&mut self) -> io::Result<Option<LoadedWorld>> {
        let Some(dir) = self.world_dir.clone() else {
            return Ok(None);
        };
        let meta_file = meta_path(&dir);
        if !meta_file.exists() {
            return Ok(None);
        }

        let bytes = fs::read(&meta_file)?;
        let meta: WorldMeta = postcard::from_bytes(&bytes).map_err(postcard_err)?;

        let mut chunks = Vec::new();
        let regions = regions_dir(&dir);
        if regions.is_dir() {
            for entry in fs::read_dir(&regions)? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let bytes = fs::read(entry.path())?;
                let region_chunks: Vec<(IVec3, Chunk)> =
                    postcard::from_bytes(&bytes).map_err(postcard_err)?;
                for (pos, chunk) in region_chunks {
                    self.modified.insert(pos);
                    chunks.push((pos, chunk));
                }
            }
        }

        Ok(Some(LoadedWorld {
            seed: meta.seed,
            player: meta.player,
            chunks,
        }))
    }

    /// Marks a chunk as edited: it must be persisted (and its region
    /// rewritten) on the next save.
    pub fn mark_chunk_dirty(&mut self, chunk_pos: IVec3) {
        self.modified.insert(chunk_pos);
        self.dirty_chunks.insert(chunk_pos);
    }

    /// Marks the player save as changed since the last save.
    pub fn mark_player_dirty(&mut self) {
        self.player_dirty = true;
    }

    /// `true` if a save would write anything new.
    pub fn has_dirty(&self) -> bool {
        !self.dirty_chunks.is_empty() || self.player_dirty
    }

    /// Advances the autosave clock by `dt` seconds. Returns `true` at most
    /// once per `autosave_interval_secs` crossed, regardless of whether
    /// anything is actually dirty (callers check [`Self::has_dirty`]
    /// themselves so a quiet world doesn't touch disk).
    pub fn autosave_due(&mut self, dt: f64) -> bool {
        self.accumulator += dt;
        if self.accumulator >= self.autosave_interval_secs {
            self.accumulator -= self.autosave_interval_secs;
            true
        } else {
            false
        }
    }

    /// Writes every region file touched by `dirty_chunks` (in full, from
    /// `cache`) plus `meta.bin`, then clears dirty tracking. A no-op for an
    /// ephemeral server. Used both for periodic autosave and for the final
    /// save on `Goodbye`.
    pub fn save(
        &mut self,
        seed: u64,
        player: Option<PlayerSave>,
        cache: &std::collections::HashMap<IVec3, Chunk>,
    ) -> io::Result<()> {
        let Some(dir) = self.world_dir.clone() else {
            return Ok(());
        };

        let mut affected_regions: HashSet<(i32, i32)> = HashSet::new();
        for &pos in &self.dirty_chunks {
            affected_regions.insert(region_of(pos));
        }

        for region in affected_regions {
            let region_chunks: Vec<(IVec3, Chunk)> = self
                .modified
                .iter()
                .filter(|&&pos| region_of(pos) == region)
                .filter_map(|&pos| cache.get(&pos).map(|chunk| (pos, chunk.clone())))
                .collect();
            write_atomic(&region_path(&dir, region), &region_chunks)?;
        }

        let meta = WorldMeta {
            version: META_FORMAT_VERSION,
            seed,
            player,
        };
        write_atomic(&meta_path(&dir), &meta)?;

        self.dirty_chunks.clear();
        self.player_dirty = false;
        Ok(())
    }
}
