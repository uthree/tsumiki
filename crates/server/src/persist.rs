//! World persistence: chunks + player state saved to disk (doc/roadmap.md
//! M1, "World persistence"; M2 upgrades the player slot to per-name).
//!
//! Format contract:
//! - `<world_dir>/meta.bin`: postcard-serialized world metadata (format
//!   version, world seed, and the persisted player saves). Format v1 (M1) had
//!   a single global player slot; format v2 (M2) keys player saves by name.
//!   See [`decode_meta`] for how the two are told apart and migrated.
//! - `<world_dir>/regions/r.<rx>.<rz>.bin`: postcard-serialized
//!   `Vec<(IVec3, Chunk)>` holding every chunk (at any Y level) that has ever
//!   been modified and whose region is `(x.div_euclid(REGION_SIZE),
//!   z.div_euclid(REGION_SIZE))`. Unmodified chunks regenerate deterministically
//!   from the seed and are deliberately never written.
//!
//! Writes are atomic enough for a game save: each file is written to a
//! sibling `.tmp` path first, then renamed into place, so a crash mid-write
//! never leaves a half-written file at the real path.

use std::collections::{HashMap, HashSet};
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

const META_FORMAT_VERSION: u32 = 2;

/// Key a migrated v1 (single global slot) player save is filed under in the
/// v2 per-name map.
const LEGACY_PLAYER_NAME: &str = "player";

/// Just enough of `meta.bin`'s layout to read the leading `version` field
/// without committing to a full struct shape. Used by [`decode_meta`] to
/// dispatch to the right versioned struct.
#[derive(Deserialize)]
struct VersionHeader {
    version: u32,
}

/// On-disk contents of `meta.bin`, format v1 (M1): a single global player
/// slot. Kept only so [`decode_meta`] can migrate old saves.
#[derive(Serialize, Deserialize)]
struct WorldMetaV1 {
    version: u32,
    seed: u64,
    player: Option<PlayerSave>,
}

/// On-disk contents of `meta.bin`, format v2 (M2): player saves keyed by
/// name, since real multiplayer distinguishes clients by identity.
#[derive(Serialize, Deserialize)]
struct WorldMetaV2 {
    version: u32,
    seed: u64,
    players: HashMap<String, PlayerSave>,
}

/// Decodes `meta.bin`'s seed and player data, migrating a v1 file
/// transparently.
///
/// postcard has no self-describing format, so there is no generic "peek the
/// tag" operation. Instead we decode only the leading `version` field via
/// [`postcard::take_from_bytes`] (which, unlike `from_bytes`, tolerates and
/// returns the unconsumed remainder rather than erroring on it), then decode
/// the *whole* buffer again from the start using whichever full struct that
/// version corresponds to. This is sound because postcard serializes struct
/// fields in declaration order and both versions declare `version` first, so
/// the header decode and the full decode agree on where `version` lives.
fn decode_meta(bytes: &[u8]) -> io::Result<(u64, HashMap<String, PlayerSave>)> {
    let (header, _) = postcard::take_from_bytes::<VersionHeader>(bytes).map_err(postcard_err)?;
    match header.version {
        2 => {
            let meta: WorldMetaV2 = postcard::from_bytes(bytes).map_err(postcard_err)?;
            Ok((meta.seed, meta.players))
        }
        1 => {
            let meta: WorldMetaV1 = postcard::from_bytes(bytes).map_err(postcard_err)?;
            let mut players = HashMap::new();
            if let Some(player) = meta.player {
                players.insert(LEGACY_PLAYER_NAME.to_string(), player);
            }
            Ok((meta.seed, players))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("meta.bin has unsupported format version {other}"),
        )),
    }
}

/// The chunks and metadata read back from disk at startup.
pub struct LoadedWorld {
    pub seed: u64,
    /// Persisted player saves, keyed by player name.
    pub players: HashMap<String, PlayerSave>,
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
        let (seed, players) = decode_meta(&bytes)?;

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
            seed,
            players,
            chunks,
        }))
    }

    /// Marks a chunk as edited: it must be persisted (and its region
    /// rewritten) on the next save.
    pub fn mark_chunk_dirty(&mut self, chunk_pos: IVec3) {
        self.modified.insert(chunk_pos);
        self.dirty_chunks.insert(chunk_pos);
    }

    /// Marks the player save map as changed since the last save.
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
        players: &HashMap<String, PlayerSave>,
        cache: &HashMap<IVec3, Chunk>,
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

        let meta = WorldMetaV2 {
            version: META_FORMAT_VERSION,
            seed,
            players: players.clone(),
        };
        write_atomic(&meta_path(&dir), &meta)?;

        self.dirty_chunks.clear();
        self.player_dirty = false;
        Ok(())
    }
}
